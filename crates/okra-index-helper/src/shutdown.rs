//! Cooperative shutdown. The console control handler (Ctrl+C, Ctrl+Break, console close,
//! logoff, system shutdown) only sets a flag; the sync loop notices it within ~100 ms, stops
//! polling, and exits normally with status 0 so Task Scheduler does not treat it as a failure.
//!
//! Index files are written atomically (cache first, cursor second), so even a hard kill by
//! Task Scheduler (`schtasks /End`) or a power loss is safe: the next start replays journal
//! records from the last saved cursor.

use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

pub struct ShutdownFlag {
    requested: AtomicBool,
    stopped: AtomicBool,
}

impl ShutdownFlag {
    pub const fn new() -> Self {
        Self { requested: AtomicBool::new(false), stopped: AtomicBool::new(false) }
    }

    pub fn request(&self) {
        self.requested.store(true, Ordering::SeqCst);
    }

    pub fn requested(&self) -> bool {
        self.requested.load(Ordering::SeqCst)
    }

    /// The main thread has finished its final write and is about to exit.
    pub fn mark_stopped(&self) {
        self.stopped.store(true, Ordering::SeqCst);
    }

    /// Wait (bounded) for [`mark_stopped`]. Used by the OS handler thread, because Windows
    /// terminates the process as soon as a close/logoff/shutdown handler returns.
    pub fn wait_stopped(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while !self.stopped.load(Ordering::SeqCst) {
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(20));
        }
        true
    }

    /// Sleep up to `total`, waking early on a shutdown request. Returns `true` if shutdown
    /// was requested.
    pub fn sleep(&self, total: Duration) -> bool {
        let deadline = Instant::now() + total;
        loop {
            if self.requested() {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            thread::sleep((deadline - now).min(Duration::from_millis(100)));
        }
    }
}

static GLOBAL: ShutdownFlag = ShutdownFlag::new();

pub fn global() -> &'static ShutdownFlag {
    &GLOBAL
}

#[cfg(windows)]
pub fn install_handler() -> Result<(), String> {
    type Handler = unsafe extern "system" fn(u32) -> i32;
    #[link(name = "kernel32")]
    extern "system" {
        fn SetConsoleCtrlHandler(handler: Option<Handler>, add: i32) -> i32;
    }

    const CTRL_C_EVENT: u32 = 0;
    const CTRL_BREAK_EVENT: u32 = 1;
    const CTRL_CLOSE_EVENT: u32 = 2;
    const CTRL_LOGOFF_EVENT: u32 = 5;
    const CTRL_SHUTDOWN_EVENT: u32 = 6;

    unsafe extern "system" fn on_control(event: u32) -> i32 {
        match event {
            CTRL_C_EVENT | CTRL_BREAK_EVENT => {
                global().request();
                1
            }
            CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT => {
                global().request();
                // Windows kills the process when this returns; give the main thread a moment
                // to finish its current atomic write and exit cleanly.
                global().wait_stopped(Duration::from_secs(4));
                1
            }
            _ => 0,
        }
    }

    if unsafe { SetConsoleCtrlHandler(Some(on_control), 1) } == 0 {
        return Err("Could not install the console shutdown handler".into());
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn install_handler() -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sleep_runs_to_completion_without_a_request() {
        let flag = ShutdownFlag::new();
        let started = Instant::now();
        assert!(!flag.sleep(Duration::from_millis(150)));
        assert!(started.elapsed() >= Duration::from_millis(140));
    }

    #[test]
    fn sleep_returns_immediately_when_already_requested() {
        let flag = ShutdownFlag::new();
        flag.request();
        let started = Instant::now();
        assert!(flag.sleep(Duration::from_secs(30)));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn sleep_wakes_early_when_another_thread_requests_shutdown() {
        use std::sync::Arc;
        let flag = Arc::new(ShutdownFlag::new());
        let remote = Arc::clone(&flag);
        let waker = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            remote.request();
        });
        let started = Instant::now();
        assert!(flag.sleep(Duration::from_secs(30)));
        assert!(started.elapsed() < Duration::from_secs(5));
        waker.join().unwrap();
    }

    #[test]
    fn handler_wait_is_bounded_and_released_by_mark_stopped() {
        let flag = ShutdownFlag::new();
        assert!(!flag.wait_stopped(Duration::from_millis(60)));
        flag.mark_stopped();
        assert!(flag.wait_stopped(Duration::from_secs(1)));
    }
}
