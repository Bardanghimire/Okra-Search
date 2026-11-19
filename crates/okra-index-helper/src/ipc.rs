//! Logon-session-only named-pipe search bridge for the elevated NTFS index helper.
//!
//! Protocol v1 is length-prefixed binary data. The launcher sends a bounded fuzzy query and a
//! result limit; the helper searches its in-memory snapshot cache and returns bounded UTF-8 paths.
//! The pipe DACL grants access only to the interactive logon session and SYSTEM.
//!
//! A second request type, `OKS1`, returns the helper's health (see `status.rs`). Searches are
//! refused with a non-zero `OKR1` status while the helper is starting, stopping, or stale, so
//! the launcher falls through to its next backend instead of showing outdated results.

use okra_core::matcher::{char_mask, file_name_score, lc, Query};
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::path::PathBuf;
use std::time::SystemTime;

const REQUEST_MAGIC: &[u8; 4] = b"OKQ1";
const RESPONSE_MAGIC: &[u8; 4] = b"OKR1";
const MAX_QUERY_BYTES: usize = 1024;
const MAX_RESULTS: usize = 200;
const MAX_PATH_BYTES: usize = 128 * 1024;

#[derive(Clone, Copy, Debug)]
struct SnapshotEntry {
    path_start: u32,
    path_len: u32,
    name_start: u32,
    lower_start: u32,
    lower_len: u32,
    is_dir: bool,
}

struct SnapshotCache {
    paths: Vec<PathBuf>,
    stamps: Option<Vec<(u64, Option<SystemTime>)>>,
    /// Original UTF-8 paths and lowercase basenames share one compact byte arena. This avoids two
    /// heap allocations per entry and cuts per-entry metadata from two String headers to offsets.
    strings: Vec<u8>,
    entries: Vec<SnapshotEntry>,
    /// Character postings stored bit-sliced: postings[character_bit * words + word] contains
    /// candidate entry bits. This keeps the same ~8 bytes per entry as the old mask array while
    /// allowing each query to intersect 64-entry blocks instead of testing every entry mask.
    postings: Vec<u64>,
    posting_words: usize,
    character_counts: [usize; 64],
}

// Keep the cache compatible with toolchains where arrays longer than 32 elements do not
// implement Default. This also makes the initial contents of the posting counters explicit.
impl Default for SnapshotCache {
    fn default() -> Self {
        Self {
            paths: Vec::new(),
            stamps: None,
            strings: Vec::new(),
            entries: Vec::new(),
            postings: Vec::new(),
            posting_words: 0,
            character_counts: [0; 64],
        }
    }
}

impl SnapshotCache {
    fn new(paths: Vec<PathBuf>) -> Self {
        Self {
            paths,
            ..Self::default()
        }
    }

    fn path_text(&self, entry: &SnapshotEntry) -> &str {
        let start = entry.path_start as usize;
        let end = start + entry.path_len as usize;
        std::str::from_utf8(&self.strings[start..end]).expect("snapshot paths are UTF-8")
    }

