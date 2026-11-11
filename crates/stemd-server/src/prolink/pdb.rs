//! The subset of `export.pdb` this monitor needs.
//!
//! A rekordbox medium carries a page-oriented little-endian database at
//! `PIONEER/rekordbox/export.pdb`. Only three page types matter here — tracks,
//! artists and albums — and a track row already carries its file size, its
//! title and the path the deck would read over NFS.
//!
//! Strings are the part that traps implementations: the common form is UTF-16LE
//! text at +4 of the string, not the UTF-16BE at +3 that some older clients
//! assume. ASCII strings and non-ASCII titles both round-trip here.

use std::collections::HashMap;

use anyhow::{Result, bail};

/// Every page is 4096 bytes.
const PAGE_SIZE: usize = 4096;
/// Where the entry rows begin inside a page.
const ENTRIES_START: usize = 40;
/// Largest entry count before the footer's 4-byte masks are 8191 (unset).
const UNSET_LARGE_COUNT: usize = 8191;

const PAGE_TRACKS: u32 = 0;
const PAGE_ARTISTS: u32 = 2;
const PAGE_ALBUMS: u32 = 3;

const TRACK_MAGIC: u16 = 0x24;
const ARTIST_MAGIC: u16 = 0x60;
const LONG_ARTIST_MAGIC: u16 = 0x64;
const ALBUM_MAGIC: u16 = 0x80;

/// A track row, reduced to what a download needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Track {
    pub id: u32,
    pub title: String,
    pub artist_id: u32,
    pub album_id: u32,
    pub file_size: u32,
    pub duration: u16,
    /// The path relative to the mounted export, e.g. `/Contents/...`.
    pub path: String,
    pub filename: String,
}

/// A medium's library, parsed once.
pub struct Pdb {
    tracks: HashMap<u32, Track>,
    artists: HashMap<u32, String>,
    albums: HashMap<u32, String>,
}

impl Pdb {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < PAGE_SIZE {
            bail!("the database is shorter than one page");
        }
        let page_size = u32_at(bytes, 4).unwrap_or(0) as usize;
        if page_size != PAGE_SIZE {
            bail!("the database claims {page_size}-byte pages, not {PAGE_SIZE}");
        }

        let mut tracks = HashMap::new();
        let mut artists = HashMap::new();
        let mut albums = HashMap::new();

        let mut offset = PAGE_SIZE;
        while offset + PAGE_SIZE <= bytes.len() {
            let page = &bytes[offset..offset + PAGE_SIZE];
            let index = u32_at(page, 4).unwrap_or(0);
            let page_type = u32_at(page, 8).unwrap_or(u32::MAX);
            let u5 = page[27];
            let u9 = u16_at(page, 36).unwrap_or(0);
            let strange = index != 0 && u5 & 0x40 != 0;
            let empty = index == 0 && u9 == 0;
            if !strange && !empty {
                match page_type {
                    PAGE_TRACKS => {
                        for at in entries(page) {
                            if let Some(track) = parse_track(page, at) {
                                tracks.insert(track.id, track);
                            }
                        }
                    }
                    PAGE_ARTISTS => {
                        for at in entries(page) {
                            if let Some((id, name)) = parse_artist(page, at) {
                                artists.insert(id, name);
                            }
                        }
                    }
                    PAGE_ALBUMS => {
                        for at in entries(page) {
                            if let Some((id, name)) = parse_album(page, at) {
                                albums.insert(id, name);
                            }
                        }
                    }
                    _ => {}
                }
            }
            offset += PAGE_SIZE;
        }

        Ok(Self {
            tracks,
            artists,
            albums,
        })
    }

    pub fn track(&self, id: u32) -> Option<&Track> {
        self.tracks.get(&id)
    }

    pub fn artist(&self, id: u32) -> &str {
        self.artists.get(&id).map(String::as_str).unwrap_or("")
    }

    pub fn album(&self, id: u32) -> &str {
        self.albums.get(&id).map(String::as_str).unwrap_or("")
    }

    pub fn track_count(&self) -> usize {
        self.tracks.len()
    }
}

