//! Persistent NTFS index state and USN Change Journal synchronization.
//!
//! Stage 2 keeps the launcher untouched. The elevated helper creates an MFT baseline, replays
//! changes that occurred during that scan, and on later runs applies only journal records newer
//! than the saved cursor. `--watch` keeps this helper process alive and polls the journal.

use crate::mft::{self, MftEntry};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::Duration;

const STATE_MAGIC: &[u8; 8] = b"OKRAUSN1";
const STATE_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const STATE_MAX_ENTRIES: u64 = 100_000_000;
const STATE_MAX_STRING_BYTES: usize = 1024 * 1024;

const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
const USN_REASON_FILE_DELETE: u32 = 0x0000_0200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct JournalInfo {
    journal_id: u64,
    first_usn: i64,
    next_usn: i64,
    lowest_valid_usn: i64,
}

#[derive(Debug, Clone)]
struct UsnChange {
    file_reference: u64,
    parent_reference: u64,
    usn: i64,
    reason: u32,
    name: String,
    is_dir: bool,
}

#[derive(Debug)]
struct IndexState {
    volume: String,
    roots: Vec<String>,
    max_depth: usize,
    journal_id: u64,
    cursor: i64,
    entries: Vec<MftEntry>,
}

#[derive(Debug, Default, Clone, Copy)]
struct SyncStats {
    changes_read: usize,
    cursor_before: i64,
    cursor_after: i64,
    rebuilt: bool,
}

fn read_u16(bytes: &[u8], at: usize) -> Result<u16, String> {
    let value = bytes
        .get(at..at + 2)
        .ok_or_else(|| "Truncated USN record field".to_string())?;
    Ok(u16::from_le_bytes([value[0], value[1]]))
}