    fn name_text<'a>(&'a self, entry: &SnapshotEntry) -> &'a str {
        let path = self.path_text(entry);
        &path[entry.name_start as usize..]
    }

    fn lower_text(&self, entry: &SnapshotEntry) -> &str {
        let start = entry.lower_start as usize;
        let end = start + entry.lower_len as usize;
        std::str::from_utf8(&self.strings[start..end]).expect("lowercase names are UTF-8")
    }

    fn append_path(&mut self, path: &str, is_dir: bool) -> Result<(), String> {
        let name_start = path
            .rfind(['\\', '/'])
            .map(|separator| separator + 1)
            .filter(|start| *start < path.len())
            .unwrap_or(0);
        let path_start = u32::try_from(self.strings.len())
            .map_err(|_| "NTFS search cache exceeds the 4 GiB path arena limit".to_string())?;
        let path_len = u32::try_from(path.len())
            .map_err(|_| "A snapshot path exceeds the 4 GiB path length limit".to_string())?;
        let lower_start = path_start
            .checked_add(path_len)
            .ok_or_else(|| "NTFS search cache path offset overflow".to_string())?;
        let name_start_u32 = u32::try_from(name_start)
            .map_err(|_| "A snapshot basename offset is too large".to_string())?;
        let lower_bytes = path[name_start..]
            .chars()
            .map(|character| lc(character).len_utf8())
            .sum::<usize>();
        let lower_bytes_u32 = u32::try_from(lower_bytes)
            .map_err(|_| "A lowercase basename exceeds the 4 GiB offset limit".to_string())?;
        // The arena uses 32-bit offsets, so validate the *end* of both strings, not just their
        // starting offsets. This prevents the final append from crossing the 4 GiB boundary.
        lower_start
            .checked_add(lower_bytes_u32)
            .ok_or_else(|| "NTFS search cache exceeds the 4 GiB path arena limit".to_string())?;
        let additional = path
            .len()
            .checked_add(lower_bytes)
            .ok_or_else(|| "NTFS search cache string size overflow".to_string())?;
        self.strings
            .try_reserve(additional)
            .map_err(|error| format!("Could not allocate NTFS path strings: {error}"))?;
        self.entries
            .try_reserve(1)
            .map_err(|error| format!("Could not allocate NTFS path metadata: {error}"))?;

        self.strings.extend_from_slice(path.as_bytes());
        let lower_begin = self.strings.len();
        for character in path[name_start..].chars() {
            let lower = lc(character);
            let mut encoded = [0u8; 4];
            self.strings
                .extend_from_slice(lower.encode_utf8(&mut encoded).as_bytes());
        }
        let lower_len = u32::try_from(self.strings.len() - lower_begin)
            .map_err(|_| "A lowercase basename is too long".to_string())?;
        debug_assert_eq!(lower_len as usize, lower_bytes);
        self.entries.push(SnapshotEntry {
            path_start,
            path_len,
            name_start: name_start_u32,
            lower_start,
            lower_len,
            is_dir,
        });
        Ok(())
    }

    fn rebuild_postings(&mut self) -> Result<(), String> {
        let words = self.entries.len().div_ceil(64);
        let slots = words
            .checked_mul(64)
            .ok_or_else(|| "NTFS search posting index size overflow".to_string())?;
        self.postings.clear();
        self.character_counts.fill(0);
        self.postings
            .try_reserve_exact(slots)
            .map_err(|error| format!("Could not allocate NTFS search postings: {error}"))?;
        self.postings.resize(slots, 0);

        for (position, entry) in self.entries.iter().enumerate() {
            let mut mask = char_mask(self.lower_text(entry).chars());
            while mask != 0 {
                let character_bit = mask.trailing_zeros() as usize;
                let slot = character_bit * words + position / 64;
                self.postings[slot] |= 1u64 << (position % 64);
                self.character_counts[character_bit] =
                    self.character_counts[character_bit].saturating_add(1);
                mask &= mask - 1;
            }
        }
        self.posting_words = words;
        Ok(())
    }

    fn memory_bytes(&self) -> usize {
        self.strings.capacity()
            + self.entries.capacity() * std::mem::size_of::<SnapshotEntry>()
            + self.postings.capacity() * std::mem::size_of::<u64>()
            + std::mem::size_of_val(&self.character_counts)
    }

    fn refresh(&mut self) -> Result<(), String> {
        if self.paths.is_empty() {
            // Unit tests may populate the in-memory entries directly.
            return Ok(());
        }
        let mut stamps = Vec::with_capacity(self.paths.len());
        for path in &self.paths {
            let metadata = std::fs::metadata(path).map_err(|error| {
                format!("NTFS snapshot {} is not available: {error}", path.display())
            })?;
            stamps.push((metadata.len(), metadata.modified().ok()));
        }
        if self.stamps.as_ref() == Some(&stamps) {
            return Ok(());
        }
        use std::io::BufRead;

        let reload_started = std::time::Instant::now();
        // Reuse the same arena allocations on refresh; the old combined cache is released before
        // the replacement snapshots are read, avoiding a second all-volume cache during updates.
        self.strings.clear();
        self.entries.clear();
        self.postings.clear();
        self.posting_words = 0;
        self.character_counts.fill(0);
        self.stamps = None;
        // Clone only the small configured volume-path list so append_path can mutably update
        // the cache while each snapshot is streamed. The large path index is not cloned.
        let snapshot_paths = self.paths.clone();
        for path in &snapshot_paths {
            let file = std::fs::File::open(path).map_err(|error| {
                format!("Could not open NTFS snapshot {}: {error}", path.display())
            })?;
            let mut reader = std::io::BufReader::new(file);
            let mut line = String::new();
            if reader
                .read_line(&mut line)
                .map_err(|error| format!("Could not read NTFS snapshot header: {error}"))?
                == 0
            {
                return Err(format!("NTFS snapshot {} is empty", path.display()));
            }
            let header = line.trim_end_matches(['\r', '\n']);
            if !header.starts_with("okra-index-v1\t") {
                return Err(format!(
                    "NTFS snapshot {} has an unsupported header",
                    path.display()
                ));
            }
            loop {
                line.clear();
                if reader
                    .read_line(&mut line)
                    .map_err(|error| format!("Could not read NTFS snapshot: {error}"))?
                    == 0
                {
                    break;
                }
                let row = line.trim_end_matches(['\r', '\n']);
                let Some((kind, raw_path)) = row.split_once('\t') else {
                    continue;
                };
                if raw_path.is_empty()
                    || raw_path.len() > MAX_PATH_BYTES
                    || raw_path.contains(['\n', '\r', '\t'])
                {
                    continue;
                }
                let is_dir = match kind {
                    "D" => true,
                    "F" => false,
                    _ => continue,
                };
                self.append_path(raw_path, is_dir)?;
                if self.entries.len() % 250_000 == 0 {
                    eprintln!(
                        "NTFS IPC snapshot load: {} paths parsed ...",
                        self.entries.len()
                    );
                }
            }
        }
        self.rebuild_postings()?;
        self.stamps = Some(stamps);
        eprintln!(
            "NTFS IPC snapshot cache loaded: {} paths from {} volume snapshot(s), {:.1} MiB compact index, {:.1} ms",
            self.entries.len(),
            self.paths.len(),
            self.memory_bytes() as f64 / (1024.0 * 1024.0),
            reload_started.elapsed().as_secs_f64() * 1000.0
        );
        Ok(())
    }

    fn search(&mut self, query_text: &str, max: usize) -> Result<Vec<(String, bool)>, String> {
        self.refresh()?;
        let max = max.clamp(1, MAX_RESULTS);
        let query = Query::new(query_text);
        if query.len() < 2 || self.entries.is_empty() {
            return Ok(Vec::new());
        }
        let words = self.entries.len().div_ceil(64);
        if self.posting_words != words || self.postings.len() != words * 64 {
            self.rebuild_postings()?;
        }

        // Keep only the best `max` results while scanning. A bounded heap avoids allocating a
        // potentially huge Vec of every fuzzy match for broad two-character queries.
        let mut best: BinaryHeap<Reverse<(i64, Reverse<usize>)>> = BinaryHeap::with_capacity(max);
        // Intersect the rarest query-character postings first. Empty blocks then short-circuit
        // before the common letters are touched, which is especially useful for short queries.
        let mut required_bits = [0u8; 64];
        let mut required_len = 0usize;
        let mut required = query.mask();
        while required != 0 {
            required_bits[required_len] = required.trailing_zeros() as u8;
            required_len += 1;
            required &= required - 1;
        }
        required_bits[..required_len]
            .sort_unstable_by_key(|bit| self.character_counts[*bit as usize]);

        let mut word = 0usize;
        while word < words {
            let mut candidate_word = u64::MAX;
            for character_bit in &required_bits[..required_len] {
                candidate_word &= self.postings[*character_bit as usize * words + word];
                if candidate_word == 0 {
                    break;
                }
            }
            while candidate_word != 0 {
                let bit = candidate_word.trailing_zeros() as usize;
                let position = word * 64 + bit;
                candidate_word &= candidate_word - 1;
                if position >= self.entries.len() {
                    continue;
                }
                let entry = &self.entries[position];
                if let Some(rank) = file_name_score(
                    &query,
                    self.name_text(entry),
                    self.lower_text(entry),
                    entry.is_dir,
                ) {
                    let quality = (rank, Reverse(position));
                    let candidate = Reverse(quality);
                    if best.len() < max {
                        best.push(candidate);
                    } else if best.peek().is_some_and(|worst| quality > worst.0) {
                        best.pop();
                        best.push(candidate);
                    }
                }
            }
            word += 1;
        }

        let mut hits: Vec<(i64, usize)> = best
            .into_iter()
            .map(|Reverse((rank, Reverse(position)))| (rank, position))
            .collect();
        hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        Ok(hits
            .into_iter()
            .map(|(_, position)| {
                let entry = &self.entries[position];
                (self.path_text(entry).to_owned(), entry.is_dir)
            })
            .collect())
    }
}

