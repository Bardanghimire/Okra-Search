//! NTFS Master File Table snapshot reader.
//!
//! MFT enumeration and the existing cache snapshot format. The journal module builds and
//! incrementally updates a persistent FRN graph using this snapshot reader.

use std::collections::HashMap;
use std::path::Path;

const CACHE_MAGIC: &str = "okra-index-v1";
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
const NTFS_ROOT_FILE_ID: u64 = 5;
const USN_REASON_FILE_DELETE: u32 = 0x0000_0200;
const FILE_ID_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

#[derive(Debug, Clone)]
pub(super) struct MftEntry {
    pub(super) file_reference: u64,
    pub(super) parent_reference: u64,
    pub(super) name: String,
    pub(super) is_dir: bool,
}

/// Parse the output format of FSCTL_ENUM_USN_DATA: an eight-byte continuation FRN followed by
/// zero or more variable-length USN_RECORD_V2 structures.
fn parse_enum_buffer(buffer: &[u8]) -> Result<(u64, Vec<MftEntry>), String> {
    if buffer.len() < 8 {
        return Err("FSCTL_ENUM_USN_DATA returned a truncated continuation record".into());
    }
    let next_reference = read_u64(buffer, 0)?;
    let mut entries = Vec::new();
    let mut offset = 8usize;

    while offset < buffer.len() {
        if buffer.len() - offset < 8 {
            return Err("Truncated USN record header".into());
        }
        let record_length = read_u32(buffer, offset)? as usize;
        if record_length < 60 || record_length > buffer.len() - offset {
            return Err(format!("Invalid USN record length {record_length}"));
        }
        let major = read_u16(buffer, offset + 4)?;
        if major == 2 {
            let file_reference = read_u64(buffer, offset + 8)?;
            let parent_reference = read_u64(buffer, offset + 16)?;
            let reason = read_u32(buffer, offset + 40)?;
            let attributes = read_u32(buffer, offset + 52)?;
            let name_length = read_u16(buffer, offset + 56)? as usize;
            let name_offset = read_u16(buffer, offset + 58)? as usize;
            if name_length % 2 != 0 || (name_length > 0 && name_offset < 60) {
                return Err("Invalid UTF-16 filename fields in USN record".into());
            }
            let name_end = name_offset
                .checked_add(name_length)
                .ok_or_else(|| "USN filename range overflow".to_string())?;
            if name_length > 0 && name_end > record_length {
                return Err("USN filename extends beyond its record".into());
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
            let name = String::from_utf16_lossy(&wide);
            // MFT enumeration can return a record whose final reason is deletion. It must not be
            // resurrected into the filename index.
            if reason & USN_REASON_FILE_DELETE == 0
                && (!name.is_empty() || file_reference & 0x0000_FFFF_FFFF_FFFF == NTFS_ROOT_FILE_ID)
            {
                entries.push(MftEntry {
                    file_reference,
                    parent_reference,
                    name,
                    is_dir: attributes & FILE_ATTRIBUTE_DIRECTORY != 0,
                });
            }
        }
        // NTFS uses USN_RECORD_V2. Other record versions are skipped rather than misparsed.
        offset += record_length;
    }

    Ok((next_reference, entries))
}

fn read_u16(bytes: &[u8], at: usize) -> Result<u16, String> {
    let value = bytes.get(at..at + 2).ok_or_else(|| "Truncated USN field".to_string())?;
    Ok(u16::from_le_bytes([value[0], value[1]]))
}

fn read_u32(bytes: &[u8], at: usize) -> Result<u32, String> {
    let value = bytes.get(at..at + 4).ok_or_else(|| "Truncated USN field".to_string())?;
    Ok(u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
}

fn read_u64(bytes: &[u8], at: usize) -> Result<u64, String> {
    let value = bytes.get(at..at + 8).ok_or_else(|| "Truncated USN field".to_string())?;
    Ok(u64::from_le_bytes([
        value[0], value[1], value[2], value[3], value[4], value[5], value[6], value[7],
    ]))
}

fn volume_root(volume: &str) -> String {
    format!("{}\\", volume.trim_end_matches(['\\', '/']))
}

pub(super) fn validate_volume_and_roots(volume: &str, roots: &[String]) -> Result<(), String> {
    let bytes = volume.as_bytes();
    if bytes.len() != 2 || !bytes[0].is_ascii_alphabetic() || bytes[1] != b':' {
        return Err("--volume must be a drive letter such as C:".into());
    }
    let prefix = volume_root(volume);
    for raw_root in roots {
        if !raw_root.get(..2).is_some_and(|drive| drive.eq_ignore_ascii_case(volume))
            || raw_root.get(2..3) != Some("\\")
        {
            return Err(format!("Root {raw_root:?} is not an absolute path on {volume}"));
        }
        let root = raw_root.trim_end_matches(['\\', '/']);
        if root.len() < prefix.len() - 1 {
            return Err(format!("Invalid root path: {raw_root}"));
        }
    }
    Ok(())
}

fn path_from_components(root: &str, components: &[&str]) -> String {
    let mut path = root.to_string();
    for component in components.iter().rev() {
        if !path.ends_with('\\') {
            path.push('\\');
        }
        path.push_str(component);
    }
    path
}

fn resolve_path(
    start: usize,
    entries: &[MftEntry],
    by_reference: &HashMap<u64, usize>,
    root: &str,
) -> Option<String> {
    let mut components: Vec<&str> = Vec::new();
    let mut current = start;
    // NTFS directory parent chains should be shallow. The explicit bound also prevents a corrupt
    // or stale MFT reference from creating an infinite loop.
    for _ in 0..512 {
        let entry = entries.get(current)?;
        if entry.file_reference & FILE_ID_MASK == NTFS_ROOT_FILE_ID {
            return Some(path_from_components(root, &components));
        }
        components.push(&entry.name);

        // FSCTL_ENUM_USN_DATA does not guarantee that the volume-root record itself is present in
        // the returned set. A direct parent reference to FRN 5 is enough to anchor this chain at
        // the volume root; requiring a separate root record makes every path unresolved when that
        // special record is omitted by the enumeration.
        if entry.parent_reference & FILE_ID_MASK == NTFS_ROOT_FILE_ID {
            return Some(path_from_components(root, &components));
        }

        current = *by_reference.get(&entry.parent_reference)?;
    }
    None
}

fn relative_depth(path: &str, root: &str) -> Option<usize> {
    let root = root.trim_end_matches(['\\', '/']);
    if path.eq_ignore_ascii_case(root) {
        return Some(0);
    }
    let prefix = format!("{root}\\");
    if path.len() <= prefix.len() || !path.get(..prefix.len())?.eq_ignore_ascii_case(&prefix) {
        return None;
    }
    Some(path[prefix.len()..].split('\\').filter(|part| !part.is_empty()).count())
}

fn is_within_roots(path: &str, roots: &[String], max_depth: usize) -> bool {
    roots.iter().any(|root| {
        relative_depth(path, root)
            .is_some_and(|depth| depth > 0 && depth <= max_depth)
    })
}

pub(super) fn make_cache_text(
    entries: &[MftEntry],
    roots: &[String],
    max_depth: usize,
    volume: &str,
) -> String {
    let by_reference: HashMap<u64, usize> = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| (entry.file_reference, index))
        .collect();
    let mut text = format!("{CACHE_MAGIC}\t{max_depth}\t{}\n", roots.join("|"));
    let root = volume_root(volume);

    for (index, entry) in entries.iter().enumerate() {
        let Some(path) = resolve_path(index, entries, &by_reference, &root) else {
            continue;
        };
        if !is_within_roots(&path, roots, max_depth)
            || path.contains(['\t', '\n', '\r'])
        {
            continue;
        }
        text.push(if entry.is_dir { 'D' } else { 'F' });
        text.push('\t');
        text.push_str(&path);
        text.push('\n');
    }
    text
}

pub(super) fn write_atomically(path: &Path, contents: &str) -> Result<(), String> {
    if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Could not create output folder: {e}"))?;
    }
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, contents)
        .map_err(|e| format!("Could not write temporary snapshot: {e}"))?;
    crate::atomic::replace(&temporary, path)
        .map_err(|e| format!("Could not replace snapshot file: {e}"))
}