fn read_u32(bytes: &[u8], at: usize) -> Result<u32, String> {
    let value = bytes
        .get(at..at + 4)
        .ok_or_else(|| "Truncated USN record field".to_string())?;
    Ok(u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
}

fn read_u64(bytes: &[u8], at: usize) -> Result<u64, String> {
    let value = bytes
        .get(at..at + 8)
        .ok_or_else(|| "Truncated USN record field".to_string())?;
    Ok(u64::from_le_bytes([
        value[0], value[1], value[2], value[3], value[4], value[5], value[6], value[7],
    ]))
}

/// Parse FSCTL_READ_USN_JOURNAL output: an eight-byte next-USN value followed by records.
/// This NTFS helper currently supports USN_RECORD_V2 only. Unknown versions fail closed so the
/// caller never advances its persistent cursor past changes it did not understand.
fn parse_usn_buffer(buffer: &[u8]) -> Result<(i64, Vec<UsnChange>), String> {
    if buffer.len() < 8 {
        return Err("FSCTL_READ_USN_JOURNAL returned a truncated next-USN value".into());
    }
    let next_usn = read_u64(buffer, 0)? as i64;
    if next_usn < 0 {
        return Err("USN journal returned a negative next-USN value".into());
    }
    let mut changes = Vec::new();
    let mut previous_usn: Option<i64> = None;
    let mut offset = 8usize;

    while offset < buffer.len() {
        if buffer.len() - offset < 8 {
            return Err("Truncated USN journal record header".into());
        }
        let record_length = read_u32(buffer, offset)? as usize;
        if record_length < 8 || record_length > buffer.len() - offset {
            return Err(format!("Invalid USN journal record length {record_length}"));
        }
        let major = read_u16(buffer, offset + 4)?;
        if major == 2 {
            if record_length < 60 {
                return Err(format!("USN_RECORD_V2 is too short ({record_length} bytes)"));
            }
            let file_reference = read_u64(buffer, offset + 8)?;
            let parent_reference = read_u64(buffer, offset + 16)?;
            let usn = read_u64(buffer, offset + 24)? as i64;
            if usn < 0 || previous_usn.is_some_and(|previous| usn < previous) {
                return Err("USN journal records are not in valid ascending USN order".into());
            }
            previous_usn = Some(usn);
            let reason = read_u32(buffer, offset + 40)?;
            let attributes = read_u32(buffer, offset + 52)?;
            let name_length = read_u16(buffer, offset + 56)? as usize;
            let name_offset = read_u16(buffer, offset + 58)? as usize;
            if name_length % 2 != 0 || (name_length > 0 && name_offset < 60) {
                return Err("Invalid UTF-16 filename fields in USN journal record".into());
            }
            let name_end = name_offset
                .checked_add(name_length)
                .ok_or_else(|| "USN journal filename range overflow".to_string())?;
            if name_length > 0 && name_end > record_length {
                return Err("USN journal filename extends beyond its record".into());
            }
            let name_bytes = if name_length == 0 {
                &buffer[offset + 60..offset + 60]
            } else {
                &buffer[offset + name_offset..offset + name_end]
            };
            let wide: Vec<u16> = name_bytes
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect();
            changes.push(UsnChange {
                file_reference,
                parent_reference,
                usn,
                reason,
                name: String::from_utf16_lossy(&wide),
                is_dir: attributes & FILE_ATTRIBUTE_DIRECTORY != 0,
            });
        } else {
            return Err(format!(
                "Unsupported USN journal record version {major}; refusing to advance the index cursor"
            ));
        }
        offset += record_length;
    }

    Ok((next_usn, changes))
}

fn normalized_roots(roots: &[String]) -> Vec<String> {
    let mut normalized: Vec<String> = roots.iter().map(|root| root.to_lowercase()).collect();
    normalized.sort();
    normalized
}

fn config_matches(state: &IndexState, volume: &str, roots: &[String], max_depth: usize) -> bool {
    state.volume.eq_ignore_ascii_case(volume)
        && normalized_roots(&state.roots) == normalized_roots(roots)
        && state.max_depth == max_depth
}

fn journal_resume_problem(state: &IndexState, journal: JournalInfo) -> Option<String> {
    if state.journal_id != journal.journal_id {
        return Some(format!(
            "USN journal ID changed (saved {}, current {})",
            state.journal_id, journal.journal_id
        ));
    }
    if state.cursor < journal.first_usn {
        return Some(format!(
            "saved USN cursor {} has expired; journal FirstUsn is {}",
            state.cursor, journal.first_usn
        ));
    }
    if state.cursor < journal.lowest_valid_usn {
        return Some(format!(
            "saved USN cursor {} predates the journal's LowestValidUsn {}",
            state.cursor, journal.lowest_valid_usn
        ));
    }
    if state.cursor > journal.next_usn {
        return Some(format!(
            "saved USN cursor {} is ahead of journal NextUsn {}",
            state.cursor, journal.next_usn
        ));
    }
    None
}

/// A mutable FRN map that preserves directory parent references. This means a directory rename
/// updates one record; descendants resolve through the updated parent when the TSV is exported.
struct MutableIndex {
    entries: Vec<Option<MftEntry>>,
    by_reference: HashMap<u64, usize>,
}

impl MutableIndex {
    fn new(entries: Vec<MftEntry>) -> Self {
        let mut index = Self {
            entries: Vec::with_capacity(entries.len()),
            by_reference: HashMap::with_capacity(entries.len()),
        };
        for entry in entries {
            if let Some(existing) = index.by_reference.get(&entry.file_reference).copied() {
                index.entries[existing] = Some(entry);
            } else {
                let position = index.entries.len();
                index.by_reference.insert(entry.file_reference, position);
                index.entries.push(Some(entry));
            }
        }
        index
    }

    fn apply(&mut self, change: UsnChange) {
        if change.reason & USN_REASON_FILE_DELETE != 0 {
            if let Some(position) = self.by_reference.remove(&change.file_reference) {
                self.entries[position] = None;
            }
            return;
        }

        // Some journal records (for example, close-only notifications) can omit a useful name.
        // They do not alter the namespace graph and should not erase a known name.
        if change.name.is_empty() {
            return;
        }

        let entry = MftEntry {
            file_reference: change.file_reference,
            parent_reference: change.parent_reference,
            name: change.name,
            is_dir: change.is_dir,
        };
        if let Some(position) = self.by_reference.get(&entry.file_reference).copied() {
            self.entries[position] = Some(entry);
        } else {
            let position = self.entries.len();
            self.by_reference.insert(entry.file_reference, position);
            self.entries.push(Some(entry));
        }
    }

    fn finish(self) -> Vec<MftEntry> {
        let mut entries: Vec<MftEntry> = self.entries.into_iter().flatten().collect();
        entries.sort_unstable_by_key(|entry| entry.file_reference);
        entries
    }
}

fn apply_changes(index: &mut MutableIndex, changes: Vec<UsnChange>) -> usize {
    let count = changes.len();
    for change in changes {
        // The USN value is parsed for diagnostics/ordering validation. The journal API returns
        // records in USN order, and the latest record for a file reference wins.
        let _record_usn = change.usn;
        index.apply(change);
    }
    count
}

fn encode_state(state: &IndexState) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(STATE_MAGIC);
    put_string(&mut bytes, &state.volume)?;
    put_u32(
        &mut bytes,
        u32::try_from(state.roots.len()).map_err(|_| "Too many configured roots".to_string())?,
    );
    for root in &state.roots {
        put_string(&mut bytes, root)?;
    }
    put_u64(&mut bytes, state.max_depth as u64);
    put_u64(&mut bytes, state.journal_id);
    put_u64(&mut bytes, state.cursor as u64);
    put_u64(&mut bytes, state.entries.len() as u64);
    for entry in &state.entries {
        put_u64(&mut bytes, entry.file_reference);
        put_u64(&mut bytes, entry.parent_reference);
        bytes.push(u8::from(entry.is_dir));
        put_string(&mut bytes, &entry.name)?;
    }
    if bytes.len() as u64 > STATE_MAX_BYTES {
        return Err(format!(
            "Persistent index would be {} bytes, larger than the {}-byte safety limit",
            bytes.len(), STATE_MAX_BYTES
        ));
    }
    Ok(bytes)
}

fn put_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn put_string(bytes: &mut Vec<u8>, value: &str) -> Result<(), String> {
    if value.len() > STATE_MAX_STRING_BYTES {
        return Err("A persistent index string exceeds the safety limit".into());
    }
    put_u32(
        bytes,
        u32::try_from(value.len()).map_err(|_| "Persistent index string is too long".to_string())?,
    );
    bytes.extend_from_slice(value.as_bytes());
    Ok(())
}

