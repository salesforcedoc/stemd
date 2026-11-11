//! Announcing this host as a virtual CDJ.
//!
//! Decks publish what they have loaded only in status packets, and they unicast
//! those to peers that have announced themselves on UDP 50000. A bare status
//! keepalive is enough for that; the longer hello/claim handshake buys
//! browsability and dbserver access, which this monitor does not need.
//!
//! Number 7 is deliberate. A real player is 1–6, so this can never collide with
//! hardware, and it is above 4, so no deck will try to browse us. `--prolink-deck-number`
//! lowers it for a rig that insists on the range python-prodj-link uses (5).

use std::net::Ipv4Addr;

use super::packets::MAGIC;

/// Length of the announcement packet.
pub const LENGTH: usize = 0x36;

/// A stable locally-administered MAC for the virtual player.
///
/// Nothing here reads the host's real MAC: the field identifies a monitor, not a
/// network adapter, and a duplicate would only matter to another monitor. The
/// locally-administered bit keeps it out of any vendor's space.
pub const fn synthetic_mac(player: u8) -> [u8; 6] {
    [0x02, 0x00, 0x00, 0x00, 0x00, player]
}

/// Build the `type_status` keepalive a deck must hear before it will unicast
/// status to us.
pub fn announcement(player: u8, model: &str, address: Ipv4Addr, mac: [u8; 6]) -> [u8; LENGTH] {
    let mut packet = [0u8; LENGTH];
    packet[..MAGIC.len()].copy_from_slice(MAGIC);
    packet[0x0a] = 0x06; // type_status

    // A twenty-byte NUL-padded model name. Non-ASCII cannot be expressed here;
    // the field is bytes and every real model is ASCII.
    for (slot, byte) in packet[0x0c..0x20].iter_mut().zip(model.bytes()) {
        *slot = byte;
    }

    packet[0x20] = 1; // u1
    packet[0x21] = 2; // device type: CDJ
    packet[0x23] = LENGTH as u8; // subtype equals the packet length
    packet[0x24] = player;
    packet[0x25] = 1; // u2
    packet[0x26..0x2c].copy_from_slice(&mac);
    packet[0x2c..0x30].copy_from_slice(&address.octets());
    packet[0x30] = 2; // device_count; 2 is what CDJ-3000s expect
    packet[0x34] = 1; // player, not mixer / rekordbox
    packet[0x35] = 0x64; // required to coexist with CDJ-3000s
    packet
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_announcement_names_us_and_carries_the_cdj3000_byte() {
        let ip = Ipv4Addr::new(169, 254, 4, 9);
        let packet = announcement(7, "Virtual CDJ", ip, synthetic_mac(7));
        assert_eq!(packet.len(), 0x36);
        assert_eq!(&packet[..10], MAGIC);
        assert_eq!(packet[0x0a], 0x06);
        assert_eq!(&packet[0x0c..0x0c + 11], b"Virtual CDJ");
        assert_eq!(packet[0x21], 2);
        assert_eq!(packet[0x23], 0x36);
        assert_eq!(packet[0x24], 7);
        assert_eq!(&packet[0x26..0x2c], &[0x02, 0, 0, 0, 0, 7]);
        assert_eq!(&packet[0x2c..0x30], &ip.octets());
        assert_eq!(packet[0x35], 0x64);
    }

    #[test]
    fn the_model_field_is_truncated_rather_than_overrunning() {
        let packet = announcement(
            7,
            "a model name far longer than twenty bytes",
            Ipv4Addr::LOCALHOST,
            synthetic_mac(7),
        );
        assert_eq!(&packet[0x0c..0x20], b"a model name far lon");
    }

    #[test]
    fn the_synthetic_mac_is_locally_administered_and_unique_per_number() {
        assert_eq!(synthetic_mac(7)[0] & 0x02, 0x02);
        assert_ne!(synthetic_mac(7), synthetic_mac(5));
    }
}
