//! Atomic replacement of an existing file.
//!
//! On Windows, `MoveFileExW(MOVEFILE_REPLACE_EXISTING)` can return
//! `ERROR_ACCESS_DENIED` when another process has the destination open for reading,
//! even when that reader allowed delete sharing. Use `FileRenameInfoEx` with
//! `FILE_RENAME_FLAG_POSIX_SEMANTICS` as a fallback so readers can finish on the old
//! file while new readers see the replacement.

use std::io;
use std::path::Path;

#[cfg(not(windows))]
pub(crate) fn replace(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::rename(from, to)
}

#[cfg(windows)]
pub(crate) fn replace(from: &Path, to: &Path) -> io::Result<()> {
    use std::alloc::{alloc, dealloc, Layout};
    use std::ffi::c_void;
    use std::mem::{align_of, offset_of};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::fs::OpenOptionsExt;
    use std::ptr;

    const ERROR_ACCESS_DENIED: i32 = 5;
    const DELETE: u32 = 0x0001_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const FILE_SHARE_DELETE: u32 = 0x0000_0004;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_RENAME_INFO_EX: u32 = 22;
    const FILE_RENAME_FLAG_REPLACE_IF_EXISTS: u32 = 0x0000_0001;
    const FILE_RENAME_FLAG_POSIX_SEMANTICS: u32 = 0x0000_0002;

    #[repr(C)]
    struct FileRenameInfoEx {
        flags: u32,
        root_directory: *mut c_void,
        file_name_length: u32,
        file_name: [u16; 1],
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn SetFileInformationByHandle(
            file: *mut c_void,
            information_class: u32,
            information: *mut c_void,
            buffer_size: u32,
        ) -> i32;
        fn GetLastError() -> u32;
    }

    // The normal rename is cheapest and works when no reader has the destination open.
    match std::fs::rename(from, to) {
        Ok(()) => return Ok(()),
        Err(error) if error.raw_os_error() == Some(ERROR_ACCESS_DENIED) => {}
        Err(error) => return Err(error),
    }

    // FileRenameInfoEx requires a fully qualified target name when RootDirectory is null.
    let absolute_to = if to.is_absolute() {
        to.to_path_buf()
    } else {
        std::env::current_dir()?.join(to)
    };
    let wide_name: Vec<u16> = absolute_to.as_os_str().encode_wide().collect();
    let name_bytes = wide_name
        .len()
        .checked_mul(std::mem::size_of::<u16>())
        .and_then(|length| u32::try_from(length).ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "replacement path is too long"))?;
    let name_offset = offset_of!(FileRenameInfoEx, file_name);
    let struct_size = name_offset
        .checked_add(name_bytes as usize)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "replacement path is too long"))?;
    let buffer_size = u32::try_from(struct_size)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "replacement path is too long"))?;
    let layout = Layout::from_size_align(struct_size, align_of::<FileRenameInfoEx>())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid replacement path layout"))?;

    let mut options = std::fs::OpenOptions::new();
    options
        .access_mode(DELETE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS);
    let source = options.open(from)?;

    // SAFETY: The allocation uses the alignment and size of the variable-length
    // FILE_RENAME_INFO_EX structure. All fields and the UTF-16 filename are initialized
    // before calling SetFileInformationByHandle, and the buffer remains alive for the call.
    let buffer = unsafe { alloc(layout) };
    if buffer.is_null() {
        return Err(io::Error::new(io::ErrorKind::OutOfMemory, "could not allocate rename information"));
    }
    unsafe {
        let info = buffer.cast::<FileRenameInfoEx>();
        ptr::addr_of_mut!((*info).flags).write(
            FILE_RENAME_FLAG_REPLACE_IF_EXISTS | FILE_RENAME_FLAG_POSIX_SEMANTICS,
        );
        ptr::addr_of_mut!((*info).root_directory).write(ptr::null_mut());
        ptr::addr_of_mut!((*info).file_name_length).write(name_bytes);
        ptr::copy_nonoverlapping(
            wide_name.as_ptr(),
            ptr::addr_of_mut!((*info).file_name).cast::<u16>(),
            wide_name.len(),
        );
    }

    let succeeded = unsafe {
        SetFileInformationByHandle(
            source.as_raw_handle(),
            FILE_RENAME_INFO_EX,
            buffer.cast::<c_void>(),
            buffer_size,
        )
    };
    let error_code = if succeeded == 0 {
        Some(unsafe { GetLastError() })
    } else {
        None
    };
    unsafe { dealloc(buffer, layout) };

    match error_code {
        None => Ok(()),
        Some(code) => Err(io::Error::from_raw_os_error(code as i32)),
    }
}

#[cfg(test)]
mod tests {
    use super::replace;
    use std::io::Read;

    #[test]
    fn replaces_a_file_while_another_reader_still_has_it_open() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("okra-atomic-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let destination = dir.join("snapshot.tsv");
        let temporary = dir.join("snapshot.tmp");
        std::fs::write(&destination, b"old snapshot").unwrap();
        std::fs::write(&temporary, b"new snapshot").unwrap();

        let mut old_reader = std::fs::File::open(&destination).unwrap();
        replace(&temporary, &destination).unwrap();

        let mut old_contents = String::new();
        old_reader.read_to_string(&mut old_contents).unwrap();
        assert_eq!(old_contents, "old snapshot");
        assert_eq!(std::fs::read(&destination).unwrap(), b"new snapshot");

        drop(old_reader);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
