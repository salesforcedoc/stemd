//! Reading a deck's medium over ONC RPC / NFSv2.
//!
//! A CDJ exports its USB and SD media to the whole link-local subnet, so a host
//! that has never announced itself can read `export.pdb` and the audio files
//! behind it. Only MOUNT and NFS are needed: the portmapper tells us which UDP
//! ports those programs are on, MOUNT gives us a handle for the slot's export,
//! LOOKUP walks the path, READ pulls the bytes.
//!
//! A CDJ's READ size is 8192 bytes, the NFSv2 maximum. That relies on IP
//! fragmentation; if a link drops fragments the first chunk times out, and the
//! client falls back to 1280-byte reads for the rest of the transfer.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use super::packets::Slot;

const PORTMAP: u32 = 100_000;
const MOUNT: u32 = 100_005;
const NFS: u32 = 100_003;

const PORTMAP_VERSION: u32 = 2;
const MOUNT_VERSION: u32 = 1;
const NFS_VERSION: u32 = 2;

const PORTMAP_GETPORT: u32 = 3;
const MOUNT_MNT: u32 = 1;
const MOUNT_UMNT: u32 = 3;
const NFS_LOOKUP: u32 = 4;
const NFS_READ: u32 = 6;

const AUTH_UNIX: u32 = 1;
const AUTH_NULL: u32 = 0;
const IPPROTO_UDP: u32 = 17;

/// NFSv2's maximum READ, and what a real CDJ asks for.
const CDJ_READ_SIZE: usize = 8192;
/// The largest read that fits one Ethernet frame.
const UNFRAGMENTED_READ_SIZE: usize = 1280;

/// How long one RPC attempt waits before it is retried.
const RPC_TIMEOUT: Duration = Duration::from_secs(2);
/// Attempts per RPC. A lost UDP datagram is ordinary; a dead deck must not hang.
const RPC_ATTEMPTS: u32 = 5;

/// The file handle size every CDJ serves.
const HANDLE: usize = 32;

/// `NFSERR_NOENT`, which is how a missing `export.pdb` path is reported.
pub const NFS_NOENT: u32 = 2;

/// An NFS procedure returned a non-zero status.
#[derive(Debug, thiserror::Error)]
#[error("the deck answered NFS status {status} during {what}")]
pub struct NfsStatus {
    pub status: u32,
    pub what: &'static str,
}

/// True when an error is the deck saying a path does not exist.
pub fn is_noent(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<NfsStatus>()
        .is_some_and(|status| status.status == NFS_NOENT)
}

/// A mounted medium.
pub struct Mount {
    handle: [u8; HANDLE],
    export: String,
}

/// A file on the deck.
pub struct RemoteFile {
    handle: [u8; HANDLE],
    size: u64,
    path: String,
}

impl RemoteFile {
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Override the size from a source other than LOOKUP. Some media report it
    /// only in the PDB, where the row carries the file size.
    pub fn set_size(&mut self, size: u64) {
        self.size = size;
    }
}

/// A client for one deck.
pub struct NfsClient {
    socket: UdpSocket,
    server: Ipv4Addr,
    mount_port: u16,
    nfs_port: u16,
    xid: u32,
    read_size: usize,
}

impl NfsClient {
    /// Discover the deck's mount and NFS ports and connect to it.
    pub fn connect(server: Ipv4Addr) -> Result<Self> {
        let socket = UdpSocket::bind(("0.0.0.0", 0)).context("opening a UDP socket for NFS")?;
        socket
            .set_read_timeout(Some(RPC_TIMEOUT))
            .context("setting the NFS socket timeout")?;
        let mut client = Self {
            socket,
            server,
            mount_port: 0,
            nfs_port: 0,
            xid: 0x1000,
            read_size: CDJ_READ_SIZE,
        };
        client.mount_port = client.getport(MOUNT, MOUNT_VERSION)?;
        client.nfs_port = client.getport(NFS, NFS_VERSION)?;
        Ok(client)
    }

    /// Connect without asking the portmapper. Used by tests, and by anything
    /// that already knows where the two programs live.
    #[cfg(test)]
    pub fn connect_with_ports(server: Ipv4Addr, mount_port: u16, nfs_port: u16) -> Result<Self> {
        let socket = UdpSocket::bind(("0.0.0.0", 0)).context("opening a UDP socket for NFS")?;
        socket
            .set_read_timeout(Some(RPC_TIMEOUT))
            .context("setting the NFS socket timeout")?;
        Ok(Self {
            socket,
            server,
            mount_port,
            nfs_port,
            xid: 0x1000,
            read_size: CDJ_READ_SIZE,
        })
    }

