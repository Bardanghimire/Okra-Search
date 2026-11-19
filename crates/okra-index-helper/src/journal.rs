//! Persistent NTFS index state and USN Change Journal synchronization.
//!
//! Stage 2 keeps the launcher untouched. The elevated helper creates an MFT baseline, replays
//! changes that occurred during that scan, and on later runs applies only journal records newer
//! than the saved cursor. `--watch` keeps this helper process alive and polls the journal.

use crate::mft::{self, MftEntry};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const STATE_MAGIC: &[u8; 8] = b"OKRAUSN1";
const STATE_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const STATE_MAX_ENTRIES: u64 = 100_000_000;
const STATE_MAX_STRING_BYTES: usize = 1024 * 1024;

const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
const USN_REASON_FILE_DELETE: u32 = 0x0000_0200;

/// How long the resident cursor may run ahead of the persisted one while journal records keep
/// arriving without changing any name. A crash then replays at most this much journal, which is
/// idempotent; the interval stays far below the time the journal needs to wrap.
const CURSOR_PERSIST_INTERVAL: Duration = Duration::from_secs(60);

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
    /// Records that actually created, removed or altered an entry's parent, name or directory
    /// flag. Records that merely touch an existing entry (data writes, closes, attribute or
    /// security changes) do not count and never require a snapshot export.
    namespace_changes: usize,
    cursor_before: i64,
    cursor_after: i64,
    rebuilt: bool,
    scope_changed: bool,
}

