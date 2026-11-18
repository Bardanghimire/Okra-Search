//! Health state shared between the USN sync loop (writer) and the named-pipe server (reader).
//!
//! The launcher asks for this through the `OKS1` status request so it can tell "not running"
//! from "starting", "ready", or "stale" without guessing from a failed search.

use std::sync::atomic::{AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub const STATE_STARTING: u16 = 0;
pub const STATE_READY: u16 = 1;
pub const STATE_STOPPING: u16 = 2;

pub const STATUS_REQUEST_MAGIC: &[u8; 4] = b"OKS1";
pub const STATUS_RESPONSE_MAGIC: &[u8; 4] = b"OKT1";
/// magic(4) state(2) reserved(2) entries(4) pid(4) last_ok_unix(8) failures(4)
pub const STATUS_RESPONSE_LEN: usize = 28;

/// Search refusal codes carried in the existing `OKR1` response `status` field (0 = success,
/// 1 = invalid request / internal error). Any non-zero code makes the launcher fall through
/// to the next backend, so no launcher change is needed to honour them.
pub const REFUSED_STALE: u16 = 2;
pub const REFUSED_NOT_READY: u16 = 3;

/// If the sync loop has not completed a pass for this long, the snapshot may be missing recent
/// changes (hung journal read, repeated failures), so searches are refused instead of served.
pub const STALE_AFTER_SECS: u64 = 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Health {
    pub state: u16,
    pub entries: u32,
    pub last_ok_unix: u64,
    pub failures: u32,
}

pub struct HealthState {
    state: AtomicU16,
    entries: AtomicU32,
    last_ok_unix: AtomicU64,
    failures: AtomicU32,
}

impl HealthState {
    pub const fn new() -> Self {
        Self {
            state: AtomicU16::new(STATE_STARTING),
            entries: AtomicU32::new(0),
            last_ok_unix: AtomicU64::new(0),
            failures: AtomicU32::new(0),
        }
    }

    /// A synchronization pass finished successfully (even if it found no changes).
    pub fn mark_sync_ok(&self, now: u64) {
        self.last_ok_unix.store(now, Ordering::Release);
        self.failures.store(0, Ordering::Release);
        // Never leave STOPPING; only the first success moves STARTING -> READY.
        let _ = self.state.compare_exchange(
            STATE_STARTING,
            STATE_READY,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    pub fn mark_sync_failed(&self) {
        // Saturating without `fetch_update`, which newer toolchains deprecate and older ones
        // (this crate's MSRV is 1.80) do not yet offer under the new name.
        if self.failures.load(Ordering::Acquire) < u32::MAX {
            self.failures.fetch_add(1, Ordering::AcqRel);
        }
    }

    pub fn set_entries(&self, entries: usize) {
        self.entries
            .store(entries.min(u32::MAX as usize) as u32, Ordering::Release);
    }

    pub fn mark_stopping(&self) {
        self.state.store(STATE_STOPPING, Ordering::Release);
    }

    pub fn snapshot(&self) -> Health {
        Health {
            state: self.state.load(Ordering::Acquire),
            entries: self.entries.load(Ordering::Acquire),
            last_ok_unix: self.last_ok_unix.load(Ordering::Acquire),
            failures: self.failures.load(Ordering::Acquire),
        }
    }
}

static GLOBAL: HealthState = HealthState::new();

pub fn global() -> &'static HealthState {
    &GLOBAL
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `Ok(())` when the snapshot may be served; otherwise the refusal code to send back.
pub fn searchable(health: &Health, now: u64) -> Result<(), u16> {
    if health.state != STATE_READY {
        return Err(REFUSED_NOT_READY);
    }
    if now.saturating_sub(health.last_ok_unix) > STALE_AFTER_SECS {
        return Err(REFUSED_STALE);
    }
    Ok(())
}

pub fn encode_status(health: &Health, pid: u32) -> [u8; STATUS_RESPONSE_LEN] {
    let mut out = [0u8; STATUS_RESPONSE_LEN];
    out[..4].copy_from_slice(STATUS_RESPONSE_MAGIC);
    out[4..6].copy_from_slice(&health.state.to_le_bytes());
    // out[6..8] reserved (zero)
    out[8..12].copy_from_slice(&health.entries.to_le_bytes());
    out[12..16].copy_from_slice(&pid.to_le_bytes());
    out[16..24].copy_from_slice(&health.last_ok_unix.to_le_bytes());
    out[24..28].copy_from_slice(&health.failures.to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use okra_core::providers::files::{decode_helper_status, HelperStatus};

    #[test]
    fn starts_not_ready_and_becomes_ready_on_first_success() {
        let state = HealthState::new();
        assert_eq!(searchable(&state.snapshot(), 1000), Err(REFUSED_NOT_READY));
        state.mark_sync_ok(1000);
        assert_eq!(state.snapshot().state, STATE_READY);
        assert_eq!(searchable(&state.snapshot(), 1010), Ok(()));
    }

    #[test]
    fn stops_serving_when_the_sync_loop_goes_quiet() {
        let state = HealthState::new();
        state.mark_sync_ok(1000);
        assert_eq!(
            searchable(&state.snapshot(), 1000 + STALE_AFTER_SECS),
            Ok(())
        );
        assert_eq!(
            searchable(&state.snapshot(), 1001 + STALE_AFTER_SECS),
            Err(REFUSED_STALE)
        );
        state.mark_sync_ok(2000);
        assert_eq!(searchable(&state.snapshot(), 2001), Ok(()));
    }

    #[test]
    fn stopping_is_sticky_and_refuses_searches() {
        let state = HealthState::new();
        state.mark_sync_ok(1000);
        state.mark_stopping();
        state.mark_sync_ok(1001);
        assert_eq!(state.snapshot().state, STATE_STOPPING);
        assert_eq!(searchable(&state.snapshot(), 1002), Err(REFUSED_NOT_READY));
    }

    #[test]
    fn failures_count_up_and_reset_on_success() {
        let state = HealthState::new();
        state.mark_sync_ok(1);
        state.mark_sync_failed();
        state.mark_sync_failed();
        assert_eq!(state.snapshot().failures, 2);
        state.mark_sync_ok(2);
        assert_eq!(state.snapshot().failures, 0);
    }

    #[test]
    fn helper_encoding_round_trips_through_the_launcher_decoder() {
        let state = HealthState::new();
        state.mark_sync_ok(5000);
        state.set_entries(70_799);
        state.mark_sync_failed();
        let reply = encode_status(&state.snapshot(), 4242);
        assert_eq!(
            decode_helper_status(&reply, 5007),
            HelperStatus::Ready {
                entries: 70_799,
                last_sync_age_secs: 7,
                pid: 4242,
                failed_syncs: 1
            }
        );
        // Same reply, much later: the launcher classifies it as stale on its own clock too.
        assert_eq!(
            decode_helper_status(&reply, 5000 + STALE_AFTER_SECS + 5),
            HelperStatus::Stale {
                last_sync_age_secs: STALE_AFTER_SECS + 5
            }
        );
        state.mark_stopping();
        assert_eq!(
            decode_helper_status(&encode_status(&state.snapshot(), 1), 5001),
            HelperStatus::Stopping
        );
        assert_eq!(
            decode_helper_status(&encode_status(&HealthState::new().snapshot(), 1), 1),
            HelperStatus::Starting
        );
    }
}