    /// Mount one slot's export (`/C/` for USB, `/B/` for SD).
    pub fn mount_slot(&mut self, slot: Slot) -> Result<Mount> {
        let export = slot
            .export()
            .with_context(|| format!("slot {} holds no medium", slot.name()))?;
        let mut args = Vec::new();
        put_pascal_utf16(&mut args, export);
        let reply = self.call(self.mount_port, MOUNT, MOUNT_VERSION, MOUNT_MNT, &args)?;
        let mut reader = Reader::new(&reply);
        let status = reader.u32()?;
        if status != 0 {
            return Err(NfsStatus {
                status,
                what: "MOUNT",
            }
            .into());
        }
        Ok(Mount {
            handle: reader.handle()?,
            export: export.to_owned(),
        })
    }

    /// Resolve a path relative to the mounted export and read its size from the
    /// final LOOKUP. The path a PDB row carries looks absolute but is relative to
    /// the export root, so `/Contents/...` is correct from `/C/`.
    pub fn open(&mut self, mounted: &Mount, path: &str) -> Result<RemoteFile> {
        let mut handle = mounted.handle;
        let mut size = 0;
        for part in path.split('/').filter(|part| !part.is_empty()) {
            let mut args = Vec::new();
            args.extend_from_slice(&handle);
            put_pascal_utf16(&mut args, part);
            let reply = self.call(self.nfs_port, NFS, NFS_VERSION, NFS_LOOKUP, &args)?;
            let mut reader = Reader::new(&reply);
            let status = reader.u32()?;
            if status != 0 {
                return Err(NfsStatus {
                    status,
                    what: "LOOKUP",
                }
                .into());
            }
            handle = reader.handle()?;
            size = reader.fattr()?.size as u64;
        }
        Ok(RemoteFile {
            handle,
            size,
            path: path.to_owned(),
        })
    }

    /// Open and read a whole file. `export.pdb` is about 1 MB.
    pub fn read_path(&mut self, mounted: &Mount, path: &str) -> Result<Vec<u8>> {
        let file = self.open(mounted, path)?;
        self.read_file(&file)
    }