fn read_u16(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn parse_request(header: &[u8], body: &[u8]) -> Result<(String, usize), String> {
    if header.len() != 8 || &header[..4] != REQUEST_MAGIC {
        return Err("Invalid Okra IPC request header".into());
    }
    let max = read_u16(header, 4).ok_or_else(|| "Truncated result limit".to_string())? as usize;
    let query_len =
        read_u16(header, 6).ok_or_else(|| "Truncated query length".to_string())? as usize;
    if !(1..=MAX_RESULTS).contains(&max) {
        return Err("IPC result limit must be between 1 and 200".into());
    }
    if query_len == 0 || query_len > MAX_QUERY_BYTES || body.len() != query_len {
        return Err("IPC query length is invalid".into());
    }
    let query =
        std::str::from_utf8(body).map_err(|_| "IPC query is not valid UTF-8".to_string())?;
    if query.chars().any(char::is_control) {
        return Err("IPC query contains control characters".into());
    }
    Ok((query.to_string(), max))
}

fn encode_response(results: &[(String, bool)], status: u16) -> Result<Vec<u8>, String> {
    let valid: Vec<(&[u8], bool)> = if status == 0 {
        results
            .iter()
            .filter_map(|(path, is_dir)| {
                let bytes = path.as_bytes();
                (!bytes.is_empty() && bytes.len() <= MAX_PATH_BYTES).then_some((bytes, *is_dir))
            })
            .take(MAX_RESULTS)
            .collect()
    } else {
        Vec::new()
    };
    let mut bytes = Vec::new();
    bytes.extend_from_slice(RESPONSE_MAGIC);
    bytes.extend_from_slice(&status.to_le_bytes());
    bytes.extend_from_slice(&(valid.len() as u16).to_le_bytes());
    for (path, is_dir) in valid {
        bytes.push(u8::from(is_dir));
        bytes.extend_from_slice(&(path.len() as u32).to_le_bytes());
        bytes.extend_from_slice(path);
    }
    Ok(bytes)
}

#[cfg(windows)]
pub fn start_server(snapshot_paths: Vec<PathBuf>) -> Result<(), String> {
    win_pipe::start(snapshot_paths)
}

#[cfg(not(windows))]
pub fn start_server(_snapshot_paths: Vec<PathBuf>) -> Result<(), String> {
    Err("The Okra named-pipe server is available only in the Windows build".into())
}

#[cfg(windows)]
mod win_pipe {
    use super::{encode_response, parse_request, SnapshotCache, MAX_QUERY_BYTES};
    use crate::status::{self, STATUS_REQUEST_MAGIC};
    use std::ffi::c_void;
    use std::mem::size_of;
    use std::path::PathBuf;
    use std::ptr;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    const PIPE_NAME: &str = r"\\.\pipe\okra-brown-search-v1";
    const TOKEN_QUERY: u32 = 0x0008;
    const TOKEN_ADJUST_PRIVILEGES: u32 = 0x0020;
    const TOKEN_GROUPS_CLASS: i32 = 2;
    const SE_PRIVILEGE_ENABLED: u32 = 0x0002;
    const ERROR_NOT_ALL_ASSIGNED: u32 = 1300;
    const SE_GROUP_LOGON_ID: u32 = 0xC000_0000;
    const PIPE_ACCESS_DUPLEX: u32 = 0x00000003;
    const FILE_FLAG_FIRST_PIPE_INSTANCE: u32 = 0x00080000;
    const PIPE_TYPE_BYTE: u32 = 0x00000000;
    const PIPE_READMODE_BYTE: u32 = 0x00000000;
    const PIPE_WAIT: u32 = 0x00000000;
    const PIPE_REJECT_REMOTE_CLIENTS: u32 = 0x00000008;
    const ERROR_PIPE_CONNECTED: u32 = 535;
    const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;

    type Handle = *mut c_void;

    #[repr(C)]
    struct SecurityAttributes {
        length: u32,
        descriptor: *mut c_void,
        inherit_handle: i32,
    }

    #[repr(C)]
    struct SidAndAttributes {
        sid: *mut c_void,
        attributes: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Luid {
        low_part: u32,
        high_part: i32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct LuidAndAttributes {
        luid: Luid,
        attributes: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct TokenPrivileges {
        privilege_count: u32,
        privileges: [LuidAndAttributes; 1],
    }

    #[link(name = "advapi32")]
    extern "system" {
        fn OpenProcessToken(process: Handle, access: u32, token: *mut Handle) -> i32;
        fn LookupPrivilegeValueW(system_name: *const u16, name: *const u16, luid: *mut Luid)
            -> i32;
        fn AdjustTokenPrivileges(
            token: Handle,
            disable_all: i32,
            new_state: *const TokenPrivileges,
            buffer_length: u32,
            previous_state: *mut TokenPrivileges,
            return_length: *mut u32,
        ) -> i32;
        fn GetTokenInformation(
            token: Handle,
            class: i32,
            information: *mut c_void,
            information_len: u32,
            return_len: *mut u32,
        ) -> i32;
        fn ConvertSidToStringSidW(sid: *mut c_void, string_sid: *mut *mut u16) -> i32;
        fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
            descriptor: *const u16,
            revision: u32,
            output: *mut *mut c_void,
            size: *mut u32,
        ) -> i32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentProcess() -> Handle;
        fn GetLastError() -> u32;
        fn CloseHandle(handle: Handle) -> i32;
        fn LocalFree(memory: Handle) -> Handle;
        fn CreateNamedPipeW(
            name: *const u16,
            open_mode: u32,
            pipe_mode: u32,
            max_instances: u32,
            out_buffer_size: u32,
            in_buffer_size: u32,
            default_timeout: u32,
            security_attributes: *const SecurityAttributes,
        ) -> Handle;
        fn ConnectNamedPipe(pipe: Handle, overlapped: *mut c_void) -> i32;
        fn DisconnectNamedPipe(pipe: Handle) -> i32;
        fn FlushFileBuffers(file: Handle) -> i32;
        fn ReadFile(
            file: Handle,
            buffer: *mut u8,
            bytes_to_read: u32,
            bytes_read: *mut u32,
            overlapped: *mut c_void,
        ) -> i32;

        fn WriteFile(
            file: Handle,
            buffer: *const u8,
            bytes_to_write: u32,
            bytes_written: *mut u32,
            overlapped: *mut c_void,
        ) -> i32;
    }

    struct OwnedHandle(Handle);
    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }
    }

    struct LocalMemory(Handle);
    impl Drop for LocalMemory {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    LocalFree(self.0);
                }
            }
        }
    }

    /// Enables SeSecurityPrivilege only while CreateNamedPipe applies the mandatory integrity
    /// label in the new object's SACL. The prior privilege state is restored when this guard drops.
    struct SecurityPrivilegeGuard {
        token: OwnedHandle,
        previous: TokenPrivileges,
    }

    impl Drop for SecurityPrivilegeGuard {
        fn drop(&mut self) {
            let restored = unsafe {
                AdjustTokenPrivileges(
                    self.token.0,
                    0,
                    &self.previous,
                    0,
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            };
            if restored == 0 {
                eprintln!(
                    "Warning: could not restore the helper token's previous SeSecurityPrivilege state (Windows error {}).",
                    unsafe { GetLastError() }
                );
            }
        }
    }

    fn enable_security_privilege() -> Result<SecurityPrivilegeGuard, String> {
        let mut token = ptr::null_mut();
        if unsafe {
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_QUERY | TOKEN_ADJUST_PRIVILEGES,
                &mut token,
            )
        } == 0
        {
            return Err(format!(
                "Could not open helper token to set the pipe integrity label (Windows error {})",
                unsafe { GetLastError() }
            ));
        }
        let token = OwnedHandle(token);
        let name = "SeSecurityPrivilege"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let mut luid = Luid {
            low_part: 0,
            high_part: 0,
        };
        if unsafe { LookupPrivilegeValueW(ptr::null(), name.as_ptr(), &mut luid) } == 0 {
            return Err(format!(
                "Could not look up SeSecurityPrivilege (Windows error {})",
                unsafe { GetLastError() }
            ));
        }

        let enabled = TokenPrivileges {
            privilege_count: 1,
            privileges: [LuidAndAttributes {
                luid,
                attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        let mut previous = TokenPrivileges {
            privilege_count: 0,
            privileges: [LuidAndAttributes {
                luid: Luid {
                    low_part: 0,
                    high_part: 0,
                },
                attributes: 0,
            }],
        };
        let mut returned = 0u32;
        if unsafe {
            AdjustTokenPrivileges(
                token.0,
                0,
                &enabled,
                size_of::<TokenPrivileges>() as u32,
                &mut previous,
                &mut returned,
            )
        } == 0
        {
            return Err(format!(
                "Could not enable SeSecurityPrivilege for pipe creation (Windows error {})",
                unsafe { GetLastError() }
            ));
        }
        if unsafe { GetLastError() } == ERROR_NOT_ALL_ASSIGNED {
            return Err(
                "The elevated helper token does not have SeSecurityPrivilege to label the pipe"
                    .into(),
            );
        }
        Ok(SecurityPrivilegeGuard { token, previous })
    }

    fn current_logon_sid() -> Result<String, String> {
        let mut token = ptr::null_mut();
        let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
        if opened == 0 {
            return Err(format!(
                "Could not open helper process token (Windows error {})",
                unsafe { GetLastError() }
            ));
        }
        let token = OwnedHandle(token);
        let mut needed = 0u32;
        unsafe {
            GetTokenInformation(token.0, TOKEN_GROUPS_CLASS, ptr::null_mut(), 0, &mut needed);
        }
        if needed < size_of::<u32>() as u32 {
            return Err(format!(
                "Could not size token-group information (Windows error {})",
                unsafe { GetLastError() }
            ));
        }
        let words = (needed as usize + size_of::<usize>() - 1) / size_of::<usize>();
        let mut storage = vec![0usize; words];
        let storage_bytes = (storage.len() * size_of::<usize>()) as u32;
        let ok = unsafe {
            GetTokenInformation(
                token.0,
                TOKEN_GROUPS_CLASS,
                storage.as_mut_ptr() as *mut c_void,
                storage_bytes,
                &mut needed,
            )
        };
        if ok == 0 {
            return Err(format!(
                "Could not read token-group information (Windows error {})",
                unsafe { GetLastError() }
            ));
        }

        let base = storage.as_ptr() as *const u8;
        let group_count = unsafe { ptr::read_unaligned(base as *const u32) } as usize;
        let alignment = std::mem::align_of::<SidAndAttributes>();
        let groups_offset = (size_of::<u32>() + alignment - 1) & !(alignment - 1);
        if groups_offset > needed as usize
            || group_count > (needed as usize - groups_offset) / size_of::<SidAndAttributes>()
        {
            return Err("Windows returned a malformed token-group list".into());
        }

        let mut logon_sid = ptr::null_mut();
        for index in 0..group_count {
            let group = unsafe {
                &*(base.add(groups_offset + index * size_of::<SidAndAttributes>())
                    as *const SidAndAttributes)
            };
            if group.attributes & SE_GROUP_LOGON_ID == SE_GROUP_LOGON_ID {
                logon_sid = group.sid;
                break;
            }
        }
        if logon_sid.is_null() {
            return Err("The elevated helper token has no logon-session SID".into());
        }

        let mut sid_text = ptr::null_mut();
        if unsafe { ConvertSidToStringSidW(logon_sid, &mut sid_text) } == 0 {
            return Err(format!(
                "Could not convert logon-session SID (Windows error {})",
                unsafe { GetLastError() }
            ));
        }
        let sid_memory = LocalMemory(sid_text as Handle);
        let mut length = 0usize;
        unsafe {
            while *sid_text.add(length) != 0 {
                length += 1;
            }
        }
        let sid = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(sid_text, length) });
        drop(sid_memory);
        drop(token);
        Ok(sid)
    }

    fn read_exact(pipe: Handle, mut output: &mut [u8]) -> Result<(), String> {
        while !output.is_empty() {
            let mut read = 0u32;
            let ok = unsafe {
                ReadFile(
                    pipe,
                    output.as_mut_ptr(),
                    output.len().min(u32::MAX as usize) as u32,
                    &mut read,
                    ptr::null_mut(),
                )
            };
            if ok == 0 || read == 0 {
                return Err(format!(
                    "Could not read Okra IPC request (Windows error {})",
                    unsafe { GetLastError() }
                ));
            }
            let (_, rest) = output.split_at_mut(read as usize);
            output = rest;
        }
        Ok(())
    }

    fn write_all(pipe: Handle, mut input: &[u8]) -> Result<(), String> {
        while !input.is_empty() {
            let mut written = 0u32;
            let ok = unsafe {
                WriteFile(
                    pipe,
                    input.as_ptr(),
                    input.len().min(u32::MAX as usize) as u32,
                    &mut written,
                    ptr::null_mut(),
                )
            };
            if ok == 0 || written == 0 {
                return Err(format!(
                    "Could not write Okra IPC response (Windows error {})",
                    unsafe { GetLastError() }
                ));
            }
            input = &input[written as usize..];
        }
        Ok(())
    }

    fn serve_one(pipe: Handle, cache: &mut SnapshotCache) -> Result<(), String> {
        let mut header = [0u8; 8];
        read_exact(pipe, &mut header)?;
        if &header[..4] == STATUS_REQUEST_MAGIC {
            let reply = status::encode_status(&status::global().snapshot(), std::process::id());
            return write_all(pipe, &reply);
        }
        if let Err(code) = status::searchable(&status::global().snapshot(), status::now_unix()) {
            // Drain nothing: the client reads only the 8-byte refusal, and the pipe is
            // disconnected right after this request.
            eprintln!("NTFS IPC search refused: helper is not serving (refusal code {code})");
            return write_all(pipe, &encode_response(&[], code)?);
        }
        let query_len = u16::from_le_bytes([header[6], header[7]]) as usize;
        if query_len == 0 || query_len > MAX_QUERY_BYTES {
            let response = encode_response(&[], 1)?;
            return write_all(pipe, &response);
        }
        let mut body = vec![0u8; query_len];
        read_exact(pipe, &mut body)?;
        let search_started = std::time::Instant::now();
        let response = match parse_request(&header, &body) {
            Ok((query, max)) => match cache.search(&query, max) {
                Ok(results) => {
                    eprintln!(
                        "NTFS IPC search {query:?}: {} result(s) in {:.2} ms",
                        results.len(),
                        search_started.elapsed().as_secs_f64() * 1000.0
                    );
                    encode_response(&results, 0)?
                }
                Err(error) => {
                    eprintln!("NTFS IPC query unavailable: {error}");
                    encode_response(&[], 1)?
                }
            },
            Err(error) => {
                eprintln!("Rejected invalid NTFS IPC request: {error}");
                encode_response(&[], 1)?
            }
        };
        write_all(pipe, &response)
    }

    pub fn start(snapshot_paths: Vec<PathBuf>) -> Result<(), String> {
        let logon_sid = current_logon_sid()?;
        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), String>>(1);
        thread::Builder::new()
            .name("okra-ntfs-ipc".into())
            .spawn(move || match create_pipe(&logon_sid) {
                Ok(pipe) => {
                    eprintln!("NTFS search IPC listening on {PIPE_NAME} (ACL restricted to this logon session and SYSTEM)");
                    let _ = ready_tx.send(Ok(()));
                    server_loop(pipe, snapshot_paths);
                }
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                }
            })
            .map_err(|error| format!("Could not start NTFS IPC server thread: {error}"))?;

        ready_rx
            .recv()
            .map_err(|_| "NTFS IPC server stopped before creating its named pipe".to_string())?
    }

    fn create_pipe(logon_sid: &str) -> Result<OwnedHandle, String> {
        let pipe_name = PIPE_NAME
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        // Medium integrity lets the unelevated desktop app write requests to this pipe; the
        // protected DACL still limits access to this exact logon session and SYSTEM.
        let sddl = format!("D:P(A;;GA;;;SY)(A;;GA;;;{logon_sid})S:(ML;;NW;;;ME)");
        let sddl_wide = sddl
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let mut descriptor = ptr::null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl_wide.as_ptr(),
                1,
                &mut descriptor,
                ptr::null_mut(),
            )
        } == 0
        {
            return Err(format!(
                "Could not create logon-session pipe ACL (Windows error {})",
                unsafe { GetLastError() }
            ));
        }
        let descriptor = LocalMemory(descriptor);
        let security = SecurityAttributes {
            length: size_of::<SecurityAttributes>() as u32,
            descriptor: descriptor.0,
            inherit_handle: 0,
        };

        // Creating an object with a SACL requires SeSecurityPrivilege. Enable it only for the
        // CreateNamedPipe call, then restore the token's previous privilege state immediately.
        let privilege = enable_security_privilege()?;
        let pipe = unsafe {
            CreateNamedPipeW(
                pipe_name.as_ptr(),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                64 * 1024,
                2048,
                0,
                &security,
            )
        };
        let pipe_error = if pipe.is_null() || pipe == INVALID_HANDLE_VALUE {
            Some(unsafe { GetLastError() })
        } else {
            None
        };
        drop(privilege);
        if let Some(error) = pipe_error {
            return Err(format!(
                "Could not securely claim the Okra named pipe (Windows error {error}). The IPC server will not start."
            ));
        }
        Ok(OwnedHandle(pipe))
    }

    fn server_loop(pipe: OwnedHandle, snapshot_paths: Vec<PathBuf>) {
        let mut cache = SnapshotCache::new(snapshot_paths);

        // Reuse the same instance between clients. Keeping the handle alive prevents a gap in
        // which another process could take ownership of the pipe name.
        loop {
            let connected = unsafe { ConnectNamedPipe(pipe.0, ptr::null_mut()) };
            if connected == 0 {
                let error = unsafe { GetLastError() };
                if error != ERROR_PIPE_CONNECTED {
                    eprintln!("Could not accept Okra named-pipe client (Windows error {error}).");
                    unsafe {
                        DisconnectNamedPipe(pipe.0);
                    }
                    thread::sleep(Duration::from_millis(100));
                    continue;
                }
            }
            if let Err(error) = serve_one(pipe.0, &mut cache) {
                eprintln!("Okra named-pipe request failed: {error}");
            }
            // DisconnectNamedPipe discards data the client has not read yet. Wait for the
            // client to read the whole reply first, or fast replies (such as status) are lost.
            unsafe {
                FlushFileBuffers(pipe.0);
            }
            unsafe {
                DisconnectNamedPipe(pipe.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_parser_checks_magic_lengths_and_limits() {
        let mut header = Vec::from(&REQUEST_MAGIC[..]);
        header.extend_from_slice(&50u16.to_le_bytes());
        header.extend_from_slice(&4u16.to_le_bytes());
        assert_eq!(
            parse_request(&header, b"note").unwrap(),
            ("note".into(), 50)
        );
        assert!(parse_request(b"BAD!\x32\x00\x04\x00", b"note").is_err());
        assert!(parse_request(b"OKQ1\x00\x00\x04\x00", b"note").is_err());
        assert!(parse_request(&header, b"no").is_err());
        assert!(parse_request(&header, b"no\n!").is_err());

        let response = encode_response(&[(r"C:\notes.txt".into(), false)], 0).unwrap();
        assert_eq!(&response[..4], RESPONSE_MAGIC);
        assert_eq!(u16::from_le_bytes([response[4], response[5]]), 0);
        assert_eq!(u16::from_le_bytes([response[6], response[7]]), 1);
        assert_eq!(response[8], 0);
        assert_eq!(u32::from_le_bytes(response[9..13].try_into().unwrap()), 12);
        assert_eq!(&response[13..], br"C:\notes.txt");
    }

    #[test]
    fn snapshot_cache_loads_the_existing_tsv_format() {
        let path =
            std::env::temp_dir().join(format!("okra-ipc-snapshot-{}.tsv", std::process::id()));
        std::fs::write(
            &path,
            "okra-index-v1\t8\tC:\\Users\\me\nF\tC:\\Users\\me\\notes.txt\nD\tC:\\Users\\me\\note-folder\n",
        )
        .unwrap();
        let mut cache = SnapshotCache::new(vec![path.clone()]);
        let results = cache.search("note", 10).unwrap();
        assert_eq!(results.len(), 2);
        assert!(results
            .iter()
            .any(
                |(path, is_dir)| path.rsplit(['\\', '/']).next() == Some("notes.txt") && !*is_dir
            ));
        assert!(results
            .iter()
            .any(
                |(path, is_dir)| path.rsplit(['\\', '/']).next() == Some("note-folder") && *is_dir
            ));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn snapshot_search_matches_file_names_and_respects_the_limit() {
        let mut cache = SnapshotCache::default();
        cache.append_path(r"C:\Users\me\notes.txt", false).unwrap();
        cache.append_path(r"C:\Users\me\note-folder", true).unwrap();
        cache.append_path(r"C:\Users\me\photo.jpg", false).unwrap();
        cache.rebuild_postings().unwrap();
        assert_eq!(cache.search("note", 10).unwrap().len(), 2);
        assert_eq!(cache.search("photo", 1).unwrap().len(), 1);
        assert!(cache.search("zzzz", 10).unwrap().is_empty());
    }

    #[test]
    fn exact_filename_ranks_above_same_named_directory() {
        let mut cache = SnapshotCache::default();
        cache.append_path(r"C:\Hosts", true).unwrap();
        cache
            .append_path(r"C:\Windows\System32\drivers\etc\hosts", false)
            .unwrap();
        cache.rebuild_postings().unwrap();
        let results = cache.search("hosts", 10).unwrap();
        assert_eq!(results[0].0, r"C:\Windows\System32\drivers\etc\hosts");
        assert!(!results[0].1);
    }

    #[test]
    fn snapshot_cache_merges_multiple_volume_snapshots() {
        let suffix = std::process::id();
        let c_path = std::env::temp_dir().join(format!("okra-ipc-C-{suffix}.tsv"));
        let d_path = std::env::temp_dir().join(format!("okra-ipc-D-{suffix}.tsv"));
        std::fs::write(
            &c_path,
            concat!(
                "okra-index-v1\t18446744073709551615\tC:\\",
                "\nF\tC:\\Windows\\hosts\n"
            ),
        )
        .unwrap();
        std::fs::write(
            &d_path,
            concat!(
                "okra-index-v1\t18446744073709551615\tD:\\",
                "\nF\tD:\\Projects\\hosts\n"
            ),
        )
        .unwrap();
        let mut cache = SnapshotCache::new(vec![c_path.clone(), d_path.clone()]);
        let results = cache.search("hosts", 10).unwrap();
        assert_eq!(results.len(), 2);
        assert!(results.iter().any(|(path, _)| path.starts_with("C:\\")));
        assert!(results.iter().any(|(path, _)| path.starts_with("D:\\")));
        let _ = std::fs::remove_file(c_path);
        let _ = std::fs::remove_file(d_path);
    }

    #[test]
    fn bitset_postings_do_not_drop_unicode_or_repeated_character_candidates() {
        let mut cache = SnapshotCache::default();
        cache.append_path(r"C:\Users\me\résumé.txt", false).unwrap();
        cache.append_path(r"C:\Users\me\cacao.txt", false).unwrap();
        cache
            .append_path(r"C:\Users\me\unrelated.bin", false)
            .unwrap();
        cache.rebuild_postings().unwrap();
        assert!(cache
            .search("résumé", 10)
            .unwrap()
            .iter()
            .any(|(path, _)| path.ends_with("résumé.txt")));
        assert!(cache
            .search("cacao", 10)
            .unwrap()
            .iter()
            .any(|(path, _)| path.ends_with("cacao.txt")));
    }

    #[test]
    fn bounded_top_k_keeps_only_the_requested_result_count() {
        let mut cache = SnapshotCache::default();
        for index in 0..10_000 {
            cache
                .append_path(&format!(r"C:\synthetic\cargo-{index:05}.toml"), false)
                .unwrap();
        }
        cache.rebuild_postings().unwrap();
        let results = cache.search("cargo", 17).unwrap();
        assert_eq!(results.len(), 17);
        assert!(results.iter().all(|(path, _)| path.contains("cargo-")));
    }

    /// `cargo test --release -p okra-index-helper -- --ignored --nocapture bench_million_entry_search`
    #[test]
    #[ignore]
    fn bench_million_entry_search() {
        let mut cache = SnapshotCache::default();
        for index in 0..1_000_000 {
            let name = match index % 8 {
                0 => format!(r"C:\Users\bench\Cargo-{index:07}.toml"),
                1 => format!(r"C:\Program Files\Chrome\Chrome-{index:07}.exe"),
                2 => format!(r"C:\Windows\System32\Hosts-{index:07}.dll"),
                _ => format!(r"C:\Data\Archive\document-{index:07}.txt"),
            };
            cache.append_path(&name, false).unwrap();
        }
        cache.rebuild_postings().unwrap();
        eprintln!(
            "synthetic index: {:.1} MiB reserved compact-index memory for {} entries",
            cache.memory_bytes() as f64 / (1024.0 * 1024.0),
            cache.entries.len()
        );
        for query in ["cargo", "chrome", "hosts", "document", "zzqxjkvw"] {
            // Warm once, then collect a small latency distribution rather than reporting a
            // single noisy sample. This measures in-memory query cost, not cold TSV loading.
            let _ = cache.search(query, 200).unwrap();
            let mut samples_ms = Vec::with_capacity(25);
            let mut result_count = 0usize;
            for _ in 0..25 {
                let started = std::time::Instant::now();
                let hits = cache.search(query, 200).unwrap();
                samples_ms.push(started.elapsed().as_secs_f64() * 1000.0);
                result_count = hits.len();
            }
            samples_ms.sort_by(f64::total_cmp);
            let median = samples_ms[samples_ms.len() / 2];
            let p95 = samples_ms[(samples_ms.len() * 95 / 100).min(samples_ms.len() - 1)];
            eprintln!("{query}: {result_count} results; median {median:.2} ms; p95 {p95:.2} ms");
        }
    }
}
