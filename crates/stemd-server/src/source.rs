//! The client-computed identity of a source file.
//!
//! `GET /v1/jobs/lookup` is only useful if the server and the client agree on
//! what "the same file" means. The deck's shim and the iOS app compute this
//! value before uploading; the desktop computes it for a dropped file so an
//! entry a deck created can be reused without decoding the file again, and so
//! the folder a drop writes can be a cache hit too.
//!
//! The construction is deliberately cheap and deliberately not a content hash:
//! the size and the two ends catch truncation, re-encoding and a changed tag
//! block, which is what "the same track" means here. The offset basis is the
//! one the deck's own C code has always used, not the textbook FNV constant;
//! agreeing with the clients matters more than being canonical.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{bail, Context, Result};

/// Bytes read from each end. The middle of an eight-minute track is exactly
/// what does not change when a file is copied, retagged or re-exported.
const WINDOW: usize = 64 * 1024;

/// The deck's FNV-1a parameters. See the note above about the basis.
const OFFSET: u64 = 1_469_598_103_934_665_603;
const PRIME: u64 = 1_099_511_628_211;

fn mix(hash: &mut u64, bytes: &[u8]) {
    for &byte in bytes {
        *hash ^= u64::from(byte);
        *hash = hash.wrapping_mul(PRIME);
    }
}

/// Hash a file the way a client does before it uploads it.
///
/// Returns 16 lowercase hex characters. Errors are the caller's to ignore: a
/// file that cannot be read cannot be uploaded either.
pub fn file_hash(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let size = file
        .metadata()
        .with_context(|| format!("stat {}", path.display()))?
        .len();
    if size == 0 {
        bail!("{} is empty", path.display());
    }

    let mut hash = OFFSET;
    // The size as an eight-byte little-endian integer, which is what the C and
    // Swift implementations mix.
    mix(&mut hash, &size.to_le_bytes());

    let mut window = [0u8; WINDOW];
    let head = (size as usize).min(WINDOW);
    file.read_exact(&mut window[..head])
        .with_context(|| format!("reading {}", path.display()))?;
    mix(&mut hash, &window[..head]);

    if size > WINDOW as u64 {
        file.seek(SeekFrom::End(-(WINDOW as i64)))
            .with_context(|| format!("seeking {}", path.display()))?;
        file.read_exact(&mut window)
            .with_context(|| format!("reading the end of {}", path.display()))?;
        mix(&mut hash, &window);
    }

    Ok(format!("{hash:016x}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed vector, not just "it changes when the bytes do": the deck and
    /// the app are the other implementations, and this is the only place the
    /// constant can be checked without a device.
    #[test]
    fn a_small_file_hashes_the_way_the_deck_computes_it() {
        let dir = std::env::temp_dir().join(format!(
            "stemd-source-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("create the test directory");
        let path = dir.join("counted.bin");
        let bytes: Vec<u8> = (0..100).collect();
        std::fs::write(&path, &bytes).expect("write the test file");

        assert_eq!(file_hash(&path).expect("hash"), "2073823b5e5e4eab");

        std::fs::remove_dir_all(&dir).ok();
    }
}
