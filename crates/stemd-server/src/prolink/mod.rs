//! Watching a Pioneer Pro DJ Link network.
//!
//! Decks publish their loaded track only in status packets, and they unicast
//! those to peers that have announced themselves. So this monitor listens
//! passively until it hears a real player's keepalive, and only then starts
//! announcing itself. With no decks on the network it transmits nothing at all.
//!
//! When a rekordbox-analyzed track is loaded from USB or SD, the monitor reads
//! the medium over NFS, pulls `export.pdb` for the metadata and path, downloads
//! the file once per identity, and hands it to the same drop queue the window
//! uses. The identity is `(source player, slot, track id, metadata hash)`.
//!
//! The protocol behaviour follows `flesniak/python-prodj-link` (Apache-2.0);
//! the Rust implementation here was written from the wire format. Its packet
//! module credits `brunchboy/dysentery` (EPL-1.0) and `awwright/libpdjl`; none
//! of their code is copied here. The Rust `prolink` crate (GPL-3.0) was used
//! only to confirm protocol facts and is not linked or copied.

mod fetch;
mod identity;
mod nfs;
mod packets;
mod pdb;
mod vcdj;

use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, Ipv4Addr, SocketAddrV4, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use parking_lot::Mutex;

use crate::cli::Args;
use crate::models;

pub use packets::{Analyze, PlayState, Slot};

/// How often the virtual CDJ announces while a player is present.
const KEEPALIVE_PERIOD: Duration = Duration::from_millis(1500);
/// A player that has not sent anything for this long is gone.
const DECK_TTL: Duration = Duration::from_secs(5);
/// A track must stay loaded this long before a download is started, so browsing
/// a medium does not fetch every row the cursor crosses.
const STABLE_FOR: Duration = Duration::from_millis(500);
/// One socket read. Short enough that a stop is noticed promptly.
const SOCKET_TIMEOUT: Duration = Duration::from_millis(200);

/// The interface and local identity to announce as.
#[derive(Clone, Debug)]
pub struct Interface {
    pub name: String,
    pub ip: Ipv4Addr,
    pub broadcast: Ipv4Addr,
    pub mac: [u8; 6],
}

/// Everything a monitor needs, resolved from the command line.
#[derive(Clone, Debug)]
pub struct Config {
    pub interface: Interface,
    pub download_dir: PathBuf,
    pub deck_number: u8,
    /// Fetch and separate newly seen tracks. Off means the monitor is a
    /// display: it still reads the PDB for titles, but never pulls audio.
    pub download: bool,
}

impl Config {
    /// `Ok(None)` when `--prolink` was not given.
    pub fn from_args(args: &Args) -> Result<Option<Self>> {
        if !args.prolink {
            return Ok(None);
        }
        if args.prolink_deck_number == 0 || args.prolink_deck_number > 127 {
            bail!(
                "--prolink-deck-number {} is outside 1–127",
                args.prolink_deck_number
            );
        }
        let interface =
            resolve_interface(args.prolink_interface.as_deref(), args.prolink_deck_number)?;
        let download_dir = match &args.prolink_download_dir {
            Some(directory) => directory.clone(),
            None => models::support_dir()?.join("prolink"),
        };
        std::fs::create_dir_all(&download_dir)
            .with_context(|| format!("creating {}", download_dir.display()))?;
        Ok(Some(Self {
            interface,
            download_dir,
            deck_number: args.prolink_deck_number,
            download: args.prolink_download,
        }))
    }
}

/// A started monitor and the files it downloads.
pub struct Started {
    pub handle: Arc<ProLink>,
    /// Paths of files that finished downloading, with what the PDB said about
    /// them. The receiver's owner decides what to do with them; in stemd that
    /// is the drop queue.
    pub downloads: mpsc::Receiver<Downloaded>,
}

/// One track the monitor wrote to disk.
pub struct Downloaded {
    pub path: PathBuf,
    pub details: crate::details::TrackDetails,
}

/// A track as the window sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct FetchKey {
    pub source: u8,
    pub slot: Slot,
    pub id: u32,
}

/// Where a track is in the download pipeline.
#[derive(Clone, Debug, Default)]
pub enum FetchState {
    #[default]
    Idle,
    Resolving,
    Downloading {
        done: u64,
        total: u64,
    },
    Downloaded,
    /// Already downloaded in an earlier run.
    Seen,
    /// Display-only mode could not read the medium for a title.
    Unavailable,
    Failed(String),
}