/// One independently journaled NTFS volume. Each volume owns separate persistent state and a
/// snapshot, while the IPC server merges those snapshots into one searchable index.
#[derive(Debug, Clone)]
pub(super) struct VolumeConfig {
    pub volume: String,
    pub roots: Vec<String>,
    pub max_depth: usize,
    pub snapshot_path: std::path::PathBuf,
    pub state_path: std::path::PathBuf,
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
                return Err(format!(
                    "USN_RECORD_V2 is too short ({record_length} bytes)"
                ));
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

fn resume_problem(state: &IndexState, volume: &str, journal: JournalInfo) -> Option<String> {
    if !state.volume.eq_ignore_ascii_case(volume) {
        return Some(format!(
            "saved index belongs to volume {}, not {volume}",
            state.volume
        ));
    }
    journal_resume_problem(state, journal)
}

fn journal_resume_problem(state: &IndexState, journal: JournalInfo) -> Option<String> {
    journal_resume_problem_at(state.journal_id, state.cursor, journal)
}

fn journal_resume_problem_at(
    saved_journal_id: u64,
    saved_cursor: i64,
    journal: JournalInfo,
) -> Option<String> {
    if saved_journal_id != journal.journal_id {
        return Some(format!(
            "USN journal ID changed (saved {saved_journal_id}, current {})",
            journal.journal_id
        ));
    }
    if saved_cursor < journal.first_usn {
        return Some(format!(
            "saved USN cursor {saved_cursor} has expired; journal FirstUsn is {}",
            journal.first_usn
        ));
    }
    if saved_cursor < journal.lowest_valid_usn {
        return Some(format!(
            "saved USN cursor {saved_cursor} predates the journal's LowestValidUsn {}",
            journal.lowest_valid_usn
        ));
    }
    if saved_cursor > journal.next_usn {
        return Some(format!(
            "saved USN cursor {saved_cursor} is ahead of journal NextUsn {}",
            journal.next_usn
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

    /// Apply one journal record. Returns true only when the graph's names changed (an entry was
    /// created, removed, or had its parent, name or directory flag altered).
    fn apply(&mut self, change: UsnChange) -> bool {
        if change.reason & USN_REASON_FILE_DELETE != 0 {
            if let Some(position) = self.by_reference.remove(&change.file_reference) {
                self.entries[position] = None;
                return true;
            }
            return false;
        }

        // Some journal records (for example, close-only notifications) can omit a useful name.
        // They do not alter the namespace graph and should not erase a known name.
        if change.name.is_empty() {
            return false;
        }

        let entry = MftEntry {
            file_reference: change.file_reference,
            parent_reference: change.parent_reference,
            name: change.name,
            is_dir: change.is_dir,
        };
        if let Some(position) = self.by_reference.get(&entry.file_reference).copied() {
            let unchanged = self.entries[position].as_ref().is_some_and(|existing| {
                existing.parent_reference == entry.parent_reference
                    && existing.is_dir == entry.is_dir
                    && existing.name == entry.name
            });
            if unchanged {
                return false;
            }
            self.entries[position] = Some(entry);
        } else {
            let position = self.entries.len();
            self.by_reference.insert(entry.file_reference, position);
            self.entries.push(Some(entry));
        }
        true
    }

    fn live_count(&self) -> usize {
        self.by_reference.len()
    }

    /// Approximate retained graph storage (entry vector, FRN buckets, and UTF-8 name buffers).
    /// Allocator metadata and HashMap control bytes are intentionally excluded; process RSS is
    /// measured separately in the Windows validation script.
    fn memory_bytes(&self) -> usize {
        let entry_strings = self
            .entries
            .iter()
            .flatten()
            .map(|entry| entry.name.capacity())
            .sum::<usize>();
        self.entries.capacity() * std::mem::size_of::<Option<MftEntry>>()
            + self.by_reference.capacity() * std::mem::size_of::<(u64, usize)>()
            + entry_strings
    }

    fn snapshot_entries(&self) -> Vec<MftEntry> {
        let mut entries: Vec<MftEntry> = self.entries.iter().flatten().cloned().collect();
        entries.sort_unstable_by_key(|entry| entry.file_reference);
        entries
    }

    fn compact_if_sparse(&mut self) {
        let live = self.by_reference.len();
        let slots = self.entries.len();
        let deleted = slots.saturating_sub(live);
        if slots < 100_000 || deleted.saturating_mul(4) < slots {
            return;
        }

        let mut compacted = Vec::with_capacity(live);
        let mut by_reference = HashMap::with_capacity(live);
        for entry in self.entries.drain(..).flatten() {
            let position = compacted.len();
            by_reference.insert(entry.file_reference, position);
            compacted.push(Some(entry));
        }
        self.entries = compacted;
        self.by_reference = by_reference;
    }

    fn finish(self) -> Vec<MftEntry> {
        let mut entries: Vec<MftEntry> = self.entries.into_iter().flatten().collect();
        entries.sort_unstable_by_key(|entry| entry.file_reference);
        entries
    }
}

/// The helper keeps this mutable FRN graph resident between USN polling cycles. Previously each
/// one-second poll re-read the entire state file and rebuilt the large file-reference HashMap,
/// even when the journal contained no changes.
struct ResidentVolumeIndex {
    volume: String,
    roots: Vec<String>,
    max_depth: usize,
    journal_id: u64,
    cursor: i64,
    index: MutableIndex,
    /// A name, scope or earlier persistence failure means the snapshot and state must both be
    /// rewritten. While this is false, the exported snapshot is exactly what a fresh export of
    /// the resident graph would produce, so only the cursor can be behind.
    dirty: bool,
    /// The cursor advanced since the state file was last written.
    state_unsaved: bool,
    last_state_write: Instant,
    /// Batches and records seen since the last write that changed no names (export skipped).
    skipped_batches: u64,
    skipped_records: u64,
}

impl ResidentVolumeIndex {
    fn from_state(state: IndexState, dirty: bool) -> Self {
        let volume = state.volume;
        let index = MutableIndex::new(state.entries);
        eprintln!(
            "Resident NTFS graph on {volume}: {} MFT entries, approximately {:.1} MiB retained graph storage; FRN map will be reused between journal polls",
            index.live_count(),
            index.memory_bytes() as f64 / (1024.0 * 1024.0)
        );
        Self {
            volume,
            roots: state.roots,
            max_depth: state.max_depth,
            journal_id: state.journal_id,
            cursor: state.cursor,
            index,
            dirty,
            state_unsaved: false,
            last_state_write: Instant::now(),
            skipped_batches: 0,
            skipped_records: 0,
        }
    }

    fn snapshot_state(&self, roots: &[String], max_depth: usize) -> IndexState {
        IndexState {
            volume: self.volume.clone(),
            roots: roots.to_vec(),
            max_depth,
            journal_id: self.journal_id,
            cursor: self.cursor,
            entries: self.index.snapshot_entries(),
        }
    }
}

/// Outcome of applying one batch of journal records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BatchEffect {
    records: usize,
    namespace_changes: usize,
}

fn apply_changes_counted(index: &mut MutableIndex, changes: Vec<UsnChange>) -> BatchEffect {
    let records = changes.len();
    let mut namespace_changes = 0usize;
    for change in changes {
        // The USN value is parsed for diagnostics/ordering validation. The journal API returns
        // records in USN order, and the latest record for a file reference wins.
        let _record_usn = change.usn;
        if index.apply(change) {
            namespace_changes += 1;
        }
    }
    index.compact_if_sparse();
    BatchEffect {
        records,
        namespace_changes,
    }
}

fn apply_changes(index: &mut MutableIndex, changes: Vec<UsnChange>) -> usize {
    apply_changes_counted(index, changes).records
}

/// What a polling round must write to disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PersistPlan {
    /// Nothing is due (the common case for batches that change no names).
    Nothing,
    /// Only the cursor is behind: rewrite the state file, leave the snapshot untouched.
    StateOnly,
    /// Names or scope changed (or a write failed earlier): rewrite snapshot and state.
    Full,
}

fn plan_persistence(
    dirty: bool,
    rebuilt: bool,
    scope_changed: bool,
    has_outputs: bool,
    state_unsaved: bool,
    since_state_write: Duration,
    force_state: bool,
) -> PersistPlan {
    if dirty || rebuilt || scope_changed || !has_outputs {
        PersistPlan::Full
    } else if state_unsaved && (force_state || since_state_write >= CURSOR_PERSIST_INTERVAL) {
        PersistPlan::StateOnly
    } else {
        PersistPlan::Nothing
    }
}

struct StateReader<R> {
    reader: R,
    remaining: u64,
}

impl<R: std::io::Read> StateReader<R> {
    fn read_exact(&mut self, target: &mut [u8]) -> Result<(), String> {
        let length = target.len() as u64;
        if length > self.remaining {
            return Err("Persistent index file is truncated".into());
        }
        self.reader
            .read_exact(target)
            .map_err(|error| format!("Persistent index file is truncated: {error}"))?;
        self.remaining -= length;
        Ok(())
    }

    fn u8(&mut self) -> Result<u8, String> {
        let mut bytes = [0u8; 1];
        self.read_exact(&mut bytes)?;
        Ok(bytes[0])
    }

    fn u32(&mut self) -> Result<u32, String> {
        let mut bytes = [0u8; 4];
        self.read_exact(&mut bytes)?;
        Ok(u32::from_le_bytes(bytes))
    }

    fn u64(&mut self) -> Result<u64, String> {
        let mut bytes = [0u8; 8];
        self.read_exact(&mut bytes)?;
        Ok(u64::from_le_bytes(bytes))
    }

    fn string(&mut self) -> Result<String, String> {
        let length = self.u32()? as usize;
        if length > STATE_MAX_STRING_BYTES {
            return Err("Persistent index string exceeds the safety limit".into());
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(length)
            .map_err(|error| format!("Could not allocate persistent index string: {error}"))?;
        bytes.resize(length, 0);
        self.read_exact(&mut bytes)?;
        String::from_utf8(bytes).map_err(|_| "Persistent index contains invalid UTF-8".to_string())
    }

    fn finish(mut self) -> Result<(), String> {
        if self.remaining != 0 {
            return Err("Persistent index contains unexpected trailing data".into());
        }
        let mut extra = [0u8; 1];
        match std::io::Read::read(&mut self.reader, &mut extra) {
            Ok(0) => Ok(()),
            Ok(_) => Err("Persistent index contains unexpected trailing data".into()),
            Err(error) => Err(format!(
                "Could not finish reading persistent index: {error}"
            )),
        }
    }
}

struct StateWriter<'a, W> {
    writer: &'a mut W,
    written: u64,
}

impl<'a, W: std::io::Write> StateWriter<'a, W> {
    fn raw(&mut self, bytes: &[u8]) -> Result<(), String> {
        let next = self
            .written
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| "Persistent index size overflow".to_string())?;
        if next > STATE_MAX_BYTES {
            return Err(format!(
                "Persistent index would exceed the {}-byte safety limit",
                STATE_MAX_BYTES
            ));
        }
        self.writer
            .write_all(bytes)
            .map_err(|error| format!("Could not write persistent index: {error}"))?;
        self.written = next;
        Ok(())
    }

    fn u32(&mut self, value: u32) -> Result<(), String> {
        self.raw(&value.to_le_bytes())
    }

    fn u64(&mut self, value: u64) -> Result<(), String> {
        self.raw(&value.to_le_bytes())
    }

    fn string(&mut self, value: &str) -> Result<(), String> {
        if value.len() > STATE_MAX_STRING_BYTES {
            return Err("A persistent index string exceeds the safety limit".into());
        }
        self.u32(
            u32::try_from(value.len())
                .map_err(|_| "Persistent index string is too long".to_string())?,
        )?;
        self.raw(value.as_bytes())
    }
}

fn write_state<W: std::io::Write>(writer: &mut W, state: &IndexState) -> Result<u64, String> {
    let mut output = StateWriter { writer, written: 0 };
    output.raw(STATE_MAGIC)?;
    output.string(&state.volume)?;
    output.u32(
        u32::try_from(state.roots.len()).map_err(|_| "Too many configured roots".to_string())?,
    )?;
    for root in &state.roots {
        output.string(root)?;
    }
    output.u64(state.max_depth as u64)?;
    output.u64(state.journal_id)?;
    output.u64(state.cursor as u64)?;
    output.u64(state.entries.len() as u64)?;
    for entry in &state.entries {
        output.u64(entry.file_reference)?;
        output.u64(entry.parent_reference)?;
        output.raw(&[u8::from(entry.is_dir)])?;
        output.string(&entry.name)?;
    }
    Ok(output.written)
}

#[cfg(test)]
fn encode_state(state: &IndexState) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    write_state(&mut bytes, state)?;
    Ok(bytes)
}

