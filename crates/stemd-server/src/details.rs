//! Song details attached to a cached separation.
//!
//! The cache key is derived from audio, so on its own it says nothing about
//! what was separated. These fields are what `/v1/jobs` callers, the window's
//! drop path and the ProLink monitor can say about the source, and they are
//! written beside the stems so the cache can be browsed without the server.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// What is known about the source of a separation.
///
/// Every field is optional: an HTTP client may send none, a drop knows the file
/// name, and the ProLink monitor knows the whole rekordbox row.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TrackDetails {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    /// `prolink`, `drop` or `http`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The file on this machine that was separated, when there was one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_file: Option<PathBuf>,
    /// The path on the deck's medium, for a ProLink track.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub medium_path: Option<PathBuf>,
    /// The player whose medium holds the track. On ProLink a deck can play
    /// another player's medium, so this is not always the deck showing it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deck: Option<u8>,
    /// The medium slot: 2 is SD, 3 is USB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<u8>,
    /// The rekordbox track id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track_id: Option<u32>,
}

impl TrackDetails {
    /// Trim and bound the strings taken from a request.
    ///
    /// The detail file is meant to be read by a person, not to be a place for a
    /// client to park a megabyte.
    pub fn clip(&mut self) {
        for field in [&mut self.title, &mut self.artist, &mut self.album] {
            if let Some(text) = field {
                let trimmed = text.trim();
                *field = if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.chars().take(256).collect())
                };
            }
        }
    }
}