struct StateReader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> StateReader<'a> {
    fn take(&mut self, length: usize) -> Result<&'a [u8], String> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or_else(|| "Persistent index offset overflow".to_string())?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| "Persistent index file is truncated".to_string())?;
        self.offset = end;
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, String> {
        let value = self.take(4)?;
        Ok(u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
    }

    fn u64(&mut self) -> Result<u64, String> {
        let value = self.take(8)?;
        Ok(u64::from_le_bytes([
            value[0], value[1], value[2], value[3], value[4], value[5], value[6], value[7],
        ]))
    }

    fn string(&mut self) -> Result<String, String> {
        let length = self.u32()? as usize;
        if length > STATE_MAX_STRING_BYTES {
            return Err("Persistent index string exceeds the safety limit".into());
        }
        String::from_utf8(self.take(length)?.to_vec())
            .map_err(|_| "Persistent index contains invalid UTF-8".to_string())
    }
}

fn decode_state(bytes: &[u8]) -> Result<IndexState, String> {
    if bytes.len() < STATE_MAGIC.len() || &bytes[..STATE_MAGIC.len()] != STATE_MAGIC {
        return Err("Persistent index has an unknown file signature".into());
    }
    let mut reader = StateReader {
        bytes,
        offset: STATE_MAGIC.len(),
    };
    let volume = reader.string()?;
    let root_count = reader.u32()? as usize;
    if root_count == 0 || root_count > 4096 {
        return Err("Persistent index has an invalid configured-root count".into());
    }
    let mut roots = Vec::with_capacity(root_count);
    for _ in 0..root_count {
        roots.push(reader.string()?);
    }
    let max_depth = usize::try_from(reader.u64()?)
        .map_err(|_| "Persistent index depth does not fit this platform".to_string())?;
    let journal_id = reader.u64()?;
    let cursor = reader.u64()? as i64;
    let entry_count = reader.u64()?;
    if entry_count > STATE_MAX_ENTRIES {
        return Err("Persistent index contains an unreasonable number of MFT entries".into());
    }
    // Each encoded entry needs at least 21 bytes (two u64 references, a directory byte, and a
    // u32 string length). Check this before reserving memory so a tiny corrupt state file cannot
    // claim a huge entry count and trigger an excessive allocation.
    let remaining = bytes.len().saturating_sub(reader.offset);
    let maximum_possible_entries = (remaining / 21) as u64;
    if entry_count > maximum_possible_entries {
        return Err("Persistent index entry count exceeds the remaining file data".into());
    }
    let entry_capacity = usize::try_from(entry_count)
        .map_err(|_| "Persistent index entry count does not fit this platform".to_string())?;
    let mut entries = Vec::with_capacity(entry_capacity);
    for _ in 0..entry_count {
        let file_reference = reader.u64()?;
        let parent_reference = reader.u64()?;
        let is_dir = match reader.u8()? {
            0 => false,
            1 => true,
            _ => return Err("Persistent index contains an invalid directory flag".into()),
        };
        let name = reader.string()?;
        entries.push(MftEntry {
            file_reference,
            parent_reference,
            name,
            is_dir,
        });
    }
    if reader.offset != bytes.len() {
        return Err("Persistent index contains unexpected trailing data".into());
    }
    Ok(IndexState {
        volume,
        roots,
        max_depth,
        journal_id,
        cursor,
        entries,
    })
}

fn load_state(path: &Path) -> Result<Option<IndexState>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let metadata = fs::metadata(path)
        .map_err(|error| format!("Could not inspect persistent index: {error}"))?;
    if metadata.len() > STATE_MAX_BYTES {
        return Err(format!(
            "Persistent index is {} bytes, larger than the {}-byte safety limit",
            metadata.len(), STATE_MAX_BYTES
        ));
    }
    let bytes = fs::read(path).map_err(|error| format!("Could not read persistent index: {error}"))?;
    decode_state(&bytes).map(Some)
}

fn write_bytes_atomically(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Could not create state folder: {error}"))?;
    }
    let temporary = path.with_extension("state.tmp");
    fs::write(&temporary, bytes)
        .map_err(|error| format!("Could not write temporary state file: {error}"))?;
    crate::atomic::replace(&temporary, path)
        .map_err(|error| format!("Could not replace persistent state file: {error}"))
}

fn persist_outputs(state: &IndexState, snapshot_path: &Path, state_path: &Path) -> Result<usize, String> {
    let snapshot = mft::make_cache_text(
        &state.entries,
        &state.roots,
        state.max_depth,
        &state.volume,
    );
    let count = snapshot.lines().skip(1).count();
    let state_bytes = encode_state(state)?;
    // Write the derived cache first. If saving the cursor subsequently fails, replaying the same
    // journal records on the next run is safe and idempotent; saving the cursor first could leave
    // a stale cache if the process stopped between the two writes.
    mft::write_atomically(snapshot_path, &snapshot)?;
    write_bytes_atomically(state_path, &state_bytes)?;
    Ok(count)
}