fn decode_state_reader<R: std::io::Read>(reader: R, size: u64) -> Result<IndexState, String> {
    if size < STATE_MAGIC.len() as u64 {
        return Err("Persistent index has an unknown file signature".into());
    }
    let mut reader = StateReader {
        reader,
        remaining: size,
    };
    let mut magic = [0u8; STATE_MAGIC.len()];
    reader.read_exact(&mut magic)?;
    if &magic != STATE_MAGIC {
        return Err("Persistent index has an unknown file signature".into());
    }

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
    let maximum_possible_entries = reader.remaining / 21;
    if entry_count > maximum_possible_entries {
        return Err("Persistent index entry count exceeds the remaining file data".into());
    }
    let entry_capacity = usize::try_from(entry_count)
        .map_err(|_| "Persistent index entry count does not fit this platform".to_string())?;
    let mut entries = Vec::new();
    entries
        .try_reserve_exact(entry_capacity)
        .map_err(|error| format!("Could not allocate persistent MFT entries: {error}"))?;
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
    reader.finish()?;
    Ok(IndexState {
        volume,
        roots,
        max_depth,
        journal_id,
        cursor,
        entries,
    })
}

#[cfg(test)]
fn decode_state(bytes: &[u8]) -> Result<IndexState, String> {
    decode_state_reader(std::io::Cursor::new(bytes), bytes.len() as u64)
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
            metadata.len(),
            STATE_MAX_BYTES
        ));
    }
    let file = fs::File::open(path)
        .map_err(|error| format!("Could not open persistent index: {error}"))?;
    decode_state_reader(std::io::BufReader::new(file), metadata.len()).map(Some)
}

fn write_state_temporary(path: &Path, state: &IndexState) -> Result<PathBuf, String> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Could not create state folder: {error}"))?;
    }
    let temporary = path.with_extension("state.tmp");
    let file = fs::File::create(&temporary)
        .map_err(|error| format!("Could not create temporary state file: {error}"))?;
    let mut writer = std::io::BufWriter::new(file);
    if let Err(error) = write_state(&mut writer, state) {
        drop(writer);
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    if let Err(error) = writer.flush() {
        drop(writer);
        let _ = fs::remove_file(&temporary);
        return Err(format!("Could not flush temporary state file: {error}"));
    }
    drop(writer);
    Ok(temporary)
}

fn persist_outputs(
    state: &IndexState,
    snapshot_path: &Path,
    state_path: &Path,
) -> Result<usize, String> {
    // Stage the state file first, but stream it through a bounded buffer instead of allocating a
    // second volume-sized byte Vec. The search snapshot is then replaced before the cursor state:
    // a crash between those replacements safely replays journal records on the next start.
    let temporary_state = write_state_temporary(state_path, state)?;
    let count = match mft::write_cache_atomically(
        snapshot_path,
        &state.entries,
        &state.roots,
        state.max_depth,
        &state.volume,
    ) {
        Ok(count) => count,
        Err(error) => {
            let _ = fs::remove_file(&temporary_state);
            return Err(error);
        }
    };
    crate::atomic::replace(&temporary_state, state_path)
        .map_err(|error| format!("Could not replace persistent index: {error}"))?;
    Ok(count)
}