/// Metadata resolved from the medium.
#[derive(Clone, Debug)]
pub struct MetaView {
    pub title: String,
    pub artist: String,
}

#[derive(Clone, Debug)]
pub struct LoadedView {
    pub track_id: u32,
    pub source: u8,
    pub slot: Slot,
    pub play: PlayState,
    pub meta: Option<MetaView>,
    pub fetch: FetchState,
}

#[derive(Clone, Debug)]
pub struct DeckView {
    pub number: u8,
    pub model: String,
    pub address: Ipv4Addr,
    pub loaded: Option<LoadedView>,
}

/// A consistent copy of the monitor's state for one frame.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub interface: String,
    pub decks: Vec<DeckView>,
}

/// The monitor's handle. Shared with the window; contains no `AppState`, so
/// there is no cycle between the two.
pub struct ProLink {
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    interface: String,
    commands: mpsc::Sender<fetch::Command>,
}

impl ProLink {
    pub fn snapshot(&self) -> Snapshot {
        let state = self.state.lock();
        let mut decks: Vec<DeckView> = state
            .decks
            .values()
            .map(|deck| DeckView {
                number: deck.number,
                model: deck.model.clone(),
                address: deck.address,
                loaded: deck.loaded.map(|loaded| LoadedView {
                    track_id: loaded.key.id,
                    source: loaded.key.source,
                    slot: loaded.key.slot,
                    play: loaded.play,
                    meta: state.metas.get(&loaded.key).cloned(),
                    fetch: state.fetches.get(&loaded.key).cloned().unwrap_or_default(),
                }),
            })
            .collect();
        decks.sort_by_key(|deck| deck.number);
        Snapshot {
            interface: self.interface.clone(),
            decks,
        }
    }

    /// Delete the tracks fetched from decks and the identities that mark them
    /// as already downloaded, then queue whatever is loaded now again.
    ///
    /// Separated stems are untouched: they live in the cache, and "Empty cache"
    /// is what removes those.
    pub fn forget_downloads(&self) {
        if self.commands.send(fetch::Command::Forget).is_err() {
            tracing::debug!("ProLink: download worker is gone");
            return;
        }
        let mut state = self.state.lock();
        state.fetches.clear();
        let now = Instant::now();
        for deck in state.decks.values_mut() {
            deck.candidate = deck.loaded.and_then(|loaded| {
                (loaded.analyze.is_rekordbox() && loaded.key.slot.is_media()).then_some(Candidate {
                    key: loaded.key,
                    since: now,
                    sent: false,
                })
            });
        }
    }