fn rebuild_state(
    volume: &str,
    roots: &[String],
    max_depth: usize,
) -> Result<(IndexState, usize), String> {
    for attempt in 1..=3 {
        let before = native::query_journal(volume)?;
        eprintln!(
            "Building an MFT baseline (attempt {attempt}/3); journal ID {}, cursor {} ...",
            before.journal_id, before.next_usn
        );
        let entries = mft::enumerate(volume)?;
        mft::log_path_diagnostics(&entries, roots, max_depth, volume);
        let after = native::query_journal(volume)?;

        if before.journal_id != after.journal_id
            || before.next_usn < after.first_usn
            || before.next_usn < after.lowest_valid_usn
        {
            eprintln!("The USN journal changed or wrapped during the MFT scan; retrying baseline.");
            continue;
        }

        let mut index = MutableIndex::new(entries);
        let replay = native::read_changes(
            volume,
            before.next_usn,
            after.journal_id,
            after.next_usn,
            |batch| {
                apply_changes(&mut index, batch);
            },
        );
        let (cursor, changes_read) = match replay {
            Ok(result) => result,
            Err(error) => {
                let latest = native::query_journal(volume)?;
                if latest.journal_id != after.journal_id
                    || before.next_usn < latest.first_usn
                    || before.next_usn < latest.lowest_valid_usn
                {
                    eprintln!("The journal rolled over during initial replay ({error}); retrying baseline.");
                    continue;
                }
                return Err(error);
            }
        };
        let verify = native::query_journal(volume)?;
        if verify.journal_id != after.journal_id
            || cursor < verify.first_usn
            || cursor < verify.lowest_valid_usn
            || cursor > verify.next_usn
        {
            eprintln!("The journal changed while replaying the initial scan; retrying baseline.");
            continue;
        }

        let state = IndexState {
            volume: volume.to_string(),
            roots: roots.to_vec(),
            max_depth,
            journal_id: verify.journal_id,
            cursor,
            entries: index.finish(),
        };
        return Ok((state, changes_read));
    }
    Err("The USN journal changed repeatedly during the MFT baseline. Run the helper again.".into())
}

fn sync_once(
    volume: &str,
    roots: &[String],
    max_depth: usize,
    state_path: &Path,
) -> Result<(IndexState, SyncStats), String> {
    let journal = native::query_journal(volume)?;
    let loaded = match load_state(state_path) {
        Ok(state) => state,
        Err(error) => {
            eprintln!("Persistent index cannot be resumed ({error}); rebuilding from the MFT.");
            None
        }
    };

    if let Some(state) = loaded {
        let reason = if !config_matches(&state, volume, roots, max_depth) {
            Some("volume, configured roots, or maximum depth changed".to_string())
        } else {
            journal_resume_problem(&state, journal)
        };
        if let Some(reason) = reason {
            eprintln!("Cannot resume the saved index: {reason}. Rebuilding from the MFT.");
        } else {
            let cursor_before = state.cursor;
            let mut index = MutableIndex::new(state.entries);
            let (cursor_after, changes_read) = match native::read_changes(
                volume,
                cursor_before,
                journal.journal_id,
                journal.next_usn,
                |batch| {
                    apply_changes(&mut index, batch);
                },
            ) {
                Ok(result) => result,
                Err(error) => {
                    let latest = native::query_journal(volume)?;
                    if latest.journal_id == journal.journal_id
                        && cursor_before >= latest.first_usn
                        && cursor_before >= latest.lowest_valid_usn
                        && cursor_before <= latest.next_usn
                    {
                        return Err(error);
                    }
                    eprintln!("The saved journal position expired while reading; rebuilding from the MFT.");
                    return rebuild_with_stats(volume, roots, max_depth, cursor_before);
                }
            };

            let latest = native::query_journal(volume)?;
            if latest.journal_id == journal.journal_id
                && cursor_after >= latest.first_usn
                && cursor_after >= latest.lowest_valid_usn
                && cursor_after <= latest.next_usn
            {
                let updated = IndexState {
                    volume: state.volume,
                    roots: state.roots,
                    max_depth: state.max_depth,
                    journal_id: latest.journal_id,
                    cursor: cursor_after,
                    entries: index.finish(),
                };
                return Ok((
                    updated,
                    SyncStats {
                        changes_read,
                        cursor_before,
                        cursor_after,
                        rebuilt: false,
                    },
                ));
            }
            eprintln!("The USN journal changed during synchronization; rebuilding from the MFT.");
        }
    }

    rebuild_with_stats(volume, roots, max_depth, 0)
}

fn rebuild_with_stats(
    volume: &str,
    roots: &[String],
    max_depth: usize,
    old_cursor: i64,
) -> Result<(IndexState, SyncStats), String> {
    let (state, changes_read) = rebuild_state(volume, roots, max_depth)?;
    let cursor_after = state.cursor;
    Ok((
        state,
        SyncStats {
            changes_read,
            cursor_before: old_cursor,
            cursor_after,
            rebuilt: true,
        },
    ))
}