#[cfg(windows)]
mod native {
    use super::{parse_enum_buffer, MftEntry};
    use std::ffi::c_void;
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;

    const GENERIC_READ: u32 = 0x8000_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const FILE_SHARE_DELETE: u32 = 0x0000_0004;
    const OPEN_EXISTING: u32 = 3;
    const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;
    const FSCTL_ENUM_USN_DATA: u32 = 0x0009_00B3;
    const ERROR_HANDLE_EOF: u32 = 38;
    const INVALID_HANDLE_VALUE: *mut c_void = -1isize as *mut c_void;

    #[repr(C)]
    struct MftEnumDataV0 {
        start_file_reference_number: u64,
        low_usn: i64,
        high_usn: i64,
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
                "Could not open {volume} as a volume (Windows error {code}). Run the helper elevated and confirm the drive is NTFS."
            ));
        }
        Ok(VolumeHandle(handle))
    }

    pub fn enumerate(volume: &str) -> Result<Vec<MftEntry>, String> {
        let handle = open_volume(volume)?;
        let mut input = MftEnumDataV0 {
            start_file_reference_number: 0,
            low_usn: 0,
            high_usn: i64::MAX,
        };
        let mut entries = Vec::new();
        let mut buffer = vec![0u8; 1024 * 1024];
        let mut pages = 0usize;

        loop {
            let mut returned = 0u32;
            let succeeded = unsafe {
                DeviceIoControl(
                    handle.0,
                    FSCTL_ENUM_USN_DATA,
                    &mut input as *mut MftEnumDataV0 as *mut c_void,
                    size_of::<MftEnumDataV0>() as u32,
                    buffer.as_mut_ptr() as *mut c_void,
                    buffer.len() as u32,
                    &mut returned,
                    ptr::null_mut(),
                )
            };
            if succeeded == 0 {
                let code = unsafe { GetLastError() };
                if code == ERROR_HANDLE_EOF {
                    break;
                }
                return Err(format!("FSCTL_ENUM_USN_DATA failed (Windows error {code})"));
            }
            if returned == 0 {
                break;
            }
            let (next_reference, page) = parse_enum_buffer(&buffer[..returned as usize])?;
            entries.extend(page);
            pages += 1;
            if next_reference <= input.start_file_reference_number {
                break;
            }
            input.start_file_reference_number = next_reference;
            if pages % 64 == 0 {
                eprintln!("Read {pages} MFT pages ({} records)", entries.len());
            }
        }

        if entries.is_empty() {
            return Err(format!("No NTFS file records were returned for {volume}"));
        }
        Ok(entries)
    }
}

