use super::{Action, Item, Kind, Provider};
use crate::matcher::{score, Query};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long one window list is reused. Typing a query asks for the list on every keystroke, and
/// the set of open windows does not change between two keystrokes.
const WINDOW_LIST_TTL: Duration = Duration::from_millis(750);

#[derive(Default)]
pub struct WindowsProvider {
    cache: Mutex<Option<(Instant, Vec<WindowEntry>)>>,
}

#[derive(Clone)]
struct WindowEntry {
    hwnd: isize,
    title: String,
    pid: u32,
}

#[cfg(windows)]
mod native {
    use super::WindowEntry;
    use std::ffi::c_void;

    type Hwnd = *mut c_void;

    #[link(name = "user32")]
    extern "system" {
        fn EnumWindows(
            callback: Option<unsafe extern "system" fn(Hwnd, isize) -> i32>,
            data: isize,
        ) -> i32;
        fn IsWindowVisible(hwnd: Hwnd) -> i32;
        fn GetWindowTextLengthW(hwnd: Hwnd) -> i32;
        fn GetWindowTextW(hwnd: Hwnd, text: *mut u16, max_count: i32) -> i32;
        fn GetWindowThreadProcessId(hwnd: Hwnd, process_id: *mut u32) -> u32;
        fn IsWindow(hwnd: Hwnd) -> i32;
        fn IsIconic(hwnd: Hwnd) -> i32;
        fn ShowWindow(hwnd: Hwnd, command: i32) -> i32;
        fn SetForegroundWindow(hwnd: Hwnd) -> i32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentProcessId() -> u32;
    }

    unsafe extern "system" fn collect(hwnd: Hwnd, data: isize) -> i32 {
        let windows = &mut *(data as *mut Vec<WindowEntry>);
        if IsWindowVisible(hwnd) == 0 {
            return 1;
        }
        let current_pid = GetCurrentProcessId();
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid == current_pid {
            return 1;
        }
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return 1;
        }
        let mut buffer = vec![0u16; (len as usize) + 1];
        let copied = GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32);
        if copied <= 0 {
            return 1;
        }
        let title = String::from_utf16_lossy(&buffer[..copied as usize])
            .trim()
            .to_string();
        if !title.is_empty() {
            windows.push(WindowEntry {
                hwnd: hwnd as isize,
                title,
                pid,
            });
        }
        1
    }

    pub fn enumerate() -> Vec<WindowEntry> {
        let mut windows = Vec::new();
        unsafe {
            EnumWindows(
                Some(collect),
                &mut windows as *mut Vec<WindowEntry> as isize,
            );
        }
        windows
    }

    pub fn focus(hwnd: isize) -> Result<(), String> {
        let handle = hwnd as Hwnd;
        unsafe {
            if IsWindow(handle) == 0 {
                return Err("That window is no longer available".into());
            }
            if IsIconic(handle) != 0 {
                ShowWindow(handle, 9); // SW_RESTORE
            }
            if SetForegroundWindow(handle) == 0 {
                return Err("Windows did not allow this window to be focused".into());
            }
        }
        Ok(())
    }
}

#[cfg(not(windows))]
mod native {
    use super::WindowEntry;
    pub fn enumerate() -> Vec<WindowEntry> {
        Vec::new()
    }
    pub fn focus(_hwnd: isize) -> Result<(), String> {
        Err("Window switching is available in the Windows build".into())
    }
}

impl WindowsProvider {
    pub fn focus(hwnd: isize) -> Result<(), String> {
        native::focus(hwnd)
    }

    /// The open windows, listed afresh at most once per [`WINDOW_LIST_TTL`]. A window that closed
    /// in the meantime is caught by `focus`, which checks the handle before using it.
    fn entries(&self) -> Vec<WindowEntry> {
        let mut cache = self.cache.lock().unwrap();
        if let Some((listed_at, list)) = cache.as_ref() {
            if listed_at.elapsed() < WINDOW_LIST_TTL {
                return list.clone();
            }
        }
        let list = native::enumerate();
        *cache = Some((Instant::now(), list.clone()));
        list
    }
}

impl Provider for WindowsProvider {
    fn name(&self) -> &'static str {
        "Open Windows"
    }
    fn prefix(&self) -> Option<&'static str> {
        Some("win")
    }
    fn query(&self, q: &str, limit: usize) -> Vec<Item> {
        let query = Query::new(q);
        let mut items: Vec<Item> = self
            .entries()
            .into_iter()
            .filter_map(|window| {
                let rank = if query.is_empty() {
                    0
                } else {
                    score(&query, &window.title)? + 25
                };
                Some(Item {
                    id: format!("window:{}", window.hwnd),
                    title: window.title,
                    subtitle: format!("Open window · PID {}", window.pid),
                    kind: Kind::Window,
                    action: Action::FocusWindow(window.hwnd),
                    score: rank,
                })
            })
            .collect();
        items.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.title.cmp(&b.title)));
        items.truncate(limit);
        items
    }
}
