//! Integration with voidtools "Everything" (https://www.voidtools.com).
//!
//! Two transports, both talking to the already-running Everything service, which holds an
//! NTFS-MFT-based index and answers in milliseconds:
//!   1. `EverythingSdk` – loads `Everything64.dll` / `Everything32.dll` (SDK2, IPC) at runtime
//!      with `libloading`, so the crate still builds and runs on machines without it.
//!   2. `EsCli` – shells out to the `es.exe` command-line client.
//!
//! The query string is passed through unchanged, so native Everything syntax works:
//! `ext:pdf report`, `size:>10mb`, `dm:today`, `path:src *.rs`, `regex:^inv\d+`.

use libloading::{Library, Symbol};
use std::iter::once;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

type SetSearchW = unsafe extern "system" fn(*const u16);
type SetU32 = unsafe extern "system" fn(u32);
type QueryW = unsafe extern "system" fn(i32) -> i32;
type GetU32 = unsafe extern "system" fn() -> u32;
type GetFullPathW = unsafe extern "system" fn(u32, *mut u16, u32) -> u32;
type IsFolder = unsafe extern "system" fn(u32) -> i32;

const REQUEST_FULL_PATH_AND_FILE_NAME: u32 = 0x0000_0004;

pub struct EverythingSdk {
    // The SDK2 keeps its query state in globals, so every query holds this lock end to end.
    lib: Mutex<Library>,
}

impl EverythingSdk {
    /// `explicit` (from config) is always tried; automatic discovery only runs on Windows.
    pub fn load(explicit: Option<&Path>) -> Option<Self> {
        let name = if cfg!(target_pointer_width = "64") { "Everything64.dll" } else { "Everything32.dll" };
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(p) = explicit {
            candidates.push(p.to_path_buf());
        }
        if cfg!(windows) {
            if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(|p| p.to_path_buf())) {
                candidates.push(dir.join(name));
            }
            candidates.push(PathBuf::from(name)); // normal DLL search order (cwd, PATH, ...)
            for var in ["ProgramFiles", "ProgramFiles(x86)"] {
                if let Some(base) = std::env::var_os(var) {
                    let root = PathBuf::from(base).join("Everything");
                    candidates.push(root.join(name));
                    candidates.push(root.join("SDK").join("dll").join(name));
                }
            }
        }
        candidates
            .into_iter()
            .find_map(|c| unsafe { Library::new(&c) }.ok())
            .map(|lib| Self { lib: Mutex::new(lib) })
    }

    /// Returns `None` when the query could not run (typically: Everything is not running).
    pub fn search(&self, q: &str, max: u32) -> Option<Vec<(PathBuf, bool)>> {
        let lib = self.lib.lock().ok()?;
        unsafe {
            let set_search: Symbol<SetSearchW> = lib.get(b"Everything_SetSearchW\0").ok()?;
            let set_flags: Symbol<SetU32> = lib.get(b"Everything_SetRequestFlags\0").ok()?;
            let set_max: Symbol<SetU32> = lib.get(b"Everything_SetMax\0").ok()?;
            let query: Symbol<QueryW> = lib.get(b"Everything_QueryW\0").ok()?;
            let count: Symbol<GetU32> = lib.get(b"Everything_GetNumResults\0").ok()?;
            let full_path: Symbol<GetFullPathW> = lib.get(b"Everything_GetResultFullPathNameW\0").ok()?;
            let is_folder: Symbol<IsFolder> = lib.get(b"Everything_IsFolderResult\0").ok()?;

            let wide: Vec<u16> = q.encode_utf16().chain(once(0)).collect();
            set_search(wide.as_ptr());
            set_flags(REQUEST_FULL_PATH_AND_FILE_NAME);
            set_max(max);
            if query(1) == 0 {
                return None;
            }
            let n = count();
            let mut out = Vec::with_capacity(n as usize);
            let mut buf = vec![0u16; 32768];
            for i in 0..n {
                let len = full_path(i, buf.as_mut_ptr(), buf.len() as u32) as usize;
                if len == 0 {
                    continue;
                }
                let s = String::from_utf16_lossy(&buf[..len.min(buf.len())]);
                out.push((PathBuf::from(s), is_folder(i) != 0));
            }
            Some(out)
        }
    }
}

pub struct EsCli {
    exe: PathBuf,
}

fn suppress_console(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    #[cfg(not(windows))]
    {
        let _ = command;
    }
}

impl EsCli {
    pub fn find(explicit: Option<&Path>) -> Option<Self> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(p) = explicit {
            candidates.push(p.to_path_buf());
        }
        if cfg!(windows) {
            candidates.push(PathBuf::from("es.exe")); // on PATH
            for var in ["ProgramFiles", "ProgramFiles(x86)"] {
                if let Some(base) = std::env::var_os(var) {
                    candidates.push(PathBuf::from(base).join("Everything").join("es.exe"));
                }
            }
        }
        candidates
            .into_iter()
            .find(|c| {
                let mut command = Command::new(c);
                suppress_console(&mut command);
                command.arg("-version").output().is_ok()
            })
            .map(|exe| Self { exe })
    }

    pub fn search(&self, q: &str, max: u32) -> Option<Vec<(PathBuf, bool)>> {
        let mut command = Command::new(&self.exe);
        suppress_console(&mut command);
        let out = command.arg("-n").arg(max.to_string()).arg(q).output().ok()?;
        if !out.status.success() {
            return None; // e.g. Everything is not running (IPC error)
        }
        Some(
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(|l| {
                    let p = PathBuf::from(l);
                    let dir = p.is_dir();
                    (p, dir)
                })
                .collect(),
        )
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn es_cli_parses_output() {
        let dir = std::env::temp_dir().join(format!("fs-es-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("fake-es");
        std::fs::write(&script, "#!/bin/sh\n[ \"$1\" = \"-version\" ] && exit 0\necho /tmp/a.txt\necho\necho /tmp\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let es = EsCli::find(Some(&script)).expect("fake es found");
        let r = es.search("a b", 10).unwrap();
        assert_eq!(r.len(), 2);
        assert!(!r[0].1 && r[1].1);
    }

    #[test]
    fn missing_dll_is_none() {
        assert!(EverythingSdk::load(Some(Path::new("/nonexistent/Everything64.dll"))).is_none());
    }
}