/// Synchronize one volume. By default the helper exits after one sync; `--watch` keeps it alive
/// and polls for new USN records at the configured interval.
pub(super) fn run(
    volume: &str,
    roots: &[String],
    max_depth: usize,
    snapshot_path: &Path,
    state_path: &Path,
    watch: bool,
    poll_seconds: u64,
) -> Result<(), String> {
    mft::validate_volume_and_roots(volume, roots)?;
    let mut initialized = false;
    let health = super::status::global();
    let shutdown = super::shutdown::global();

    // A journal cursor is only advanced after its changes were applied and both output files
    // were written (see `persist_outputs`), so stopping between passes never loses changes.
    while !shutdown.requested() {
        match sync_once(volume, roots, max_depth, state_path) {
            Ok((state, stats)) => {
                let has_outputs = snapshot_path.exists() && state_path.exists();
                let changed = stats.rebuilt
                    || stats.changes_read > 0
                    || stats.cursor_before != stats.cursor_after
                    || !has_outputs;
                let mut outputs_current = true;
                if changed {
                    match persist_outputs(&state, snapshot_path, state_path) {
                        Ok(indexed) => {
                            health.set_entries(indexed);
                            if stats.rebuilt {
                                println!(
                                    "Built NTFS index: {} MFT entries, {} journal changes replayed, {} paths exported.",
                                    state.entries.len(), stats.changes_read, indexed
                                );
                            } else {
                                println!(
                                    "Applied {} USN journal records (cursor {} -> {}); {} paths exported.",
                                    stats.changes_read, stats.cursor_before, stats.cursor_after, indexed
                                );
                            }
                        }
                        Err(error) if watch && initialized => {
                            // Do not advance the persisted cursor when either output could not
                            // be replaced. The next pass reloads the old state and safely replays
                            // these journal records; this is preferable to killing a healthy
                            // logon helper for a transient Windows sharing conflict.
                            outputs_current = false;
                            health.mark_sync_failed();
                            eprintln!(
                                "Could not persist the NTFS index: {error}. Retrying in {poll_seconds}s."
                            );
                        }
                        Err(error) => return Err(error),
                    }
                } else {
                    if health.snapshot().entries == 0 {
                        // Resumed from saved files with nothing new: report the size of the
                        // snapshot that is already on disk (header line excluded).
                        health.set_entries(count_snapshot_paths(snapshot_path));
                    }
                    if !watch {
                        println!(
                            "NTFS index is up to date at USN {}; no changes to apply.",
                            state.cursor
                        );
                    }
                }
                // Only now is the on-disk snapshot known to be current, so only now may the
                // pipe serve it (the first success flips the helper from starting to ready).
                if outputs_current {
                    health.mark_sync_ok(super::status::now_unix());
                }
            }
            Err(error) if watch && initialized => {
                health.mark_sync_failed();
                eprintln!("USN synchronization failed: {error}. Retrying in {poll_seconds}s.");
            }
            // Before the first success there is nothing to serve; exit non-zero so Task
            // Scheduler's restart-on-failure policy retries instead of idling unhealthy.
            Err(error) => return Err(error),
        }

        initialized = true;
        if !watch {
            return Ok(());
        }
        if shutdown.sleep(Duration::from_secs(poll_seconds)) {
            break;
        }
    }
    Ok(())
}

/// Number of exported paths in an existing snapshot (every line after the header).
fn count_snapshot_paths(path: &Path) -> usize {
    fs::read(path)
        .map(|bytes| bytes.iter().filter(|b| **b == b'\n').count().saturating_sub(1))
        .unwrap_or(0)
}

#[cfg(windows)]
mod native {
    use super::{parse_usn_buffer, JournalInfo, UsnChange};
    use std::ffi::c_void;
    use std::mem::{size_of, MaybeUninit};
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;

    const GENERIC_READ: u32 = 0x8000_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const FILE_SHARE_DELETE: u32 = 0x0000_0004;
    const OPEN_EXISTING: u32 = 3;
    const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;
    const FSCTL_QUERY_USN_JOURNAL: u32 = 0x0009_00F4;
    const FSCTL_READ_USN_JOURNAL: u32 = 0x0009_00BB;
    const ERROR_HANDLE_EOF: u32 = 38;
    const ERROR_MORE_DATA: u32 = 234;
    const ERROR_JOURNAL_NOT_ACTIVE: u32 = 1179;
    const ERROR_JOURNAL_ENTRY_DELETED: u32 = 1181;
    const INVALID_HANDLE_VALUE: *mut c_void = -1isize as *mut c_void;

    #[repr(C)]
    // V2 is the largest currently documented query result. V0/V1 use the same first 56 bytes,
    // so zero-initializing this buffer also safely accepts those older result versions.
    struct UsnJournalDataV2 {
        journal_id: u64,
        first_usn: i64,
        next_usn: i64,
        lowest_valid_usn: i64,
        max_usn: i64,
        maximum_size: u64,
        allocation_delta: u64,
        min_supported_major_version: u16,
        max_supported_major_version: u16,
        flags: u32,
        range_track_chunk_size: u64,
        range_track_file_size_threshold: i64,
    }