/// Rewrite only the state file. Safe only while the exported snapshot already equals an export
/// of the same graph (see `ResidentVolumeIndex::dirty`).
fn persist_state_only(state: &IndexState, state_path: &Path) -> Result<(), String> {
    let temporary_state = write_state_temporary(state_path, state)?;
    crate::atomic::replace(&temporary_state, state_path)
        .map_err(|error| format!("Could not replace persistent index: {error}"))
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
        // Keep the complete MFT graph for this volume in persistent state. Configured roots and
        // depth restrict only the exported/searchable snapshot, so changing scope can be served
        // from this graph without repeating the expensive MFT enumeration.
        let entries = mft::enumerate(volume)?;
        mft::log_path_diagnostics(&entries);
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

fn rebuild_resident(
    volume: &str,
    roots: &[String],
    max_depth: usize,
    old_cursor: i64,
    resident: &mut Option<ResidentVolumeIndex>,
) -> Result<SyncStats, String> {
    let (state, changes_read) = rebuild_state(volume, roots, max_depth)?;
    let cursor_after = state.cursor;
    *resident = Some(ResidentVolumeIndex::from_state(state, true));
    Ok(SyncStats {
        changes_read,
        namespace_changes: changes_read,
        cursor_before: old_cursor,
        cursor_after,
        rebuilt: true,
        scope_changed: false,
    })
}

/// Synchronize a volume against its resident FRN graph. Persistent state is loaded once when the
/// helper starts; subsequent polling cycles query the USN journal and apply deltas in place.
fn sync_once(
    volume: &str,
    roots: &[String],
    max_depth: usize,
    state_path: &Path,
    resident: &mut Option<ResidentVolumeIndex>,
) -> Result<SyncStats, String> {
    let journal = native::query_journal(volume)?;

    if resident.is_none() {
        let loaded = match load_state(state_path) {
            Ok(state) => state,
            Err(error) => {
                eprintln!("Persistent index cannot be resumed ({error}); rebuilding from the MFT.");
                None
            }
        };
        match loaded {
            Some(state) => {
                if let Some(reason) = resume_problem(&state, volume, journal) {
                    eprintln!("Cannot resume the saved index: {reason}. Rebuilding from the MFT.");
                    let old_cursor = state.cursor;
                    return rebuild_resident(volume, roots, max_depth, old_cursor, resident);
                }
                let scope_changed = !config_matches(&state, volume, roots, max_depth);
                *resident = Some(ResidentVolumeIndex::from_state(state, scope_changed));
            }
            None => return rebuild_resident(volume, roots, max_depth, 0, resident),
        }
    }

    let (current_volume_matches, current_cursor, current_journal_id, scope_changed) = {
        let current = resident
            .as_ref()
            .ok_or_else(|| "NTFS resident index initialization failed".to_string())?;
        (
            current.volume.eq_ignore_ascii_case(volume),
            current.cursor,
            current.journal_id,
            normalized_roots(&current.roots) != normalized_roots(roots)
                || current.max_depth != max_depth,
        )
    };
    if !current_volume_matches {
        eprintln!("The resident index belongs to a different volume; rebuilding from the MFT.");
        return rebuild_resident(volume, roots, max_depth, current_cursor, resident);
    }
    if let Some(reason) = journal_resume_problem_at(current_journal_id, current_cursor, journal) {
        eprintln!("Cannot resume the resident index: {reason}. Rebuilding from the MFT.");
        return rebuild_resident(volume, roots, max_depth, current_cursor, resident);
    }

    let cursor_before = current_cursor;
    let mut namespace_changes = 0usize;
    let (cursor_after, changes_read) = match native::read_changes(
        volume,
        cursor_before,
        journal.journal_id,
        journal.next_usn,
        |batch| {
            if let Some(current) = resident.as_mut() {
                let effect = apply_changes_counted(&mut current.index, batch);
                namespace_changes += effect.namespace_changes;
                if effect.namespace_changes > 0 {
                    current.dirty = true;
                }
            }
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
                // Earlier batches may already have been applied. Keep the old cursor and retry;
                // replaying those records is idempotent and the dirty flag forces persistence.
                if let Some(current) = resident.as_mut() {
                    current.dirty = true;
                }
                return Err(error);
            }
            eprintln!("The saved journal position expired while reading; rebuilding from the MFT.");
            return rebuild_resident(volume, roots, max_depth, cursor_before, resident);
        }
    };

    let latest = native::query_journal(volume)?;
    if latest.journal_id == journal.journal_id
        && cursor_after >= latest.first_usn
        && cursor_after >= latest.lowest_valid_usn
        && cursor_after <= latest.next_usn
    {
        let current = resident
            .as_mut()
            .ok_or_else(|| "NTFS resident index disappeared during synchronization".to_string())?;
        current.journal_id = latest.journal_id;
        current.cursor = cursor_after;
        // `dirty` is set above only by records that changed names. A cursor that merely moved
        // forward past name-neutral records is tracked separately and persisted on a timer.
        current.dirty |= scope_changed;
        if changes_read > 0 || cursor_before != cursor_after {
            current.state_unsaved = true;
        }
        return Ok(SyncStats {
            changes_read,
            namespace_changes,
            cursor_before,
            cursor_after,
            rebuilt: false,
            scope_changed,
        });
    }

    eprintln!("The USN journal changed during synchronization; rebuilding from the MFT.");
    rebuild_resident(volume, roots, max_depth, cursor_before, resident)
}

