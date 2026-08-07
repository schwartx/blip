//! Transports. Every one of them funnels into the same `Command` channel, so
//! the daemon has exactly one code path for "something arrived".

pub mod hook;
pub mod http;
pub mod pipe;

use crate::model::Command;
use std::sync::mpsc::Sender;

/// Handed to each transport thread. Delivering a command means pushing it onto
/// the queue and then poking the UI thread — the UI thread never blocks on IO
/// and the IO threads never touch UI state.
#[derive(Clone)]
pub struct Bridge {
    tx: Sender<Command>,
    /// The panel HWND as a raw isize so this stays `Send`. Zero until the
    /// window exists, which is fine — early commands just queue up.
    wake: std::sync::Arc<std::sync::atomic::AtomicIsize>,
}

impl Bridge {
    pub fn new(tx: Sender<Command>) -> (Self, std::sync::Arc<std::sync::atomic::AtomicIsize>) {
        let wake = std::sync::Arc::new(std::sync::atomic::AtomicIsize::new(0));
        (Bridge { tx, wake: wake.clone() }, wake)
    }

    pub fn send(&self, cmd: Command) {
        if self.tx.send(cmd).is_err() {
            return;
        }
        #[cfg(windows)]
        {
            use std::sync::atomic::Ordering;
            let h = self.wake.load(Ordering::Relaxed);
            if h != 0 {
                use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
                use windows::Win32::UI::WindowsAndMessaging::PostMessageW;
                let hwnd = HWND(h as *mut core::ffi::c_void);
                let _ = unsafe {
                    PostMessageW(Some(hwnd), crate::ui::WM_APP_IPC, WPARAM(0), LPARAM(0))
                };
            }
        }
    }
}