    #[repr(C)]
    struct ReadUsnJournalDataV0 {
        start_usn: i64,
        reason_mask: u32,
        return_only_on_close: u32,
        timeout: u64,
        bytes_to_wait_for: u64,
        usn_journal_id: u64,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateFileW(
            file_name: *const u16,
            desired_access: u32,
            share_mode: u32,
            security_attributes: *mut c_void,
            creation_disposition: u32,
            flags_and_attributes: u32,
            template_file: *mut c_void,
        ) -> *mut c_void;
        fn DeviceIoControl(
            device: *mut c_void,
            control_code: u32,
            input_buffer: *mut c_void,
            input_size: u32,
            output_buffer: *mut c_void,
            output_size: u32,
            bytes_returned: *mut u32,
            overlapped: *mut c_void,
        ) -> i32;
        fn CloseHandle(handle: *mut c_void) -> i32;
        fn GetLastError() -> u32;
    }

    struct VolumeHandle(*mut c_void);

    impl Drop for VolumeHandle {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    fn open_volume(volume: &str) -> Result<VolumeHandle, String> {
        let device_path = format!(r"\\.\{}", volume);
        let wide: Vec<u16> = std::ffi::OsStr::new(&device_path)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                ptr::null_mut(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            let code = unsafe { GetLastError() };
            return Err(format!(
                "Could not open {volume} as an NTFS volume (Windows error {code}). Run the helper elevated and verify the volume is NTFS."
            ));
        }
        Ok(VolumeHandle(handle))
    }

    pub fn query_journal(volume: &str) -> Result<JournalInfo, String> {
        let handle = open_volume(volume)?;
        let mut output = MaybeUninit::<UsnJournalDataV2>::zeroed();
        let mut returned = 0u32;
        let succeeded = unsafe {
            DeviceIoControl(
                handle.0,
                FSCTL_QUERY_USN_JOURNAL,
                ptr::null_mut(),
                0,
                output.as_mut_ptr() as *mut c_void,
                size_of::<UsnJournalDataV2>() as u32,
                &mut returned,
                ptr::null_mut(),
            )
        };
        if succeeded == 0 {
            let code = unsafe { GetLastError() };
            if code == ERROR_JOURNAL_NOT_ACTIVE {
                return Err(format!(
                    "The USN Change Journal is not active on {volume}. Check it with `fsutil usn queryjournal {volume}`. To enable it explicitly, consult the `fsutil usn createjournal` documentation and choose suitable max-size/allocation-delta values (both are bytes) before running it from Administrator PowerShell."
                ));
            }
            return Err(format!(
                "FSCTL_QUERY_USN_JOURNAL failed on {volume} (Windows error {code})"
            ));
        }
        if returned < 56 {
            return Err(format!(
                "FSCTL_QUERY_USN_JOURNAL returned only {returned} bytes; expected at least {}",
                56
            ));
        }
        let data = unsafe { output.assume_init() };
        Ok(JournalInfo {
            journal_id: data.journal_id,
            first_usn: data.first_usn,
            next_usn: data.next_usn,
            lowest_valid_usn: data.lowest_valid_usn,
        })
    }

    pub fn read_changes<F>(
        volume: &str,
        start_usn: i64,
        journal_id: u64,
        target_next_usn: i64,
        mut on_batch: F,
    ) -> Result<(i64, usize), String>
    where
        F: FnMut(Vec<UsnChange>),
    {
        if start_usn < 0 || target_next_usn < 0 || start_usn > target_next_usn {
            return Err("Invalid USN journal cursor range".into());
        }
        let handle = open_volume(volume)?;
        let mut cursor = start_usn;
        let mut total_records = 0usize;
        let mut buffer = vec![0u8; 1024 * 1024];

        while cursor < target_next_usn {
            let mut input = ReadUsnJournalDataV0 {
                start_usn: cursor,
                reason_mask: u32::MAX,
                return_only_on_close: 0,
                timeout: 0,
                bytes_to_wait_for: 0,
                usn_journal_id: journal_id,
            };
            let returned = loop {
                let mut bytes_returned = 0u32;
                let succeeded = unsafe {
                    DeviceIoControl(
                        handle.0,
                        FSCTL_READ_USN_JOURNAL,
                        &mut input as *mut ReadUsnJournalDataV0 as *mut c_void,
                        size_of::<ReadUsnJournalDataV0>() as u32,
                        buffer.as_mut_ptr() as *mut c_void,
                        buffer.len() as u32,
                        &mut bytes_returned,
                        ptr::null_mut(),
                    )
                };
                if succeeded != 0 {
                    break bytes_returned;
                }

                let code = unsafe { GetLastError() };
                if code == ERROR_MORE_DATA && buffer.len() < 64 * 1024 * 1024 {
                    // Retry the same StartUsn with a larger buffer; do not consume a partial page.
                    buffer.resize((buffer.len() * 2).min(64 * 1024 * 1024), 0);
                    continue;
                }
                if code == ERROR_JOURNAL_ENTRY_DELETED {
                    return Err(format!(
                        "The saved USN cursor {cursor} is no longer present in the journal on {volume}"
                    ));
                }
                if code == ERROR_HANDLE_EOF {
                    return Err(format!(
                        "FSCTL_READ_USN_JOURNAL reached end-of-file before target USN {target_next_usn}"
                    ));
                }
                return Err(format!(
                    "FSCTL_READ_USN_JOURNAL failed at USN {cursor} (Windows error {code})"
                ));
            };
            if returned < 8 || returned as usize > buffer.len() {
                return Err(format!(
                    "FSCTL_READ_USN_JOURNAL returned an invalid byte count ({returned})"
                ));
            }

            let (next_usn, changes) = parse_usn_buffer(&buffer[..returned as usize])?;
            if next_usn < cursor {
                return Err(format!(
                    "USN journal cursor moved backwards ({cursor} -> {next_usn})"
                ));
            }
            total_records = total_records.saturating_add(changes.len());
            on_batch(changes);
            if next_usn == cursor {
                // With BytesToWaitFor=0, Windows returns the same cursor when no matching
                // records are currently available. If the queried target is still ahead, fail
                // this sync rather than persisting a cursor that silently claims to be current.
                return Err(format!(
                    "USN journal has not returned records at cursor {cursor}, but the queried target is {target_next_usn}"
                ));
            }
            cursor = next_usn;
        }
        Ok((cursor, total_records))
    }
}

#[cfg(not(windows))]
mod native {
    use super::{JournalInfo, UsnChange};