    /// Stop watching. Idempotent, and deliberately not a `Drop`: every exit
    /// path in stemd calls `_exit`, so destructors do not run.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Bind the sockets and start the two threads.
pub fn start(config: Config) -> Result<Started> {
    std::fs::create_dir_all(&config.download_dir)
        .with_context(|| format!("creating {}", config.download_dir.display()))?;

    let keepalive = UdpSocket::bind(("0.0.0.0", 50000)).context(
        "binding UDP 50000 for Pro DJ Link; another ProLink tool may already be running",
    )?;
    keepalive
        .set_broadcast(true)
        .context("enabling broadcast for the ProLink announcement")?;
    keepalive
        .set_read_timeout(Some(SOCKET_TIMEOUT))
        .context("setting the ProLink keepalive timeout")?;
    let status =
        UdpSocket::bind(("0.0.0.0", 50002)).context("binding UDP 50002 for Pro DJ Link status")?;
    status
        .set_read_timeout(Some(SOCKET_TIMEOUT))
        .context("setting the ProLink status timeout")?;

    let state = Arc::new(Mutex::new(State::default()));
    let stop = Arc::new(AtomicBool::new(false));
    let (commands_tx, commands_rx) = mpsc::channel();
    let (downloads_tx, downloads) = mpsc::channel();

    let fetcher = fetch::Fetcher::new(
        Arc::clone(&state),
        config.download_dir.clone(),
        downloads_tx,
        config.download,
    );
    let fetcher_stop = Arc::clone(&stop);
    std::thread::Builder::new()
        .name("stemd-prolink-fetch".into())
        .spawn(move || fetcher.run(commands_rx, fetcher_stop))
        .context("spawning the ProLink download worker")?;

    let interface = config.interface.name.clone();
    let address = config.interface.ip;
    let deck_number = config.deck_number;
    let download = config.download;
    let monitor_state = Arc::clone(&state);
    let monitor_stop = Arc::clone(&stop);
    let monitor_commands = commands_tx.clone();
    std::thread::Builder::new()
        .name("stemd-prolink".into())
        .spawn(move || {
            monitor(
                keepalive,
                status,
                config,
                monitor_state,
                monitor_stop,
                monitor_commands,
            )
        })
        .context("spawning the ProLink monitor")?;

    tracing::info!(
        "ProLink: watching {interface} ({address}), announcing as deck {deck_number} \
         only while a player is present{}",
        if download {
            "; new tracks will be downloaded and separated"
        } else {
            " (display only; --prolink-download fetches tracks)"
        }
    );
    Ok(Started {
        handle: Arc::new(ProLink {
            state,
            stop,
            interface,
            commands: commands_tx,
        }),
        downloads,
    })
}

fn monitor(
    keepalive: UdpSocket,
    status: UdpSocket,
    config: Config,
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    commands: mpsc::Sender<fetch::Command>,
) {
    let announcement = vcdj::announcement(
        config.deck_number,
        "Virtual CDJ",
        config.interface.ip,
        config.interface.mac,
    );
    let target = SocketAddrV4::new(config.interface.broadcast, 50000);
    let mut announced_at: Option<Instant> = None;
    let mut buffer = [0u8; 2048];

    while !stop.load(Ordering::Relaxed) {
        let players_present = {
            let state = state.lock();
            state
                .decks
                .values()
                .any(|deck| deck.last_seen.elapsed() < DECK_TTL)
        };

        // Passive until a player is heard: a network with no decks gets no
        // ProLink traffic from us at all.
        if players_present {
            if announced_at.is_none() {
                tracing::info!(
                    "ProLink: announcing as deck {} while a player is present",
                    config.deck_number
                );
            }
            let due = announced_at.is_none_or(|at| at.elapsed() >= KEEPALIVE_PERIOD);
            if due {
                if let Err(err) = keepalive.send_to(&announcement, target) {
                    tracing::debug!("ProLink: announcement failed: {err}");
                }
                announced_at = Some(Instant::now());
            }
        } else if announced_at.is_some() {
            tracing::info!("ProLink: no players left; stopped announcing");
            announced_at = None;
        }

        receive(&keepalive, &state, true, config.deck_number, &mut buffer);
        receive(&status, &state, false, config.deck_number, &mut buffer);
        tick(&state, &commands);
    }
}

fn receive(socket: &UdpSocket, state: &Mutex<State>, keepalive: bool, own: u8, buffer: &mut [u8]) {
    match socket.recv_from(buffer) {
        Ok((length, from)) => {
            let packet = &buffer[..length];
            if keepalive {
                if let Some(parsed) = packets::parse_keepalive(packet)
                    && parsed.player != 0
                    && parsed.player != own
                {
                    observe_keepalive(state, parsed);
                }
            } else if let Some(parsed) = packets::parse_status(packet)
                && parsed.player != own
            {
                let IpAddr::V4(address) = from.ip() else {
                    return;
                };
                observe_status(state, parsed, address);
            }
        }
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::WouldBlock
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::Interrupted
            ) => {}
        Err(err) => tracing::debug!("ProLink: receive failed: {err}"),
    }
}

fn observe_keepalive(state: &Mutex<State>, keepalive: packets::KeepAlive) {
    let mut state = state.lock();
    let number = keepalive.player;
    match state.decks.entry(number) {
        std::collections::btree_map::Entry::Occupied(mut entry) => {
            let deck = entry.get_mut();
            deck.last_seen = Instant::now();
            deck.address = keepalive.address;
            if !keepalive.model.is_empty() {
                deck.model = keepalive.model;
            }
        }
        std::collections::btree_map::Entry::Vacant(entry) => {
            tracing::info!(
                "ProLink: deck {number} ({}) at {}",
                keepalive.model,
                keepalive.address
            );
            entry.insert(Deck {
                number,
                model: keepalive.model,
                address: keepalive.address,
                last_seen: Instant::now(),
                loaded: None,
                candidate: None,
            });
        }
    }
}