/// The entry offsets a page's reverse index names, relative to
/// [`ENTRIES_START`].
///
/// The footer is a sequence of blocks at the end of the page. Each block holds
/// up to 16 two-byte offsets followed by four bytes of enabled/override masks;
/// the last block is at the very end and earlier blocks sit before it. The masks
/// are ignored: a row is only accepted when its own magic and bounds are valid,
/// which catches the unused slots a mask would have filtered.
fn entries(page: &[u8]) -> Vec<usize> {
    let small = page[24] as usize;
    let large = u16_at(page, 34).unwrap_or(0) as usize;
    let strange = u32_at(page, 4).is_some_and(|index| index != 0) && page[27] & 0x40 != 0;
    let empty = u32_at(page, 4) == Some(0) && u16_at(page, 36) == Some(0);
    let count = if small < large && large != UNSET_LARGE_COUNT && !strange && !empty {
        large
    } else {
        small
    };
    if count == 0 {
        return Vec::new();
    }

    let mut out = Vec::with_capacity(count);
    let mut seen = 0;
    let mut cursor = page.len();
    loop {
        let block = (count - seen).min(16);
        let footer = 4 + 2 * block;
        if cursor < footer {
            break;
        }
        cursor -= footer;
        for index in 0..block {
            let at = ENTRIES_START + u16_at(page, cursor + 2 * index).unwrap_or(0) as usize;
            if at + 2 <= page.len() {
                out.push(at);
            }
        }
        seen += block;
        if seen >= count || block < 16 {
            break;
        }
    }
    out
}

fn parse_track(page: &[u8], at: usize) -> Option<Track> {
    if u16_at(page, at)? != TRACK_MAGIC {
        return None;
    }
    // The fixed row ends with the 21-entry string index at +0x5e.
    if at + 0x88 > page.len() {
        return None;
    }
    let id = u32_at(page, at + 0x48)?;
    if id == 0 {
        return None;
    }
    let string = |index: usize| -> String {
        let offset = u16_at(page, at + 0x5e + 2 * index).unwrap_or(0) as usize;
        if offset == 0 {
            return String::new();
        }
        pio_string(page, at + offset).unwrap_or_default()
    };
    Some(Track {
        id,
        title: string(17),
        artist_id: u32_at(page, at + 0x44)?,
        album_id: u32_at(page, at + 0x40)?,
        file_size: u32_at(page, at + 0x10)?,
        duration: u16_at(page, at + 0x54)?,
        path: string(20),
        filename: string(19),
    })
}

fn parse_artist(page: &[u8], at: usize) -> Option<(u32, String)> {
    let magic = u16_at(page, at)?;
    if magic != ARTIST_MAGIC && magic != LONG_ARTIST_MAGIC {
        return None;
    }
    let id = u32_at(page, at + 4)?;
    if id == 0 {
        return None;
    }
    let name_offset = if magic == LONG_ARTIST_MAGIC {
        u16_at(page, at + 10)? as usize
    } else {
        *page.get(at + 9)? as usize
    };
    let name = if name_offset == 0 {
        String::new()
    } else {
        pio_string(page, at + name_offset).unwrap_or_default()
    };
    Some((id, name))
}

fn parse_album(page: &[u8], at: usize) -> Option<(u32, String)> {
    if u16_at(page, at)? != ALBUM_MAGIC {
        return None;
    }
    let id = u32_at(page, at + 12)?;
    if id == 0 {
        return None;
    }
    let name_offset = *page.get(at + 21)? as usize;
    let name = if name_offset == 0 {
        String::new()
    } else {
        pio_string(page, at + name_offset).unwrap_or_default()
    };
    Some((id, name))
}

