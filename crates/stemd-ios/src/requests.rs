//! What the server has answered, for the in-app debug screen.
//!
//! The Swift client records its own outgoing calls; this records the other
//! side, including the ones that never reach a handler (bad token, bad body).
//! Together they answer the question the debug screen exists for: is the app
//! talking, and is anyone else?

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use serde::Serialize;

/// Enough to see a deck's health probes and a few jobs, without the list
/// growing unbounded behind a screen nobody has open.
///
/// A deck polls twice a second, so 200 entries was under two minutes of
/// history: a failed upload was gone before anyone could read it. A thousand
/// covers a test run and costs a few hundred kilobytes.
const LIMIT: usize = 1000;

#[derive(Clone, Debug, Serialize)]
pub struct Entry {
    pub seq: u64,
    /// Unix seconds, which is what `Date(timeIntervalSince1970:)` wants.
    pub at: f64,
    pub method: String,
    pub path: String,
    pub status: u16,
    pub millis: u64,
    pub remote: String,
    /// True for the app's own requests, which arrive over loopback. The debug
    /// screen offers to hide those so a deck on the LAN stands out.
    pub loopback: bool,
}

pub struct RequestLog {
    entries: Mutex<VecDeque<Entry>>,
    next: AtomicU64,
}

impl RequestLog {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(VecDeque::with_capacity(LIMIT)),
            next: AtomicU64::new(0),
        }
    }

    pub fn record(
        &self,
        method: &str,
        path: &str,
        status: u16,
        elapsed: Duration,
        remote: SocketAddr,
    ) {
        let seq = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let entry = Entry {
            seq,
            at,
            method: method.to_owned(),
            path: mask(path),
            status,
            millis: elapsed.as_millis() as u64,
            remote: remote.ip().to_string(),
            loopback: remote.ip().is_loopback(),
        };
        let mut entries = self.entries.lock();
        if entries.len() == LIMIT {
            entries.pop_front();
        }
        entries.push_back(entry);
    }

    pub fn recent(&self) -> Vec<Entry> {
        self.entries.lock().iter().cloned().collect()
    }
}

impl Default for RequestLog {
    fn default() -> Self {
        Self::new()
    }
}

/// The token is a secret even in a debug list, and the query spelling puts it
/// in the path.
fn mask(path_and_query: &str) -> String {
    let Some((path, query)) = path_and_query.split_once('?') else {
        return path_and_query.to_owned();
    };
    let masked: Vec<&str> = query
        .split('&')
        .map(|pair| {
            if pair.starts_with("token=") {
                "token=…"
            } else {
                pair
            }
        })
        .collect();
    format!("{path}?{}", masked.join("&"))
}