    /// Read a whole file into memory.
    pub fn read_file(&mut self, file: &RemoteFile) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(usize::try_from(file.size).unwrap_or(0));
        let mut offset = 0u64;
        while offset < file.size {
            let chunk = self.read_chunk(file, offset)?;
            if chunk.is_empty() {
                bail!("the deck returned no bytes for {} at {offset}", file.path);
            }
            out.extend_from_slice(&chunk);
            offset += chunk.len() as u64;
        }
        Ok(out)
    }

    /// Stream a file to disk, reporting contiguous bytes written. A short read is
    /// a normal event and is simply asked for again.
    pub fn download_to(
        &mut self,
        file: &RemoteFile,
        destination: &Path,
        mut progress: impl FnMut(u64, u64),
    ) -> Result<()> {
        let handle = File::create(destination)
            .with_context(|| format!("creating {}", destination.display()))?;
        let mut out = BufWriter::new(handle);
        let mut offset = 0u64;
        while offset < file.size {
            let chunk = self.read_chunk(file, offset)?;
            if chunk.is_empty() {
                bail!("the deck returned no bytes for {} at {offset}", file.path);
            }
            out.write_all(&chunk)
                .with_context(|| format!("writing {}", destination.display()))?;
            offset += chunk.len() as u64;
            progress(offset, file.size);
        }
        out.flush()
            .with_context(|| format!("finishing {}", destination.display()))?;
        Ok(())
    }

    /// Release the mount. Best effort: the deck drops a stale mount on its own,
    /// and failing here must not fail a transfer that already succeeded.
    pub fn unmount(&mut self, mounted: &Mount) {
        let mut args = Vec::new();
        put_pascal_utf16(&mut args, &mounted.export);
        if let Err(err) = self.call(self.mount_port, MOUNT, MOUNT_VERSION, MOUNT_UMNT, &args) {
            tracing::debug!("ProLink: UMNT {} failed: {err:#}", mounted.export);
        }
    }

    fn read_chunk(&mut self, file: &RemoteFile, offset: u64) -> Result<Vec<u8>> {
        loop {
            let remaining = file.size.saturating_sub(offset);
            let count = remaining.min(self.read_size as u64) as u32;
            match self.read_once(file, offset, count) {
                Ok(bytes) => return Ok(bytes),
                Err(err) if self.read_size > UNFRAGMENTED_READ_SIZE => {
                    tracing::debug!(
                        "ProLink: {} read at {offset} failed at {} bytes ({err:#}); \
                         falling back to {UNFRAGMENTED_READ_SIZE}",
                        file.path,
                        self.read_size
                    );
                    self.read_size = UNFRAGMENTED_READ_SIZE;
                }
                Err(err) => return Err(err),
            }
        }
    }

    fn read_once(&mut self, file: &RemoteFile, offset: u64, count: u32) -> Result<Vec<u8>> {
        let mut args = Vec::with_capacity(HANDLE + 12);
        args.extend_from_slice(&file.handle);
        put_u32(&mut args, u32::try_from(offset).unwrap_or(u32::MAX));
        put_u32(&mut args, count);
        put_u32(&mut args, 0); // totalcount
        let reply = self.call(self.nfs_port, NFS, NFS_VERSION, NFS_READ, &args)?;
        let mut reader = Reader::new(&reply);
        let status = reader.u32()?;
        if status != 0 {
            return Err(NfsStatus {
                status,
                what: "READ",
            }
            .into());
        }
        let _attrs = reader.fattr()?;
        let length = reader.u32()? as usize;
        Ok(reader.bytes(length)?.to_vec())
    }

    fn getport(&mut self, program: u32, version: u32) -> Result<u16> {
        let mut args = Vec::new();
        put_u32(&mut args, program);
        put_u32(&mut args, version);
        put_u32(&mut args, IPPROTO_UDP);
        put_u32(&mut args, 0);
        let reply = self.call(111, PORTMAP, PORTMAP_VERSION, PORTMAP_GETPORT, &args)?;
        let mut reader = Reader::new(&reply);
        let port = reader.u32()?;
        if port == 0 {
            bail!("the deck registers no UDP port for program {program}");
        }
        Ok(port as u16)
    }

    /// One RPC, retried with the same transaction id.
    ///
    /// Reusing the xid is what lets a late first reply and a retry's reply be
    /// recognised as duplicates rather than as two answers.
    fn call(
        &mut self,
        port: u16,
        program: u32,
        version: u32,
        procedure: u32,
        args: &[u8],
    ) -> Result<Vec<u8>> {
        self.xid = self.xid.wrapping_add(1);
        let xid = self.xid;
        let message = build_call(xid, program, version, procedure, args);
        let destination = SocketAddr::from((self.server, port));

        for attempt in 1..=RPC_ATTEMPTS {
            self.socket
                .send_to(&message, destination)
                .with_context(|| format!("sending an RPC to {destination}"))?;
            let deadline = Instant::now() + RPC_TIMEOUT;
            let mut buffer = vec![0u8; 65_535];
            loop {
                match self.socket.recv_from(&mut buffer) {
                    Ok((length, from)) => {
                        if from.ip() != IpAddr::V4(self.server) {
                            continue;
                        }
                        if let Some(result) = parse_reply(xid, &buffer[..length])? {
                            return Ok(result);
                        }
                    }
                    Err(err)
                        if matches!(
                            err.kind(),
                            std::io::ErrorKind::WouldBlock
                                | std::io::ErrorKind::TimedOut
                                | std::io::ErrorKind::Interrupted
                        ) =>
                    {
                        break;
                    }
                    Err(err) => return Err(err).context("receiving an RPC reply"),
                }
                if Instant::now() >= deadline {
                    break;
                }
            }
            tracing::debug!(
                "ProLink: RPC {program}:{procedure} to {destination} timed out \
                 (attempt {attempt} of {RPC_ATTEMPTS})"
            );
        }
        bail!("the deck at {} did not answer an RPC", self.server)
    }
}

/// A decoded `fattr`, of which only the size is used.
struct Attr {
    size: u32,
}

/// The reply body after the RPC header.
fn build_call(xid: u32, program: u32, version: u32, procedure: u32, args: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(28 + args.len());
    put_u32(&mut out, xid);
    put_u32(&mut out, 0); // CALL
    put_u32(&mut out, 2); // RPC version
    put_u32(&mut out, program);
    put_u32(&mut out, version);
    put_u32(&mut out, procedure);

    // AUTH_UNIX: a fixed stamp, an empty machine name, uid/gid 0, and one group.
    // The CDJ does not inspect any of it.
    put_u32(&mut out, AUTH_UNIX);
    put_u32(&mut out, 20);
    put_u32(&mut out, 0xdead_beef);
    put_u32(&mut out, 0); // machine name length
    put_u32(&mut out, 0); // uid
    put_u32(&mut out, 0); // gid
    put_u32(&mut out, 0); // gids

    put_u32(&mut out, AUTH_NULL);
    put_u32(&mut out, 0);

    out.extend_from_slice(args);
    out
}