/// One rekordbox string.
///
/// The first byte selects the form. `0x90` is UTF-16LE text whose length field
/// is four bytes longer than the text; `0x40` is the same shape in ASCII; any
/// other first byte is the older ASCII form where the byte itself encodes the
/// padded length.
fn pio_string(data: &[u8], at: usize) -> Option<String> {
    let kind = *data.get(at)?;
    let (bytes, utf16) = match kind {
        0x40 | 0x90 => {
            let raw = u16_at(data, at + 1)? as usize;
            let length = raw.checked_sub(4)?;
            (data.get(at + 4..at + 4 + length)?, kind == 0x90)
        }
        padded if padded >= 3 => {
            let length = (padded as usize - 1) / 2 - 1;
            (data.get(at + 1..at + 1 + length)?, false)
        }
        _ => return Some(String::new()),
    };

    let text = if utf16 {
        let mut units = Vec::with_capacity(bytes.len() / 2);
        for pair in bytes.chunks_exact(2) {
            units.push(u16::from_le_bytes([pair[0], pair[1]]));
        }
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    };
    Some(text.trim_end_matches('\0').to_owned())
}

fn u16_at(data: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(data.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pio(text: &str) -> Vec<u8> {
        let mut bytes = Vec::new();
        for unit in text.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        let mut out = vec![0x90];
        out.extend_from_slice(&((bytes.len() + 4) as u16).to_le_bytes());
        out.push(0);
        out.extend_from_slice(&bytes);
        out
    }

    fn pio_ascii(text: &str) -> Vec<u8> {
        // The older form: the first byte is 2 * (len + 1) + 1.
        let padded = (2 * (text.len() + 1) + 1) as u8;
        let mut out = vec![padded];
        out.extend_from_slice(text.as_bytes());
        out
    }

    fn track_row(id: u32, title: &str, artist: u32, album: u32, path: &str, size: u32) -> Vec<u8> {
        let mut row = vec![0u8; 0x88];
        row[0..2].copy_from_slice(&TRACK_MAGIC.to_le_bytes());
        row[0x10..0x14].copy_from_slice(&size.to_le_bytes());
        row[0x40..0x44].copy_from_slice(&album.to_le_bytes());
        row[0x44..0x48].copy_from_slice(&artist.to_le_bytes());
        row[0x48..0x4c].copy_from_slice(&id.to_le_bytes());
        row[0x54..0x56].copy_from_slice(&123u16.to_le_bytes());
        add_string(&mut row, 17, &pio(title));
        add_string(&mut row, 19, &pio("file.mp3"));
        add_string(&mut row, 20, &pio(path));
        row
    }

    fn add_string(row: &mut Vec<u8>, index: usize, string: &[u8]) {
        let offset = row.len();
        row.extend_from_slice(string);
        row[0x5e + 2 * index..0x5e + 2 * index + 2].copy_from_slice(&(offset as u16).to_le_bytes());
    }

    fn artist_row(id: u32, name: &str) -> Vec<u8> {
        let mut row = vec![0u8; 10];
        row[0..2].copy_from_slice(&ARTIST_MAGIC.to_le_bytes());
        row[4..8].copy_from_slice(&id.to_le_bytes());
        row[9] = 10;
        row.extend_from_slice(&pio(name));
        row
    }

    fn album_row(id: u32, name: &str) -> Vec<u8> {
        let mut row = vec![0u8; 22];
        row[0..2].copy_from_slice(&ALBUM_MAGIC.to_le_bytes());
        row[12..16].copy_from_slice(&id.to_le_bytes());
        row[21] = 22;
        row.extend_from_slice(&pio(name));
        row
    }

    fn page(page_type: u32, index: u32, rows: &[Vec<u8>]) -> Vec<u8> {
        assert!(rows.len() <= 16);
        let mut page = vec![0u8; PAGE_SIZE];
        page[4..8].copy_from_slice(&index.to_le_bytes());
        page[8..12].copy_from_slice(&page_type.to_le_bytes());
        page[24] = rows.len() as u8;
        page[34..36].copy_from_slice(&(rows.len() as u16).to_le_bytes());

        let mut offsets = Vec::with_capacity(rows.len());
        let mut cursor = ENTRIES_START;
        for row in rows {
            offsets.push((cursor - ENTRIES_START) as u16);
            page[cursor..cursor + row.len()].copy_from_slice(row);
            cursor += row.len();
        }

        let footer = 4 + 2 * rows.len();
        let at = PAGE_SIZE - footer;
        assert!(cursor <= at, "the rows overlap the reverse index");
        for (index, offset) in offsets.iter().enumerate() {
            page[at + 2 * index..at + 2 * index + 2].copy_from_slice(&offset.to_le_bytes());
        }
        page[at + 2 * rows.len()..at + 2 * rows.len() + 4].copy_from_slice(&[0xff; 4]);
        page
    }

    fn database(track: Vec<u8>, artist: Vec<u8>, album: Vec<u8>) -> Vec<u8> {
        let mut file = vec![0u8; PAGE_SIZE];
        file[4..8].copy_from_slice(&(PAGE_SIZE as u32).to_le_bytes());
        file[8..12].copy_from_slice(&20u32.to_le_bytes());
        file.extend(page(PAGE_TRACKS, 1, &[track]));
        file.extend(page(PAGE_ARTISTS, 2, &[artist]));
        file.extend(page(PAGE_ALBUMS, 3, &[album]));
        file
    }

    #[test]
    fn a_track_and_its_names_are_found() {
        let bytes = database(
            track_row(42, "Title", 1, 2, "/Contents/deck/track.mp3", 1234),
            artist_row(1, "Artist"),
            album_row(2, "Album"),
        );
        let pdb = Pdb::parse(&bytes).expect("a database");
        let track = pdb.track(42).expect("the track");
        assert_eq!(track.title, "Title");
        assert_eq!(track.path, "/Contents/deck/track.mp3");
        assert_eq!(track.filename, "file.mp3");
        assert_eq!(track.artist_id, 1);
        assert_eq!(track.album_id, 2);
        assert_eq!(track.file_size, 1234);
        assert_eq!(track.duration, 123);
        assert_eq!(pdb.artist(1), "Artist");
        assert_eq!(pdb.album(2), "Album");
    }

    #[test]
    fn utf16le_text_is_not_read_as_big_endian() {
        let bytes = database(
            track_row(42, "Café", 1, 2, "/Contents/Café/track.mp3", 1234),
            artist_row(1, "Björk"),
            album_row(2, "Homogénic"),
        );
        let pdb = Pdb::parse(&bytes).expect("a database");
        let track = pdb.track(42).expect("the track");
        assert_eq!(track.title, "Café");
        assert_eq!(track.path, "/Contents/Café/track.mp3");
        assert_eq!(pdb.artist(1), "Björk");
        assert_eq!(pdb.album(2), "Homogénic");
    }

    #[test]
    fn the_old_ascii_string_form_decodes() {
        let mut row = track_row(7, "ignored", 0, 0, "/x.mp3", 1);
        let at = row.len();
        row.extend_from_slice(&pio_ascii("Plain"));
        row[0x5e + 2 * 17..0x5e + 2 * 17 + 2].copy_from_slice(&(at as u16).to_le_bytes());
        let page = page(PAGE_TRACKS, 1, &[row]);
        let mut file = vec![0u8; PAGE_SIZE];
        file[4..8].copy_from_slice(&(PAGE_SIZE as u32).to_le_bytes());
        file.extend(page);
        let pdb = Pdb::parse(&file).expect("a database");
        assert_eq!(pdb.track(7).expect("the track").title, "Plain");
    }

    #[test]
    fn a_missing_track_is_none_rather_than_empty() {
        let bytes = database(
            track_row(42, "Title", 1, 2, "/Contents/track.mp3", 1234),
            artist_row(1, "Artist"),
            album_row(2, "Album"),
        );
        let pdb = Pdb::parse(&bytes).expect("a database");
        assert!(pdb.track(43).is_none());
        assert_eq!(pdb.artist(99), "");
        assert_eq!(pdb.album(99), "");
    }

    #[test]
    fn a_file_that_is_not_a_database_is_refused() {
        assert!(Pdb::parse(&[0u8; 100]).is_err());
        assert!(Pdb::parse(&vec![0u8; PAGE_SIZE * 2]).is_err());
    }
}