fn observe_status(state: &Mutex<State>, status: packets::Status, address: Ipv4Addr) {
    let mut state = state.lock();
    let now = Instant::now();
    let number = status.player;
    let deck = state.decks.entry(number).or_insert_with(|| Deck {
        number,
        model: status.model.clone(),
        address,
        last_seen: now,
        loaded: None,
        candidate: None,
    });
    deck.last_seen = now;
    if deck.address.is_unspecified() {
        deck.address = address;
    }
    if !status.model.is_empty() {
        deck.model = status.model;
    }

    let key = FetchKey {
        source: status.source,
        slot: status.slot,
        id: status.track_id,
    };
    let loaded = (status.track_id != 0).then_some(Loaded {
        key,
        analyze: status.analyze,
        play: status.play,
    });
    let changed = deck.loaded.map(|loaded| (loaded.key, loaded.analyze))
        != loaded.as_ref().map(|loaded| (loaded.key, loaded.analyze));
    if !changed {
        if let Some(loaded) = &mut deck.loaded {
            loaded.play = status.play;
        }
        return;
    }

    deck.loaded = loaded;
    let downloadable = deck
        .loaded
        .is_some_and(|loaded| loaded.analyze.is_rekordbox() && loaded.key.slot.is_media());
    deck.candidate = if downloadable {
        deck.loaded.map(|loaded| Candidate {
            key: loaded.key,
            since: now,
            sent: false,
        })
    } else {
        None
    };

    match &deck.loaded {
        Some(loaded) if downloadable => tracing::info!(
            "ProLink: deck {number} loaded track {} from {} on deck {}",
            loaded.key.id,
            loaded.key.slot.name(),
            loaded.key.source
        ),
        Some(loaded) => tracing::debug!(
            "ProLink: deck {number} has track {} on {}, not a \
             rekordbox-analyzed USB/SD track; not downloading",
            loaded.key.id,
            loaded.key.slot.name()
        ),
        None => tracing::debug!("ProLink: deck {number} has nothing loaded"),
    }
}

fn tick(state: &Mutex<State>, commands: &mpsc::Sender<fetch::Command>) {
    let mut queued = Vec::new();
    {
        let mut state = state.lock();
        state
            .decks
            .retain(|_, deck| deck.last_seen.elapsed() < DECK_TTL);

        let addresses: HashMap<u8, Ipv4Addr> = state
            .decks
            .iter()
            .map(|(number, deck)| (*number, deck.address))
            .collect();
        let State { decks, fetches, .. } = &mut *state;

        for deck in decks.values_mut() {
            let Some(candidate) = &mut deck.candidate else {
                continue;
            };
            if candidate.sent || candidate.since.elapsed() < STABLE_FOR {
                continue;
            }
            let key = candidate.key;
            if matches!(
                fetches.get(&key),
                Some(
                    FetchState::Resolving
                        | FetchState::Downloading { .. }
                        | FetchState::Downloaded
                        | FetchState::Seen
                )
            ) {
                candidate.sent = true;
                continue;
            }
            let Some(address) = addresses
                .get(&key.source)
                .copied()
                .filter(|address| !address.is_unspecified())
            else {
                continue;
            };
            // Claim it before the channel send so a second deck playing the
            // same medium does not enqueue a duplicate.
            fetches.insert(key, FetchState::Resolving);
            candidate.sent = true;
            queued.push(fetch::Command::Fetch(fetch::Request { key, address }));
        }
    }

    for command in queued {
        if commands.send(command).is_err() {
            tracing::debug!("ProLink: download worker is gone");
            break;
        }
    }
}

/// Pick the interface facing the decks.
///
/// A link-local address is the default on a network without DHCP, which is what
/// a DJ rig usually is. A named interface wins over that preference, and a name
/// that does not exist is an error rather than a silent fallback.
fn resolve_interface(requested: Option<&str>, deck_number: u8) -> Result<Interface> {
    let interfaces = if_addrs::get_if_addrs().context("listing network interfaces")?;
    let mut candidates: Vec<(bool, bool, Interface)> = Vec::new();

    for interface in interfaces {
        if interface.is_loopback() {
            continue;
        }
        let if_addrs::IfAddr::V4(address) = &interface.addr else {
            continue;
        };
        if address.ip.is_unspecified() {
            continue;
        }
        if let Some(name) = requested
            && interface.name != name
        {
            continue;
        }
        let broadcast = address.broadcast.unwrap_or_else(|| {
            let ip = u32::from(address.ip);
            let mask = u32::from(address.netmask);
            Ipv4Addr::from(ip | !mask)
        });
        candidates.push((
            !interface.is_link_local(),
            !interface.is_oper_up(),
            Interface {
                name: interface.name.clone(),
                ip: address.ip,
                broadcast,
                mac: vcdj::synthetic_mac(deck_number),
            },
        ));
    }

    candidates.sort_by(|left, right| {
        (left.0, left.1, &left.2.name).cmp(&(right.0, right.1, &right.2.name))
    });
    match candidates.into_iter().next() {
        Some((_, _, interface)) => Ok(interface),
        None => match requested {
            Some(name) => {
                bail!("--prolink-interface {name:?} is not an IPv4 interface on this machine")
            }
            None => bail!("no IPv4 network interface to watch for Pro DJ Link decks"),
        },
    }
}