/// Synchronize one volume for a single polling round. The boolean says whether this volume
/// completed successfully; the optional count is present only when its snapshot was rewritten.
fn sync_volume_round(
    volume: &VolumeConfig,
    watch: bool,
    initialized: bool,
    poll_seconds: u64,
    resident: &mut Option<ResidentVolumeIndex>,
) -> Result<(bool, Option<usize>), String> {
    let stats = match sync_once(
        &volume.volume,
        &volume.roots,
        volume.max_depth,
        &volume.state_path,
        resident,
    ) {
        Ok(stats) => stats,
        Err(error) if watch && initialized => {
            eprintln!(
                "USN synchronization failed for {}: {error}. Retrying in {poll_seconds}s.",
                volume.volume
            );
            return Ok((false, None));
        }
        Err(error) => return Err(error),
    };

    let has_outputs = volume.snapshot_path.exists() && volume.state_path.exists();
    let plan = {
        let current = resident.as_mut().ok_or_else(|| {
            "NTFS resident index is unavailable after synchronization".to_string()
        })?;
        if stats.changes_read > 0 && stats.namespace_changes == 0 {
            current.skipped_batches += 1;
            current.skipped_records += stats.changes_read as u64;
        }
        plan_persistence(
            current.dirty,
            stats.rebuilt,
            stats.scope_changed,
            has_outputs,
            current.state_unsaved,
            current.last_state_write.elapsed(),
            !watch,
        )
    };

    match plan {
        PersistPlan::Nothing => {
            if !watch {
                let cursor = resident.as_ref().map_or(0, |current| current.cursor);
                println!(
                    "NTFS index on {} is up to date at USN {}; no changes to apply.",
                    volume.volume, cursor
                );
            }
            return Ok((true, None));
        }
        PersistPlan::StateOnly => {
            let (state, skipped_batches, skipped_records) = {
                let current = resident.as_ref().ok_or_else(|| {
                    "NTFS resident index is unavailable after synchronization".to_string()
                })?;
                (
                    current.snapshot_state(&volume.roots, volume.max_depth),
                    current.skipped_batches,
                    current.skipped_records,
                )
            };
            // A failed cursor write cannot make the snapshot wrong (it still matches the graph),
            // so it never marks the volume stale; retry after the next interval.
            let result = persist_state_only(&state, &volume.state_path);
            if let Some(current) = resident.as_mut() {
                current.last_state_write = Instant::now();
                if result.is_ok() {
                    current.state_unsaved = false;
                    current.skipped_batches = 0;
                    current.skipped_records = 0;
                }
            }
            match result {
                Ok(()) => println!(
                    "Saved NTFS cursor on {} at USN {}; {} batch(es) / {} journal record(s) since the last save changed no names, so no snapshot export was needed.",
                    volume.volume, state.cursor, skipped_batches, skipped_records
                ),
                Err(error) => eprintln!(
                    "Could not save the NTFS cursor for {}: {error}. The snapshot is still current; retrying later.",
                    volume.volume
                ),
            }
            return Ok((true, None));
        }
        PersistPlan::Full => {}
    }

    // Clone the MFT entries only when a real name/scope change needs persistence. The much
    // larger FRN HashMap and mutable graph remain resident and are not rebuilt every poll.
    let (state, skipped_batches, skipped_records) = {
        let current = resident.as_ref().ok_or_else(|| {
            "NTFS resident index is unavailable after synchronization".to_string()
        })?;
        (
            current.snapshot_state(&volume.roots, volume.max_depth),
            current.skipped_batches,
            current.skipped_records,
        )
    };
    match persist_outputs(&state, &volume.snapshot_path, &volume.state_path) {
        Ok(indexed) => {
            if let Some(current) = resident.as_mut() {
                current.roots = volume.roots.clone();
                current.max_depth = volume.max_depth;
                current.dirty = false;
                current.state_unsaved = false;
                current.last_state_write = Instant::now();
                current.skipped_batches = 0;
                current.skipped_records = 0;
            }
            if stats.rebuilt {
                println!(
                    "Built NTFS index on {}: {} MFT entries, {} journal changes replayed, {} paths exported.",
                    volume.volume, state.entries.len(), stats.changes_read, indexed
                );
            } else if stats.scope_changed {
                println!(
                    "Updated NTFS index scope on {} using the resident MFT graph; applied {} USN journal records (cursor {} -> {}); {} paths exported.",
                    volume.volume, stats.changes_read, stats.cursor_before, stats.cursor_after, indexed
                );
            } else {
                println!(
                    "Applied {} USN journal records on {} (cursor {} -> {}; {} changed names; {} earlier batch(es) / {} record(s) changed none); {} paths exported.",
                    stats.changes_read,
                    volume.volume,
                    stats.cursor_before,
                    stats.cursor_after,
                    stats.namespace_changes,
                    skipped_batches,
                    skipped_records,
                    indexed
                );
            }
            Ok((true, Some(indexed)))
        }
        Err(error) if watch && initialized => {
            // Leave the resident graph and cursor in memory, marked dirty. The next poll retries
            // writing the same current snapshot/state; a process restart safely replays from disk.
            if let Some(current) = resident.as_mut() {
                current.dirty = true;
            }
            eprintln!(
                "Could not persist the NTFS index for {}: {error}. Retrying in {poll_seconds}s.",
                volume.volume
            );
            Ok((false, None))
        }
        Err(error) => {
            if let Some(current) = resident.as_mut() {
                current.dirty = true;
            }
            Err(error)
        }
    }
}

/// Best-effort write of a cursor that is ahead of the saved one, used at graceful shutdown so
/// the next start replays as little journal as possible. Skipped when a full persist is owed
/// (the snapshot would not match) or when nothing is pending.
fn flush_pending_cursor(volume: &VolumeConfig, resident: &Option<ResidentVolumeIndex>) {
    let Some(current) = resident.as_ref() else {
        return;
    };
    if !current.state_unsaved || current.dirty {
        return;
    }
    let state = current.snapshot_state(&volume.roots, volume.max_depth);
    match persist_state_only(&state, &volume.state_path) {
        Ok(()) => println!(
            "Saved NTFS cursor on {} at USN {} before shutdown.",
            volume.volume, state.cursor
        ),
        Err(error) => eprintln!(
            "Could not save the NTFS cursor for {} at shutdown: {error}.",
            volume.volume
        ),
    }
}