    pub fn query_journal(_volume: &str) -> Result<JournalInfo, String> {
        Err("USN Change Journal access is available only in the Windows build".into())
    }

    pub fn read_changes<F>(
        _volume: &str,
        _start_usn: i64,
        _journal_id: u64,
        _target_next_usn: i64,
        _on_batch: F,
    ) -> Result<(i64, usize), String>
    where
        F: FnMut(Vec<UsnChange>),
    {
        Err("USN Change Journal access is available only in the Windows build".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_v2_record(
        name: &str,
        file_ref: u64,
        parent_ref: u64,
        usn: i64,
        reason: u32,
        attributes: u32,
    ) -> Vec<u8> {
        let wide: Vec<u16> = name.encode_utf16().collect();
        let length = 60 + wide.len() * 2;
        let mut record = vec![0u8; length];
        record[0..4].copy_from_slice(&(length as u32).to_le_bytes());
        record[4..6].copy_from_slice(&2u16.to_le_bytes());
        record[8..16].copy_from_slice(&file_ref.to_le_bytes());
        record[16..24].copy_from_slice(&parent_ref.to_le_bytes());
        record[24..32].copy_from_slice(&(usn as u64).to_le_bytes());
        record[40..44].copy_from_slice(&reason.to_le_bytes());
        record[52..56].copy_from_slice(&attributes.to_le_bytes());
        record[56..58].copy_from_slice(&((wide.len() * 2) as u16).to_le_bytes());
        record[58..60].copy_from_slice(&60u16.to_le_bytes());
        for (index, unit) in wide.iter().enumerate() {
            let offset = 60 + index * 2;
            record[offset..offset + 2].copy_from_slice(&unit.to_le_bytes());
        }
        record
    }

    fn sample_state() -> IndexState {
        IndexState {
            volume: "C:".into(),
            roots: vec!["C:\\Users\\barda".into()],
            max_depth: 8,
            journal_id: 77,
            cursor: 100,
            entries: vec![
                MftEntry {
                    file_reference: 20,
                    parent_reference: 5,
                    name: "Users".into(),
                    is_dir: true,
                },
                MftEntry {
                    file_reference: 21,
                    parent_reference: 20,
                    name: "barda".into(),
                    is_dir: true,
                },
                MftEntry {
                    file_reference: 22,
                    parent_reference: 21,
                    name: "notes.txt".into(),
                    is_dir: false,
                },
            ],
        }
    }

    #[test]
    fn parses_v2_usn_journal_record() {
        let mut buffer = 240u64.to_le_bytes().to_vec();
        buffer.extend(make_v2_record("résumé.txt", 22, 21, 220, 0x0000_0100, 0));
        let (next_usn, changes) = parse_usn_buffer(&buffer).unwrap();
        assert_eq!(next_usn, 240);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].file_reference, 22);
        assert_eq!(changes[0].parent_reference, 21);
        assert_eq!(changes[0].usn, 220);
        assert_eq!(changes[0].name, "résumé.txt");
        assert!(!changes[0].is_dir);
    }

    #[test]
    fn parses_delete_events_instead_of_filtering_them_out() {
        let mut buffer = 240u64.to_le_bytes().to_vec();
        buffer.extend(make_v2_record("notes.txt", 22, 21, 220, USN_REASON_FILE_DELETE, 0));
        let (_, changes) = parse_usn_buffer(&buffer).unwrap();
        assert_eq!(changes.len(), 1);
        assert_ne!(changes[0].reason & USN_REASON_FILE_DELETE, 0);
    }

    #[test]
    fn rejects_truncated_or_invalid_journal_records() {
        assert!(parse_usn_buffer(&[0; 7]).is_err());
        let mut buffer = 100u64.to_le_bytes().to_vec();
        buffer.extend(7u32.to_le_bytes());
        buffer.extend([0u8; 4]);
        assert!(parse_usn_buffer(&buffer).is_err());
    }