#[cfg(not(windows))]
mod native {
    use super::MftEntry;
    pub fn enumerate(_volume: &str) -> Result<Vec<MftEntry>, String> {
        Err("NTFS MFT enumeration is available only in the Windows build".into())
    }
}

pub(super) fn enumerate(volume: &str) -> Result<Vec<MftEntry>, String> {
    native::enumerate(volume)
}

pub(super) fn log_path_diagnostics(
    entries: &[MftEntry],
    roots: &[String],
    max_depth: usize,
    volume: &str,
) -> usize {
    let root_records = entries
        .iter()
        .filter(|entry| entry.file_reference & FILE_ID_MASK == NTFS_ROOT_FILE_ID)
        .count();
    let direct_root_children = entries
        .iter()
        .filter(|entry| entry.parent_reference & FILE_ID_MASK == NTFS_ROOT_FILE_ID)
        .count();
    let by_reference: HashMap<u64, usize> = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| (entry.file_reference, index))
        .collect();
    let by_file_id: HashMap<u64, ()> = entries
        .iter()
        .map(|entry| (entry.file_reference & FILE_ID_MASK, ()))
        .collect();
    let mut exact_parent_links = 0usize;
    let mut sequence_only_parent_links = 0usize;
    let mut missing_parent_links = 0usize;
    for entry in entries {
        if entry.parent_reference & FILE_ID_MASK == NTFS_ROOT_FILE_ID {
            continue;
        }
        if by_reference.contains_key(&entry.parent_reference) {
            exact_parent_links += 1;
        } else if by_file_id.contains_key(&(entry.parent_reference & FILE_ID_MASK)) {
            sequence_only_parent_links += 1;
        } else {
            missing_parent_links += 1;
        }
    }
    let text = make_cache_text(entries, roots, max_depth, volume);
    let count = text.lines().skip(1).count();
    eprintln!(
        "MFT path diagnostics: {} records; {root_records} volume-root record(s); {direct_root_children} direct child record(s); {exact_parent_links} exact parent links; {sequence_only_parent_links} parent links matching only by 48-bit file ID; {missing_parent_links} parent links with no matching record; exported {count} path(s) under configured roots",
        entries.len()
    );
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_v2_record(name: &str, file_ref: u64, parent_ref: u64, attributes: u32) -> Vec<u8> {
        let name_utf16: Vec<u16> = name.encode_utf16().collect();
        let record_length = 60 + name_utf16.len() * 2;
        let mut record = vec![0u8; record_length];
        record[0..4].copy_from_slice(&(record_length as u32).to_le_bytes());
        record[4..6].copy_from_slice(&2u16.to_le_bytes());
        record[8..16].copy_from_slice(&file_ref.to_le_bytes());
        record[16..24].copy_from_slice(&parent_ref.to_le_bytes());
        record[52..56].copy_from_slice(&attributes.to_le_bytes());
        record[56..58].copy_from_slice(&((name_utf16.len() * 2) as u16).to_le_bytes());
        record[58..60].copy_from_slice(&60u16.to_le_bytes());
        for (index, unit) in name_utf16.iter().enumerate() {
            let offset = 60 + index * 2;
            record[offset..offset + 2].copy_from_slice(&unit.to_le_bytes());
        }
        record
    }

    #[test]
    fn parses_v2_file_record_after_continuation_number() {
        let mut buffer = 123u64.to_le_bytes().to_vec();
        buffer.extend(make_v2_record("résumé.txt", 21, 20, 0));
        let (next, entries) = parse_enum_buffer(&buffer).unwrap();
        assert_eq!(next, 123);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].file_reference, 21);
        assert_eq!(entries[0].parent_reference, 20);
        assert_eq!(entries[0].name, "résumé.txt");
        assert!(!entries[0].is_dir);
    }

    #[test]
    fn parses_directory_attribute() {
        let mut buffer = 0u64.to_le_bytes().to_vec();
        buffer.extend(make_v2_record("Users", 20, 5, FILE_ATTRIBUTE_DIRECTORY));
        let (_, entries) = parse_enum_buffer(&buffer).unwrap();
        assert!(entries[0].is_dir);
    }

    #[test]
    fn rejects_truncated_or_out_of_range_records() {
        assert!(parse_enum_buffer(&[0; 7]).is_err());
        let mut buffer = 0u64.to_le_bytes().to_vec();
        buffer.extend(59u32.to_le_bytes());
        buffer.extend([0u8; 4]);
        assert!(parse_enum_buffer(&buffer).is_err());
    }

    #[test]
    fn resolves_paths_through_parent_file_references() {
        let entries = vec![
            MftEntry { file_reference: 5, parent_reference: 5, name: String::new(), is_dir: true },
            MftEntry { file_reference: 20, parent_reference: 5, name: "Users".into(), is_dir: true },
            MftEntry { file_reference: 21, parent_reference: 20, name: "alice".into(), is_dir: true },
            MftEntry { file_reference: 22, parent_reference: 21, name: "notes.txt".into(), is_dir: false },
        ];
        let by_reference: HashMap<u64, usize> = entries.iter().enumerate().map(|(i, e)| (e.file_reference, i)).collect();
        assert_eq!(resolve_path(3, &entries, &by_reference, "C:\\"), Some("C:\\Users\\alice\\notes.txt".into()));
    }

    #[test]
    fn resolves_paths_when_volume_root_record_is_not_enumerated() {
        // The volume-root MFT record (FRN 5) may be absent from an enumeration result.
        // Children whose parent FRN is 5 must still resolve from the volume root.
        let entries = vec![
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
        ];
        let by_reference: HashMap<u64, usize> = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.file_reference, index))
            .collect();

        assert_eq!(
            resolve_path(2, &entries, &by_reference, "C:\\"),
            Some("C:\\Users\\barda\\notes.txt".into())
        );
    }

    #[test]
    fn exports_existing_builtin_cache_format_for_configured_roots() {
        let entries = vec![
            MftEntry { file_reference: 5, parent_reference: 5, name: String::new(), is_dir: true },
            MftEntry { file_reference: 20, parent_reference: 5, name: "Users".into(), is_dir: true },
            MftEntry { file_reference: 21, parent_reference: 20, name: "alice".into(), is_dir: true },
            MftEntry { file_reference: 22, parent_reference: 21, name: "notes.txt".into(), is_dir: false },
            MftEntry { file_reference: 23, parent_reference: 20, name: "other.txt".into(), is_dir: false },
        ];
        let roots = vec!["C:\\Users\\alice".to_string()];
        let text = make_cache_text(&entries, &roots, 8, "C:");
        assert_eq!(text.lines().next().unwrap(), "okra-index-v1\t8\tC:\\Users\\alice");
        assert!(!text.lines().any(|line| line == "D\tC:\\Users\\alice"));
        assert!(text.lines().any(|line| line == "F\tC:\\Users\\alice\\notes.txt"));
        assert!(!text.contains("other.txt"));
    }

    #[test]
    fn filters_roots_and_depth_like_the_walker_index() {
        let roots = vec!["C:\\Users\\alice".to_string()];
        assert!(is_within_roots("C:\\Users\\alice\\notes.txt", &roots, 1));
        assert!(is_within_roots("C:\\Users\\alice\\docs\\a.pdf", &roots, 2));
        assert!(!is_within_roots("C:\\Users\\alice", &roots, 8));
        assert!(!is_within_roots("C:\\Users\\alice\\docs\\a.pdf", &roots, 1));
        assert!(!is_within_roots("C:\\Users\\alicia\\notes.txt", &roots, 8));
    }

    #[test]
    fn validates_volume_and_root_drive() {
        assert!(validate_volume_and_roots("C:", &["C:\\Users\\alice".into()]).is_ok());
        assert!(validate_volume_and_roots("C:", &["D:\\Users\\alice".into()]).is_err());
        assert!(validate_volume_and_roots("CC:", &["C:\\Users".into()]).is_err());
    }
}