/// Synchronize one or more NTFS volumes. Each volume has its own MFT/USN state and snapshot;
/// all snapshots are served through one search pipe. For a single volume, paths are unchanged.
pub(super) fn run_many(
    volumes: &[VolumeConfig],
    watch: bool,
    poll_seconds: u64,
) -> Result<(), String> {
    if volumes.is_empty() {
        return Err("At least one NTFS volume must be configured".into());
    }
    let mut seen = std::collections::HashSet::new();
    for volume in volumes {
        mft::validate_volume_and_roots(&volume.volume, &volume.roots)?;
        if !seen.insert(volume.volume.to_ascii_uppercase()) {
            return Err(format!("Duplicate NTFS volume {}", volume.volume));
        }
        if volume.snapshot_path == volume.state_path {
            return Err(format!(
                "Snapshot and state paths collide for {}",
                volume.volume
            ));
        }
    }

    let mut initialized = false;
    let mut path_counts: Vec<Option<usize>> = vec![None; volumes.len()];
    let mut resident: Vec<Option<ResidentVolumeIndex>> = std::iter::repeat_with(|| None)
        .take(volumes.len())
        .collect();
    let health = super::status::global();
    let shutdown = super::shutdown::global();

    // A cycle is considered ready only after every configured volume has completed a successful
    // synchronization round. A failed/offline volume therefore becomes visible as stale instead
    // of silently serving an incomplete multi-drive result set.
    while !shutdown.requested() {
        let mut all_current = true;
        for (position, volume) in volumes.iter().enumerate() {
            match sync_volume_round(
                volume,
                watch,
                initialized,
                poll_seconds,
                &mut resident[position],
            ) {
                Ok((current, Some(count))) => {
                    path_counts[position] = Some(count);
                    all_current &= current;
                }
                Ok((current, None)) => {
                    if path_counts[position].is_none() && volume.snapshot_path.exists() {
                        path_counts[position] = Some(count_snapshot_paths(&volume.snapshot_path));
                    }
                    all_current &= current;
                    // A missing initial snapshot must not be reported as healthy.
                    if path_counts[position].is_none() {
                        all_current = false;
                    }
                }
                Err(error) => return Err(error),
            }
            if !volume.snapshot_path.exists() || !volume.state_path.exists() {
                all_current = false;
            }
        }

        if all_current && path_counts.iter().all(Option::is_some) {
            let total = path_counts
                .iter()
                .flatten()
                .copied()
                .fold(0usize, usize::saturating_add);
            health.set_entries(total);
            health.mark_sync_ok(super::status::now_unix());
        } else {
            health.mark_sync_failed();
            eprintln!("One or more configured NTFS volumes are not current; helper health will become stale until all configured volumes recover.");
        }

        initialized = true;
        if !watch {
            return if all_current {
                Ok(())
            } else {
                Err(
                    "One or more configured NTFS volumes did not complete the initial index pass"
                        .into(),
                )
            };
        }
        if shutdown.sleep(Duration::from_secs(poll_seconds)) {
            break;
        }
    }
    for (position, volume) in volumes.iter().enumerate() {
        flush_pending_cursor(volume, &resident[position]);
    }
    Ok(())
}