/// Decode an RPC reply. `Ok(None)` is a datagram for another transaction.
fn parse_reply(xid: u32, data: &[u8]) -> Result<Option<Vec<u8>>> {
    let mut reader = Reader::new(data);
    let reply_xid = reader.u32()?;
    let message_type = reader.u32()?;
    if reply_xid != xid || message_type != 1 {
        return Ok(None);
    }
    let reply_stat = reader.u32()?;
    if reply_stat != 0 {
        bail!("the deck rejected the RPC call");
    }
    // Verifier: flavor and an opaque body.
    let _flavor = reader.u32()?;
    let length = reader.u32()? as usize;
    reader.skip(padded(length))?;
    let accept_stat = reader.u32()?;
    if accept_stat != 0 {
        bail!("the deck refused the RPC program (accept status {accept_stat})");
    }
    Ok(Some(reader.rest().to_vec()))
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

/// XDR opaque: a length and its bytes, padded to four.
fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    put_u32(out, bytes.len() as u32);
    out.extend_from_slice(bytes);
    let pad = padded(bytes.len()) - bytes.len();
    out.resize(out.len() + pad, 0);
}

/// An NFS path component is a Pascal string encoded as UTF-16LE, where the
/// length is the byte count of the encoded text.
fn put_pascal_utf16(out: &mut Vec<u8>, text: &str) {
    let mut bytes = Vec::with_capacity(text.len() * 2);
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    put_bytes(out, &bytes);
}

const fn padded(length: usize) -> usize {
    length.next_multiple_of(4)
}

