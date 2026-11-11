//! Pro DJ Link wire formats.
//!
//! Only the two packet families the monitor needs are decoded: the keepalive
//! that tells us a player exists, and the CDJ status that tells us what it has
//! loaded. Everything is big-endian, unlike the rekordbox database, which is
//! little-endian.
//!
//! The layouts are implemented from the protocol facts; see [`super`] for the
//! attribution to `python-prodj-link`.

use std::net::Ipv4Addr;

/// Ten-byte ASCII prefix on every Pro DJ Link packet.
pub const MAGIC: &[u8; 10] = b"Qspt1WmJOL";

/// Keepalive type: a device's status, including its player number and address.
const KEEPALIVE_STATUS: u8 = 0x06;
/// Keepalive type: an address announcement.
const KEEPALIVE_IP: u8 = 0x02;
/// Status type: a CDJ reporting what it has loaded.
const STATUS_CDJ: u8 = 0x0a;

/// A player seen on the network.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeepAlive {
    pub player: u8,
    pub model: String,
    pub mac: [u8; 6],
    pub address: Ipv4Addr,
    pub flags: u8,
}

/// Where a loaded track's medium sits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Slot {
    Usb,
    Sd,
    Other(u8),
}

impl Slot {
    pub const fn from_status(value: u8) -> Self {
        match value {
            2 => Self::Sd,
            3 => Self::Usb,
            other => Self::Other(other),
        }
    }

    /// True for the two slots whose media can be read over NFS.
    pub const fn is_media(self) -> bool {
        matches!(self, Self::Usb | Self::Sd)
    }

    /// The stable value the identity and the index store use.
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::Sd => 2,
            Self::Usb => 3,
            Self::Other(value) => value,
        }
    }

    /// The NFS export that holds this medium.
    pub const fn export(self) -> Option<&'static str> {
        match self {
            Self::Usb => Some("/C/"),
            Self::Sd => Some("/B/"),
            Self::Other(_) => None,
        }
    }

    /// A short word for the window.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Usb => "USB",
            Self::Sd => "SD",
            Self::Other(0) => "empty",
            Self::Other(1) => "CD",
            Self::Other(4) => "rekordbox",
            Self::Other(_) => "slot",
        }
    }
}

/// What produced a loaded track's metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Analyze {
    Rekordbox,
    File,
    Cd,
    Other(u8),
}

impl Analyze {
    pub const fn from_status(value: u8) -> Self {
        match value {
            1 => Self::Rekordbox,
            2 => Self::File,
            5 => Self::Cd,
            other => Self::Other(other),
        }
    }

    /// Only a rekordbox-analyzed track has a row in `export.pdb` and a path we
    /// can read over NFS.
    pub const fn is_rekordbox(self) -> bool {
        matches!(self, Self::Rekordbox)
    }
}

/// What a deck is doing with its loaded track.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayState {
    NoTrack,
    Loading,
    Playing,
    Looping,
    Paused,
    Cued,
    Cueing,
    CueScratch,
    Seeking,
    CannotPlay,
    End,
    Emergency,
    Unknown(u8),
}

impl PlayState {
    pub const fn from_status(value: u8) -> Self {
        match value {
            0 => Self::NoTrack,
            2 => Self::Loading,
            3 => Self::Playing,
            4 => Self::Looping,
            5 => Self::Paused,
            6 => Self::Cued,
            7 => Self::Cueing,
            8 => Self::CueScratch,
            9 => Self::Seeking,
            0x0e => Self::CannotPlay,
            0x11 => Self::End,
            0x12 => Self::Emergency,
            other => Self::Unknown(other),
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::NoTrack => "no track",
            Self::Loading => "loading",
            Self::Playing | Self::Looping => "playing",
            Self::Paused => "paused",
            Self::Cued | Self::Cueing | Self::CueScratch => "cued",
            Self::Seeking => "seeking",
            Self::CannotPlay | Self::End | Self::Emergency => "not playing",
            Self::Unknown(_) => "loaded",
        }
    }
}

/// A CDJ's status packet, the only place the loaded track is published.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    /// The deck that sent the packet.
    pub player: u8,
    pub model: String,
    /// The player whose medium holds the track. A deck can play from another's
    /// USB stick, so this is not always [`Status::player`].
    pub source: u8,
    pub slot: Slot,
    pub analyze: Analyze,
    pub track_id: u32,
    pub play: PlayState,
}

/// Decode a keepalive, or `None` for a packet this monitor does not use.
pub fn parse_keepalive(data: &[u8]) -> Option<KeepAlive> {
    if data.len() < 0x24 || &data[..MAGIC.len()] != MAGIC {
        return None;
    }
    match data[0x0a] {
        KEEPALIVE_STATUS => {
            if data.len() < 0x36 {
                return None;
            }
            let flags = data[0x34];
            // NXS-GW is a network gateway, not a player.
            if flags & 0x08 != 0 {
                return None;
            }
            Some(KeepAlive {
                player: data[0x24],
                model: model_name(&data[0x0c..0x20]),
                mac: data[0x26..0x2c].try_into().ok()?,
                address: ipv4(&data[0x2c..0x30])?,
                flags,
            })
        }
        KEEPALIVE_IP => {
            if data.len() < 0x32 {
                return None;
            }
            let flags = data[0x30];
            if flags & 0x08 != 0 {
                return None;
            }
            Some(KeepAlive {
                player: data[0x2e],
                model: model_name(&data[0x0c..0x20]),
                mac: data[0x28..0x2e].try_into().ok()?,
                address: ipv4(&data[0x24..0x28])?,
                flags,
            })
        }
        _ => None,
    }
}