/// Number of exported paths in an existing snapshot (every line after the header). Stream the
/// count so a multi-million-path TSV is never read into a second full-size byte buffer.
fn count_snapshot_paths(path: &Path) -> usize {
    use std::io::BufRead;
    let Ok(file) = fs::File::open(path) else {
        return 0;
    };
    let mut reader = std::io::BufReader::new(file);
    let mut line = String::new();
    let mut lines = 0usize;
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => lines = lines.saturating_add(1),
        }
    }
    lines.saturating_sub(1)
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
    fn resident_graph_applies_journal_deltas_without_rebuilding_the_frn_map() {
        let mut resident = ResidentVolumeIndex::from_state(sample_state(), false);
        let initial_map_capacity = resident.index.by_reference.capacity();
        assert_eq!(resident.index.live_count(), 3);

        let applied = apply_changes(
            &mut resident.index,
            vec![UsnChange {
                file_reference: 22,
                parent_reference: 21,
                usn: 120,
                reason: 0x0000_2000, // USN_REASON_RENAME_NEW_NAME
                name: "notes-renamed.txt".into(),
                is_dir: false,
            }],
        );
        resident.dirty = applied > 0;
        resident.cursor = 120;

        assert_eq!(applied, 1);
        assert_eq!(resident.index.live_count(), 3);
        assert_eq!(resident.index.by_reference.capacity(), initial_map_capacity);
        assert!(resident.dirty);

        let snapshot = resident.snapshot_state(&[r"C:".into()], usize::MAX);
        assert_eq!(snapshot.cursor, 120);
        assert_eq!(snapshot.roots, vec![r"C:"]);
        assert!(snapshot
            .entries
            .iter()
            .any(|entry| { entry.file_reference == 22 && entry.name == "notes-renamed.txt" }));
    }

    #[test]
    fn full_volume_scope_change_reuses_valid_same_volume_mft_state() {
        let state = sample_state();
        let journal = JournalInfo {
            journal_id: 77,
            first_usn: 0,
            next_usn: 150,
            lowest_valid_usn: 0,
        };
        let full_volume_roots = vec![r"C:\".to_string()];

        assert!(resume_problem(&state, "C:", journal).is_none());
        assert!(!config_matches(
            &state,
            "C:",
            &full_volume_roots,
            usize::MAX
        ));
    }

    #[test]
    fn saved_mft_state_is_not_reused_for_a_different_volume() {
        let state = sample_state();
        let journal = JournalInfo {
            journal_id: 77,
            first_usn: 0,
            next_usn: 150,
            lowest_valid_usn: 0,
        };
        assert!(resume_problem(&state, "D:", journal).is_some());
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
        buffer.extend(make_v2_record(
            "notes.txt",
            22,
            21,
            220,
            USN_REASON_FILE_DELETE,
            0,
        ));
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
        let snapshot = mft::make_cache_text(&entries, &state.roots, state.max_depth, &state.volume);
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
        let snapshot = mft::make_cache_text(&entries, &state.roots, state.max_depth, &state.volume);
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

        // Production startup reads through BufReader rather than duplicating the entire state
        // file in memory. Exercise the same path used by the elevated helper.
        let path = std::env::temp_dir().join(format!(
            "okra-state-stream-{}-{}.state",
            std::process::id(),
            state.cursor
        ));
        let temporary = write_state_temporary(&path, &state).unwrap();
        crate::atomic::replace(&temporary, &path).unwrap();
        let streamed = load_state(&path).unwrap().unwrap();
        assert_eq!(streamed.volume, state.volume);
        assert_eq!(streamed.roots, state.roots);
        assert_eq!(streamed.entries.len(), state.entries.len());
        let _ = fs::remove_file(&path);

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
        impossible_count[count_offset..count_offset + 8].copy_from_slice(&u64::MAX.to_le_bytes());
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

    fn change(
        file_reference: u64,
        parent: u64,
        name: &str,
        is_dir: bool,
        reason: u32,
    ) -> UsnChange {
        UsnChange {
            file_reference,
            parent_reference: parent,
            usn: 0,
            reason,
            name: name.into(),
            is_dir,
        }
    }

    #[test]
    fn apply_reports_whether_names_changed() {
        const DATA_EXTEND: u32 = 0x0000_0002;
        const CLOSE: u32 = 0x8000_0000;
        const RENAME_NEW_NAME: u32 = 0x0000_2000;
        let mut index = MutableIndex::new(sample_state().entries);

        // A write/close notification for an entry that already has this exact name and parent.
        assert!(!index.apply(change(22, 21, "notes.txt", false, DATA_EXTEND | CLOSE)));
        // Close-only record with no name never erases or changes a known entry.
        assert!(!index.apply(change(22, 21, "", false, CLOSE)));
        // Rename, move to another parent, and directory-flag changes are all name changes.
        assert!(index.apply(change(22, 21, "renamed.txt", false, RENAME_NEW_NAME)));
        assert!(index.apply(change(22, 20, "renamed.txt", false, RENAME_NEW_NAME)));
        assert!(index.apply(change(22, 20, "renamed.txt", true, RENAME_NEW_NAME)));
        // Repeating the final state is a no-op again.
        assert!(!index.apply(change(22, 20, "renamed.txt", true, CLOSE)));
        // A brand-new entry is a change; deleting it is a change; deleting it again is not.
        assert!(index.apply(change(99, 21, "new.txt", false, 0x0000_0100)));
        assert!(index.apply(change(99, 21, "new.txt", false, USN_REASON_FILE_DELETE)));
        assert!(!index.apply(change(99, 21, "new.txt", false, USN_REASON_FILE_DELETE)));
    }

    #[test]
    fn batch_effect_counts_only_name_changes() {
        let mut index = MutableIndex::new(sample_state().entries);
        let effect = apply_changes_counted(
            &mut index,
            vec![
                change(22, 21, "notes.txt", false, 0x0000_0002),
                change(22, 21, "notes.txt", false, 0x8000_0000),
                change(21, 20, "barda", true, 0x0000_0004),
                change(22, 21, "notes2.txt", false, 0x0000_2000),
            ],
        );
        assert_eq!(
            effect,
            BatchEffect {
                records: 4,
                namespace_changes: 1
            }
        );
        // The batch that only touched existing names reports none.
        let neutral = apply_changes_counted(
            &mut index,
            vec![change(22, 21, "notes2.txt", false, 0x8000_0000)],
        );
        assert_eq!(neutral.namespace_changes, 0);
        assert_eq!(index.live_count(), 3);
    }

    #[test]
    fn persistence_plan_exports_only_for_name_scope_or_failure_changes() {
        let idle = Duration::from_secs(1);
        let due = CURSOR_PERSIST_INTERVAL;
        use PersistPlan::*;
        // (dirty, rebuilt, scope, has_outputs, unsaved, since, force) -> plan
        let cases = [
            ((false, false, false, true, false, idle, false), Nothing),
            ((false, false, false, true, true, idle, false), Nothing),
            ((false, false, false, true, true, due, false), StateOnly),
            ((false, false, false, true, true, idle, true), StateOnly),
            ((false, false, false, true, false, due, true), Nothing),
            ((true, false, false, true, true, due, false), Full),
            ((false, true, false, true, false, idle, false), Full),
            ((false, false, true, true, false, idle, false), Full),
            ((false, false, false, false, false, idle, false), Full),
        ];
        for ((dirty, rebuilt, scope, outputs, unsaved, since, force), expected) in cases {
            assert_eq!(
                plan_persistence(dirty, rebuilt, scope, outputs, unsaved, since, force),
                expected,
                "dirty={dirty} rebuilt={rebuilt} scope={scope} outputs={outputs} unsaved={unsaved} since={since:?} force={force}"
            );
        }
    }

    #[test]
    fn cursor_only_save_round_trips_and_leaves_the_snapshot_untouched() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("okra-cursor-only-{unique}"));
        fs::create_dir_all(&dir).unwrap();
        let snapshot = dir.join("snap.tsv");
        let state_path = dir.join("snap.state");

        let state = sample_state();
        persist_outputs(&state, &snapshot, &state_path).unwrap();
        let snapshot_before = fs::read(&snapshot).unwrap();
        let stamp_before = fs::metadata(&snapshot).unwrap().modified().unwrap();

        let mut resident = ResidentVolumeIndex::from_state(sample_state(), false);
        resident.cursor = 5_000;
        let later = resident.snapshot_state(&state.roots, state.max_depth);
        persist_state_only(&later, &state_path).unwrap();

        let loaded = load_state(&state_path).unwrap().unwrap();
        assert_eq!(loaded.cursor, 5_000);
        assert_eq!(loaded.entries.len(), 3);
        assert_eq!(fs::read(&snapshot).unwrap(), snapshot_before);
        assert_eq!(
            fs::metadata(&snapshot).unwrap().modified().unwrap(),
            stamp_before
        );
        assert!(!state_path.with_extension("state.tmp").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    /// Randomized simulation of the sync loop with a fake disk and random crashes. Invariants:
    /// (1) whenever the plan is not `Full`, the saved snapshot's names equal the live graph's;
    /// (2) after any crash and replay from the saved cursor, the graph equals the ground truth
    ///     and `dirty` is set whenever the graph differs from the saved snapshot, so the stale
    ///     snapshot is always rewritten before it can be trusted.
    #[test]
    fn skipped_exports_never_leave_a_stale_snapshot_across_crashes() {
        type Names = Vec<(u64, u64, String, bool)>;
        fn names(index: &MutableIndex) -> Names {
            let mut all: Names = index
                .entries
                .iter()
                .flatten()
                .map(|e| {
                    (
                        e.file_reference,
                        e.parent_reference,
                        e.name.clone(),
                        e.is_dir,
                    )
                })
                .collect();
            all.sort();
            all
        }
        let mut seed = 0x0DDB_1A5E_5BAD_5EEDu64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        const DELETE: u32 = USN_REASON_FILE_DELETE;
        for _round in 0..300 {
            let mut log: Vec<UsnChange> = Vec::new();
            // Independent ground truth: a plain map with the documented record semantics
            // (delete removes, nameless records are ignored, anything else sets the entry).
            let mut truth: std::collections::BTreeMap<u64, (u64, String, bool)> = sample_state()
                .entries
                .iter()
                .map(|e| {
                    (
                        e.file_reference,
                        (e.parent_reference, e.name.clone(), e.is_dir),
                    )
                })
                .collect();
            let truth_names =
                |truth: &std::collections::BTreeMap<u64, (u64, String, bool)>| -> Names {
                    truth
                        .iter()
                        .map(|(frn, (parent, name, is_dir))| (*frn, *parent, name.clone(), *is_dir))
                        .collect()
                };
            let mut live = MutableIndex::new(sample_state().entries);
            // Fake disk: snapshot names, and saved (entries, cursor).
            let mut snapshot = names(&live);
            let mut saved_entries = sample_state().entries;
            let mut saved_cursor = 0usize;
            let mut dirty = false;

            for _step in 0..60 {
                // Build a batch: mostly name-neutral touches, sometimes real changes.
                let mut batch = Vec::new();
                for _ in 0..(1 + next() % 4) {
                    let frn = 20 + next() % 6;
                    let record = match next() % 6 {
                        0 => change(
                            frn,
                            20 + next() % 3,
                            &format!("n{}", next() % 3),
                            next() % 4 == 0,
                            0x100,
                        ),
                        1 => change(frn, 20, "x", false, DELETE),
                        2 => change(frn, 21, &format!("r{}", next() % 2), false, 0x2000),
                        _ => {
                            // Touch with whatever the entry currently is (name-neutral).
                            match live
                                .by_reference
                                .get(&frn)
                                .and_then(|p| live.entries[*p].clone())
                            {
                                Some(e) => {
                                    change(frn, e.parent_reference, &e.name, e.is_dir, 0x8000_0002)
                                }
                                None => change(frn, 21, "", false, 0x8000_0000),
                            }
                        }
                    };
                    batch.push(record);
                }
                for record in &batch {
                    log.push(record.clone());
                    if record.reason & DELETE != 0 {
                        truth.remove(&record.file_reference);
                    } else if !record.name.is_empty() {
                        truth.insert(
                            record.file_reference,
                            (record.parent_reference, record.name.clone(), record.is_dir),
                        );
                    }
                }
                let effect = apply_changes_counted(&mut live, batch);
                if effect.namespace_changes > 0 {
                    dirty = true;
                }
                assert_eq!(
                    names(&live),
                    truth_names(&truth),
                    "graph diverged from ground truth"
                );

                let since = if next() % 3 == 0 {
                    CURSOR_PERSIST_INTERVAL
                } else {
                    Duration::from_secs(1)
                };
                let plan = plan_persistence(dirty, false, false, true, true, since, false);
                if plan != PersistPlan::Full {
                    assert_eq!(snapshot, names(&live), "stale snapshot with plan {plan:?}");
                }
                match plan {
                    PersistPlan::Full => {
                        snapshot = names(&live);
                        saved_entries = live.snapshot_entries();
                        saved_cursor = log.len();
                        dirty = false;
                    }
                    PersistPlan::StateOnly => {
                        saved_entries = live.snapshot_entries();
                        saved_cursor = log.len();
                    }
                    PersistPlan::Nothing => {}
                }

                // Random crash: restart from the saved state and replay the journal suffix.
                if next() % 7 == 0 {
                    live = MutableIndex::new(saved_entries.clone());
                    dirty = false;
                    let suffix = log[saved_cursor..].to_vec();
                    let effect = apply_changes_counted(&mut live, suffix);
                    if effect.namespace_changes > 0 {
                        dirty = true;
                    }
                    assert_eq!(
                        names(&live),
                        truth_names(&truth),
                        "replay diverged from ground truth"
                    );
                    if snapshot != names(&live) {
                        assert!(
                            dirty,
                            "snapshot differs from graph after restart but dirty is false"
                        );
                    }
                }
            }
        }
    }
}