struct Reader<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    const fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }

    fn u32(&mut self) -> Result<u32> {
        let bytes = self.bytes(4)?;
        Ok(u32::from_be_bytes(bytes.try_into().expect("four bytes")))
    }

    fn bytes(&mut self, length: usize) -> Result<&'a [u8]> {
        let end = self.position.saturating_add(length);
        let bytes = self
            .data
            .get(self.position..end)
            .context("a short RPC reply")?;
        self.position = end;
        Ok(bytes)
    }

    fn skip(&mut self, length: usize) -> Result<()> {
        self.bytes(length).map(|_| ())
    }

    fn handle(&mut self) -> Result<[u8; HANDLE]> {
        Ok(self
            .bytes(HANDLE)?
            .try_into()
            .expect("the handle length is 32"))
    }

    fn fattr(&mut self) -> Result<Attr> {
        let _ftype = self.u32()?;
        let _mode = self.u32()?;
        let _nlink = self.u32()?;
        let _uid = self.u32()?;
        let _gid = self.u32()?;
        let size = self.u32()?;
        // blocksize, rdev, blocks, fsid, fileid, and three timestamps.
        for _ in 0..11 {
            self.u32()?;
        }
        Ok(Attr { size })
    }

    fn rest(self) -> &'a [u8] {
        &self.data[self.position..]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;

    const CONTENT: &[u8] = b"a medium, served over a fake NFS";

    fn reply(xid: u32, result: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        put_u32(&mut out, xid);
        put_u32(&mut out, 1); // REPLY
        put_u32(&mut out, 0); // accepted
        put_u32(&mut out, 0); // verifier flavor: null
        put_u32(&mut out, 0); // verifier length
        put_u32(&mut out, 0); // accept: success
        out.extend_from_slice(result);
        out
    }

    fn attrs(size: u32) -> Vec<u8> {
        let mut out = Vec::new();
        for index in 0..17 {
            put_u32(&mut out, if index == 5 { size } else { 0 });
        }
        out
    }

    struct Call {
        xid: u32,
        program: u32,
        procedure: u32,
        args: Vec<u8>,
    }

    fn parse_call(data: &[u8]) -> Option<Call> {
        let mut reader = Reader::new(data);
        let xid = reader.u32().ok()?;
        let message_type = reader.u32().ok()?;
        if message_type != 0 {
            return None;
        }
        let _rpc_version = reader.u32().ok()?;
        let program = reader.u32().ok()?;
        let _version = reader.u32().ok()?;
        let procedure = reader.u32().ok()?;
        // Two opaque auth bodies.
        for _ in 0..2 {
            let _flavor = reader.u32().ok()?;
            let length = reader.u32().ok()? as usize;
            reader.skip(padded(length)).ok()?;
        }
        Some(Call {
            xid,
            program,
            procedure,
            args: reader.rest().to_vec(),
        })
    }

    fn serve(socket: UdpSocket, stop: Arc<AtomicBool>) {
        socket
            .set_read_timeout(Some(Duration::from_millis(100)))
            .expect("a timeout");
        let mut buffer = [0u8; 65_535];
        while !stop.load(Ordering::Relaxed) {
            let (length, from) = match socket.recv_from(&mut buffer) {
                Ok(received) => received,
                Err(_) => continue,
            };
            let Some(call) = parse_call(&buffer[..length]) else {
                continue;
            };
            let result = match (call.program, call.procedure) {
                (MOUNT, MOUNT_MNT) => {
                    let mut out = Vec::new();
                    put_u32(&mut out, 0);
                    out.extend_from_slice(&[0x11; HANDLE]);
                    out
                }
                (MOUNT, MOUNT_UMNT) => Vec::new(),
                (NFS, NFS_LOOKUP) => {
                    let mut out = Vec::new();
                    put_u32(&mut out, 0);
                    out.extend_from_slice(&[0x22; HANDLE]);
                    out.extend_from_slice(&attrs(CONTENT.len() as u32));
                    out
                }
                (NFS, NFS_READ) => {
                    let mut reader = Reader::new(&call.args);
                    let _handle = reader.handle().expect("a handle");
                    let offset = reader.u32().expect("an offset") as usize;
                    let count = reader.u32().expect("a count") as usize;
                    let start = offset.min(CONTENT.len());
                    let end = (start + count).min(CONTENT.len());
                    let data = &CONTENT[start..end];
                    let mut out = Vec::new();
                    put_u32(&mut out, 0);
                    out.extend_from_slice(&attrs(CONTENT.len() as u32));
                    put_u32(&mut out, data.len() as u32);
                    out.extend_from_slice(data);
                    out.resize(out.len() + padded(data.len()) - data.len(), 0);
                    out
                }
                _ => {
                    let mut out = Vec::new();
                    put_u32(&mut out, 1); // status: permission denied
                    out
                }
            };
            socket
                .send_to(&reply(call.xid, &result), from)
                .expect("a reply");
        }
    }

    fn fake_deck() -> (u16, Arc<AtomicBool>, thread::JoinHandle<()>) {
        let socket = UdpSocket::bind(("127.0.0.1", 0)).expect("a fake deck");
        let port = socket.local_addr().expect("an address").port();
        let stop = Arc::new(AtomicBool::new(false));
        let handle = {
            let stop = Arc::clone(&stop);
            thread::spawn(move || serve(socket, stop))
        };
        (port, stop, handle)
    }

    #[test]
    fn a_track_can_be_read_over_nfs() {
        let (port, stop, handle) = fake_deck();
        let mut client =
            NfsClient::connect_with_ports(Ipv4Addr::LOCALHOST, port, port).expect("a client");
        let mounted = client.mount_slot(Slot::Usb).expect("a mount");
        let file = client
            .open(&mounted, "/Contents/deck/track.wav")
            .expect("a file");
        assert_eq!(file.size(), CONTENT.len() as u64);
        assert_eq!(client.read_file(&file).expect("a read"), CONTENT);
        client.unmount(&mounted);
        stop.store(true, Ordering::Relaxed);
        handle.join().expect("the fake deck");
    }

    #[test]
    fn a_download_reports_every_byte_once() {
        let (port, stop, handle) = fake_deck();
        let mut client =
            NfsClient::connect_with_ports(Ipv4Addr::LOCALHOST, port, port).expect("a client");
        let mounted = client.mount_slot(Slot::Sd).expect("a mount");
        let file = client.open(&mounted, "/track.wav").expect("a file");
        let destination = std::env::temp_dir().join(format!(
            "stemd-prolink-download-{}-{}",
            std::process::id(),
            CONTENT.len()
        ));
        let mut seen = 0u64;
        client
            .download_to(&file, &destination, |done, total| {
                assert!(done >= seen);
                assert_eq!(total, CONTENT.len() as u64);
                seen = done;
            })
            .expect("a download");
        assert_eq!(std::fs::read(&destination).expect("the file"), CONTENT);
        let _ = std::fs::remove_file(destination);
        stop.store(true, Ordering::Relaxed);
        handle.join().expect("the fake deck");
    }

    #[test]
    fn a_missing_file_is_recognisable() {
        let status = NfsStatus {
            status: NFS_NOENT,
            what: "LOOKUP",
        };
        let error: anyhow::Error = status.into();
        assert!(is_noent(&error));
    }
}
