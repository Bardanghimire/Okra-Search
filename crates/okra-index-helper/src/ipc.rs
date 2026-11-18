//! Logon-session-only named-pipe search bridge for the elevated NTFS index helper.
//!
//! Protocol v1 is length-prefixed binary data. The launcher sends a bounded fuzzy query and a
//! result limit; the helper searches its in-memory snapshot cache and returns bounded UTF-8 paths.
//! The pipe DACL grants access only to the interactive logon session and SYSTEM.
//!
//! A second request type, `OKS1`, returns the helper's health (see `status.rs`). Searches are
//! refused with a non-zero `OKR1` status while the helper is starting, stopping, or stale, so
//! the launcher falls through to its next backend instead of showing outdated results.

use okra_core::matcher::{char_mask, lc, score_with_lower, Query};
use std::path::PathBuf;
use std::time::SystemTime;

const REQUEST_MAGIC: &[u8; 4] = b"OKQ1";
const RESPONSE_MAGIC: &[u8; 4] = b"OKR1";
const MAX_QUERY_BYTES: usize = 1024;
const MAX_RESULTS: usize = 200;
const MAX_PATH_BYTES: usize = 128 * 1024;

#[derive(Clone, Debug)]
struct SnapshotEntry {
    path: String,
    name: String,
    lower_name: String,
    mask: u64,
    is_dir: bool,
}

#[derive(Default)]
struct SnapshotCache {
    path: Option<PathBuf>,
    stamp: Option<(u64, Option<SystemTime>)>,
    entries: Vec<SnapshotEntry>,
    /// `masks[i]` is `entries[i].mask`, kept in its own contiguous array so the first filtering
    /// pass streams 8 bytes per entry instead of touching every entry's strings.
    masks: Vec<u64>,
}

impl SnapshotCache {
    fn new(path: PathBuf) -> Self {
        Self {
            path: Some(path),
            ..Self::default()
        }
    }

    fn refresh(&mut self) -> Result<(), String> {
        let Some(path) = self.path.as_ref() else {
            // Unit tests may populate the in-memory entries directly.
            return Ok(());
        };
        let metadata = std::fs::metadata(path)
            .map_err(|error| format!("NTFS snapshot is not available yet: {error}"))?;
        let stamp = (metadata.len(), metadata.modified().ok());
        if self.stamp == Some(stamp) {
            return Ok(());
        }
        let reload_started = std::time::Instant::now();
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("Could not read NTFS snapshot: {error}"))?;
        let mut lines = text.lines();
        let header = lines
            .next()
            .ok_or_else(|| "NTFS snapshot is empty".to_string())?;
        if !header.starts_with("okra-index-v1\t") {
            return Err("NTFS snapshot has an unsupported header".into());
        }
        let mut entries = Vec::new();
        for line in lines {
            let Some((kind, raw_path)) = line.split_once('\t') else {
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
            let name = raw_path
                .rsplit(['\\', '/'])
                .next()
                .filter(|name| !name.is_empty())
                .unwrap_or(raw_path)
                .to_string();
            let lower_name: String = name.chars().map(lc).collect();
            let mask = char_mask(lower_name.chars());
            entries.push(SnapshotEntry {
                path: raw_path.to_string(),
                name,
                lower_name,
                mask,
                is_dir,
            });
        }
        self.entries = entries;
        self.masks = self.entries.iter().map(|entry| entry.mask).collect();
        self.stamp = Some(stamp);
        eprintln!(
            "NTFS IPC snapshot cache loaded: {} paths in {:.1} ms",
            self.entries.len(),
            reload_started.elapsed().as_secs_f64() * 1000.0
        );
        Ok(())
    }

    fn search(&mut self, query_text: &str, max: usize) -> Result<Vec<(String, bool)>, String> {
        self.refresh()?;
        let max = max.clamp(1, MAX_RESULTS);
        let query = Query::new(query_text);
        if query.len() < 2 {
            return Ok(Vec::new());
        }
        if self.masks.len() != self.entries.len() {
            // Unit tests fill `entries` directly, so rebuild the mask array when it is out of step.
            self.masks = self.entries.iter().map(|entry| entry.mask).collect();
        }
        let query_mask = query.mask();
        let mut hits: Vec<(i64, usize)> = Vec::new();
        for (position, &mask) in self.masks.iter().enumerate() {
            if mask & query_mask != query_mask {
                continue;
            }
            let entry = &self.entries[position];
            if let Some(score) = score_with_lower(&query, &entry.name, &entry.lower_name) {
                let rank = score - 10 + if entry.is_dir { 5 } else { 0 };
                hits.push((rank, position));
            }
        }
        let order = |a: &(i64, usize), b: &(i64, usize)| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1));
        if hits.len() > max {
            hits.select_nth_unstable_by(max - 1, order);
            hits.truncate(max);
        }
        hits.sort_by(order);
        Ok(hits
            .into_iter()
            .map(|(_, position)| {
                let entry = &self.entries[position];
                (entry.path.clone(), entry.is_dir)
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
pub fn start_server(snapshot_path: PathBuf) -> Result<(), String> {
    win_pipe::start(snapshot_path)
}

#[cfg(not(windows))]
pub fn start_server(_snapshot_path: PathBuf) -> Result<(), String> {
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

    pub fn start(snapshot_path: PathBuf) -> Result<(), String> {
        let logon_sid = current_logon_sid()?;
        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), String>>(1);
        thread::Builder::new()
            .name("okra-ntfs-ipc".into())
            .spawn(move || match create_pipe(&logon_sid) {
                Ok(pipe) => {
                    eprintln!("NTFS search IPC listening on {PIPE_NAME} (ACL restricted to this logon session and SYSTEM)");
                    let _ = ready_tx.send(Ok(()));
                    server_loop(pipe, snapshot_path);
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

    fn server_loop(pipe: OwnedHandle, snapshot_path: PathBuf) {
        let mut cache = SnapshotCache::new(snapshot_path);

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
        let mut cache = SnapshotCache::new(path.clone());
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
        let mut cache = SnapshotCache {
            entries: vec![
                SnapshotEntry {
                    path: r"C:\Users\me\notes.txt".into(),
                    name: "notes.txt".into(),
                    lower_name: "notes.txt".into(),
                    mask: char_mask("notes.txt".chars()),
                    is_dir: false,
                },
                SnapshotEntry {
                    path: r"C:\Users\me\note-folder".into(),
                    name: "note-folder".into(),
                    lower_name: "note-folder".into(),
                    mask: char_mask("note-folder".chars()),
                    is_dir: true,
                },
                SnapshotEntry {
                    path: r"C:\Users\me\photo.jpg".into(),
                    name: "photo.jpg".into(),
                    lower_name: "photo.jpg".into(),
                    mask: char_mask("photo.jpg".chars()),
                    is_dir: false,
                },
            ],
            ..Default::default()
        };
        assert_eq!(cache.search("note", 10).unwrap().len(), 2);
        assert_eq!(cache.search("photo", 1).unwrap().len(), 1);
        assert!(cache.search("zzzz", 10).unwrap().is_empty());
    }
}