/// Decode a CDJ status packet, or `None` for a packet this monitor does not use.
pub fn parse_status(data: &[u8]) -> Option<Status> {
    // 0x7b is the play-state byte; a packet shorter than that cannot tell us
    // what the deck is doing, so it is treated as unreadable rather than as
    // "nothing loaded".
    if data.len() < 0x7c || &data[..MAGIC.len()] != MAGIC || data[0x0a] != STATUS_CDJ {
        return None;
    }
    Some(Status {
        player: data[0x21],
        model: model_name(&data[0x0b..0x1f]),
        source: data[0x28],
        slot: Slot::from_status(data[0x29]),
        analyze: Analyze::from_status(data[0x2a]),
        track_id: u32::from_be_bytes(data[0x2c..0x30].try_into().ok()?),
        play: PlayState::from_status(data[0x7b]),
    })
}

/// A model name is twenty NUL-padded ASCII bytes.
fn model_name(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    let text = String::from_utf8_lossy(&bytes[..end]);
    text.trim().to_owned()
}

fn ipv4(bytes: &[u8]) -> Option<Ipv4Addr> {
    Some(Ipv4Addr::new(
        *bytes.first()?,
        *bytes.get(1)?,
        *bytes.get(2)?,
        *bytes.get(3)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keepalive(player: u8, ip: [u8; 4], flags: u8) -> Vec<u8> {
        let mut packet = vec![0u8; 0x36];
        packet[..MAGIC.len()].copy_from_slice(MAGIC);
        packet[0x0a] = KEEPALIVE_STATUS;
        packet[0x0c..0x0c + 9].copy_from_slice(b"CDJ-3000\0");
        packet[0x21] = 2;
        packet[0x23] = 0x36;
        packet[0x24] = player;
        packet[0x26..0x2c].copy_from_slice(&[0xa0, 0xce, 0xc8, 1, 2, 3]);
        packet[0x2c..0x30].copy_from_slice(&ip);
        packet[0x34] = flags;
        packet[0x35] = 0x64;
        packet
    }

    fn status(player: u8, source: u8, slot: u8, analyze: u8, id: u32, play: u8) -> Vec<u8> {
        let mut packet = vec![0u8; 0x7c];
        packet[..MAGIC.len()].copy_from_slice(MAGIC);
        packet[0x0a] = STATUS_CDJ;
        packet[0x0b..0x0b + 9].copy_from_slice(b"CDJ-3000\0");
        packet[0x21] = player;
        packet[0x28] = source;
        packet[0x29] = slot;
        packet[0x2a] = analyze;
        packet[0x2c..0x30].copy_from_slice(&id.to_be_bytes());
        packet[0x7b] = play;
        packet
    }

    #[test]
    fn a_status_keepalive_names_the_player_and_its_address() {
        let parsed = parse_keepalive(&keepalive(3, [169, 254, 1, 7], 1)).expect("a keepalive");
        assert_eq!(parsed.player, 3);
        assert_eq!(parsed.model, "CDJ-3000");
        assert_eq!(parsed.address, Ipv4Addr::new(169, 254, 1, 7));
        assert_eq!(parsed.mac, [0xa0, 0xce, 0xc8, 1, 2, 3]);
    }

    #[test]
    fn an_nxs_gateway_is_not_a_player() {
        assert!(parse_keepalive(&keepalive(3, [169, 254, 1, 7], 8)).is_none());
    }

    #[test]
    fn an_unknown_type_is_ignored() {
        let mut packet = keepalive(3, [169, 254, 1, 7], 1);
        packet[0x0a] = 0x01;
        assert!(parse_keepalive(&packet).is_none());
    }

    #[test]
    fn status_carries_the_source_deck_and_the_track() {
        let parsed = parse_status(&status(2, 1, 3, 1, 0x0102_0304, 3)).expect("a status");
        assert_eq!(parsed.player, 2);
        assert_eq!(parsed.source, 1);
        assert_eq!(parsed.slot, Slot::Usb);
        assert_eq!(parsed.analyze, Analyze::Rekordbox);
        assert_eq!(parsed.track_id, 0x0102_0304);
        assert_eq!(parsed.play, PlayState::Playing);
    }

    #[test]
    fn a_short_status_is_unreadable_rather_than_empty() {
        let mut packet = status(2, 1, 3, 1, 42, 3);
        packet.truncate(0x60);
        assert!(parse_status(&packet).is_none());
    }

    #[test]
    fn slots_keep_the_bytes_the_protocol_uses() {
        for (value, slot) in [(2, Slot::Sd), (3, Slot::Usb), (4, Slot::Other(4))] {
            assert_eq!(Slot::from_status(value), slot);
            assert_eq!(slot.as_u8(), value);
        }
        assert_eq!(Slot::Usb.export(), Some("/C/"));
        assert_eq!(Slot::Sd.export(), Some("/B/"));
        assert_eq!(Slot::Other(1).export(), None);
    }
}