    #[test]
    fn rejects_unknown_record_versions_without_silently_skipping_changes() {
        let mut buffer = 100u64.to_le_bytes().to_vec();
        let mut record = vec![0u8; 8];
        record[0..4].copy_from_slice(&8u32.to_le_bytes());
        record[4..6].copy_from_slice(&3u16.to_le_bytes());
        buffer.extend(record);
        let error = parse_usn_buffer(&buffer).unwrap_err();
        assert!(error.contains("Unsupported USN journal record version 3"));
    }

    #[test]
    fn rename_and_directory_updates_resolve_descendant_paths_from_parent_ids() {
        let mut state = sample_state();
        // Rename a directory below the configured root. Renaming `barda` itself would move the
        // configured root outside `C:\Users\barda`, so the correct filtered snapshot would
        // contain no entries from that renamed root.
        state.entries.push(MftEntry {
            file_reference: 23,
            parent_reference: 21,
            name: "Documents".into(),
            is_dir: true,
        });
        state.entries.push(MftEntry {
            file_reference: 24,
            parent_reference: 23,
            name: "notes.txt".into(),
            is_dir: false,
        });

        let mut index = MutableIndex::new(std::mem::take(&mut state.entries));
        index.apply(UsnChange {
            file_reference: 23,
            parent_reference: 21,
            usn: 220,
            reason: 0x0000_2000, // USN_REASON_RENAME_NEW_NAME
            name: "profile".into(),
            is_dir: true,
        });
        let entries = index.finish();
        let snapshot = mft::make_cache_text(
            &entries,
            &state.roots,
            state.max_depth,
            &state.volume,
        );
        assert!(snapshot
            .lines()
            .any(|line| line == "F\tC:\\Users\\barda\\profile\\notes.txt"));
        assert!(!snapshot.contains("C:\\Users\\barda\\Documents\\notes.txt"));
    }

    #[test]
    fn applies_file_creation_and_deletion_by_file_reference() {
        let mut state = sample_state();
        let mut index = MutableIndex::new(std::mem::take(&mut state.entries));
        index.apply(UsnChange {
            file_reference: 23,
            parent_reference: 21,
            usn: 230,
            reason: 0x0000_0100,
            name: "new.txt".into(),
            is_dir: false,
        });
        index.apply(UsnChange {
            file_reference: 22,
            parent_reference: 21,
            usn: 240,
            reason: USN_REASON_FILE_DELETE,
            name: "notes.txt".into(),
            is_dir: false,
        });
        let entries = index.finish();
        let snapshot = mft::make_cache_text(
            &entries,
            &state.roots,
            state.max_depth,
            &state.volume,
        );
        assert!(snapshot.contains("F\tC:\\Users\\barda\\new.txt"));
        assert!(!snapshot.contains("notes.txt"));
    }

    #[test]
    fn state_file_round_trips_and_rejects_trailing_data() {
        let state = sample_state();
        let encoded = encode_state(&state).unwrap();
        let decoded = decode_state(&encoded).unwrap();
        assert_eq!(decoded.volume, state.volume);
        assert_eq!(decoded.roots, state.roots);
        assert_eq!(decoded.max_depth, state.max_depth);
        assert_eq!(decoded.journal_id, state.journal_id);
        assert_eq!(decoded.cursor, state.cursor);
        assert_eq!(decoded.entries.len(), state.entries.len());

        let mut with_trailing = encoded.clone();
        with_trailing.push(0);
        assert!(decode_state(&with_trailing).is_err());

        // A malicious/corrupt count must be rejected before Vec::with_capacity.
        let encoded_entries_size = state
            .entries
            .iter()
            .map(|entry| 21 + entry.name.len())
            .sum::<usize>();
        let count_offset = encoded.len() - encoded_entries_size - 8;
        let mut impossible_count = encoded;
        impossible_count[count_offset..count_offset + 8]
            .copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(decode_state(&impossible_count).is_err());
    }

    #[test]
    fn journal_reset_and_expired_cursor_require_a_rebuild() {
        let state = sample_state();
        assert!(journal_resume_problem(
            &state,
            JournalInfo {
                journal_id: 78,
                first_usn: 0,
                next_usn: 200,
                lowest_valid_usn: 0,
            }
        )
        .is_some());
        assert!(journal_resume_problem(
            &state,
            JournalInfo {
                journal_id: 77,
                first_usn: 101,
                next_usn: 200,
                lowest_valid_usn: 0,
            }
        )
        .is_some());
        assert!(journal_resume_problem(
            &state,
            JournalInfo {
                journal_id: 77,
                first_usn: 0,
                next_usn: 99,
                lowest_valid_usn: 0,
            }
        )
        .is_some());
        assert!(journal_resume_problem(
            &state,
            JournalInfo {
                journal_id: 77,
                first_usn: 0,
                next_usn: 200,
                lowest_valid_usn: 101,
            }
        )
        .is_some());
        assert!(journal_resume_problem(
            &state,
            JournalInfo {
                journal_id: 77,
                first_usn: 0,
                next_usn: 200,
                lowest_valid_usn: 0,
            }
        )
        .is_none());
    }
}