/// The monitor's mutable state, shared by the network thread, the download
/// worker and the window.
#[derive(Default)]
pub(super) struct State {
    pub(super) decks: BTreeMap<u8, Deck>,
    pub(super) fetches: HashMap<FetchKey, FetchState>,
    pub(super) metas: HashMap<FetchKey, MetaView>,
}

pub(super) struct Deck {
    number: u8,
    model: String,
    address: Ipv4Addr,
    last_seen: Instant,
    loaded: Option<Loaded>,
    candidate: Option<Candidate>,
}

#[derive(Clone, Copy)]
struct Loaded {
    key: FetchKey,
    analyze: Analyze,
    play: PlayState,
}

struct Candidate {
    key: FetchKey,
    since: Instant,
    sent: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_named_interface_that_does_not_exist_is_an_error() {
        let error = resolve_interface(Some("definitely-not-an-interface"), 7)
            .expect_err("a missing interface");
        assert!(error.to_string().contains("definitely-not-an-interface"));
    }

    #[test]
    fn a_snapshot_carries_the_metadata_and_download_state() {
        let key = FetchKey {
            source: 1,
            slot: Slot::Usb,
            id: 42,
        };
        let mut state = State::default();
        state.decks.insert(
            2,
            Deck {
                number: 2,
                model: "CDJ-3000".into(),
                address: Ipv4Addr::new(169, 254, 1, 2),
                last_seen: Instant::now(),
                loaded: Some(Loaded {
                    key,
                    analyze: Analyze::Rekordbox,
                    play: PlayState::Playing,
                }),
                candidate: None,
            },
        );
        state.metas.insert(
            key,
            MetaView {
                title: "Title".into(),
                artist: "Artist".into(),
            },
        );
        state.fetches.insert(key, FetchState::Downloaded);

        let (commands, _worker) = mpsc::channel();
        let prolink = ProLink {
            state: Arc::new(Mutex::new(state)),
            stop: Arc::new(AtomicBool::new(false)),
            interface: "en0".into(),
            commands,
        };
        let snapshot = prolink.snapshot();
        assert_eq!(snapshot.interface, "en0");
        assert_eq!(
            snapshot
                .decks
                .iter()
                .filter(|deck| deck.loaded.is_some())
                .count(),
            1
        );
        let loaded = snapshot.decks[0].loaded.as_ref().expect("a loaded track");
        assert_eq!(loaded.track_id, 42);
        assert_eq!(loaded.meta.as_ref().expect("metadata").title, "Title");
        assert!(matches!(loaded.fetch, FetchState::Downloaded));
    }

    #[test]
    fn a_candidate_is_claimed_once_and_only_when_its_source_is_known() {
        let key = FetchKey {
            source: 1,
            slot: Slot::Usb,
            id: 7,
        };
        let mut state = State::default();
        state.decks.insert(
            1,
            Deck {
                number: 1,
                model: "CDJ".into(),
                address: Ipv4Addr::new(169, 254, 1, 1),
                last_seen: Instant::now(),
                loaded: None,
                candidate: None,
            },
        );
        state.decks.insert(
            2,
            Deck {
                number: 2,
                model: "CDJ".into(),
                address: Ipv4Addr::new(169, 254, 1, 2),
                last_seen: Instant::now(),
                loaded: Some(Loaded {
                    key,
                    analyze: Analyze::Rekordbox,
                    play: PlayState::Playing,
                }),
                candidate: Some(Candidate {
                    key,
                    since: Instant::now() - STABLE_FOR,
                    sent: false,
                }),
            },
        );
        let state = Mutex::new(state);
        let (sender, receiver) = mpsc::channel();
        tick(&state, &sender);
        tick(&state, &sender);
        let commands: Vec<_> = receiver.try_iter().collect();
        assert_eq!(commands.len(), 1);
        match &commands[0] {
            fetch::Command::Fetch(request) => {
                assert_eq!(request.key, key);
                assert_eq!(request.address, Ipv4Addr::new(169, 254, 1, 1));
            }
            fetch::Command::Forget => panic!("expected a fetch, got Forget"),
        }
        assert!(matches!(
            state.lock().fetches.get(&key),
            Some(FetchState::Resolving)
        ));
    }
}
