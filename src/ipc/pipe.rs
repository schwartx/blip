//! Named-pipe transport — the fast path for anything running on this machine.
//!
//! No network stack, no handshake, ~8ms round trip from a cold CLI process.
//! The pipe also doubles as the daemon's single-instance lock: whoever wins
//! `FILE_FLAG_FIRST_PIPE_INSTANCE` is the daemon.
//!
//! Security note: we pass a NULL security descriptor deliberately. The default
//! DACL for a named pipe grants *read* to Everyone but write only to the creator,
//! LocalSystem and administrators. Since this is an INBOUND pipe (clients only
//! ever write), that is exactly the restriction we want, and it avoids hand-
//! rolling an ACL.

use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Foundation::{
    CloseHandle, ERROR_PIPE_CONNECTED, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_MODE,
    OPEN_EXISTING, PIPE_ACCESS_INBOUND, ReadFile, WriteFile,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_MESSAGE,
    PIPE_TYPE_MESSAGE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, WaitNamedPipeW,
};
use windows::core::PCWSTR;

use crate::ipc::Bridge;
use crate::model::Command;
use crate::wide;

const BUF: u32 = 64 * 1024;

/// Per-user so two accounts on one machine don't collide. The DACL is what
/// actually enforces isolation; the name just avoids an accidental clash.
pub fn pipe_name() -> String {
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "default".into());
    let safe: String = user
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!(r"\\.\pipe\blip-{safe}-v1")
}

/// `ConnectNamedPipe` reports ERROR_PIPE_CONNECTED when the client got there
/// first. That's a success, not a failure.
fn already_connected(e: &windows::core::Error) -> bool {
    e.code().0 as u32 == (0x8007_0000 | ERROR_PIPE_CONNECTED.0)
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

pub struct PipeServer {
    handle: HANDLE,
    stop: AtomicBool,
}

// The handle is only ever touched from the single serving thread.
unsafe impl Send for PipeServer {}
unsafe impl Sync for PipeServer {}

impl PipeServer {
    /// Claim the pipe. `Err` means another daemon already owns it — the caller
    /// should exit rather than fight over it.
    pub fn bind() -> Result<Self, String> {
        let name = wide(&pipe_name());
        let handle = unsafe {
            CreateNamedPipeW(
                PCWSTR(name.as_ptr()),
                PIPE_ACCESS_INBOUND | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                0,
                BUF,
                0,
                None,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err("another blipd instance already owns the pipe".into());
        }
        Ok(PipeServer { handle, stop: AtomicBool::new(false) })
    }

    /// Blocking accept loop. Runs on its own thread for the daemon's lifetime.
    ///
    /// One instance, served serially: notification volume never justifies a
    /// pool, and serial delivery means arrival order is preserved.
    pub fn serve(&self, bridge: Bridge) {
        let mut buf = vec![0u8; BUF as usize];
        while !self.stop.load(Ordering::Relaxed) {
            if let Err(e) = unsafe { ConnectNamedPipe(self.handle, None) }
                && !already_connected(&e)
            {
                let _ = unsafe { DisconnectNamedPipe(self.handle) };
                continue;
            }

            let mut read = 0u32;
            let ok = unsafe { ReadFile(self.handle, Some(&mut buf), Some(&mut read), None) };
            if ok.is_ok() && read > 0 {
                dispatch(&buf[..read as usize], &bridge);
            }
            let _ = unsafe { DisconnectNamedPipe(self.handle) };
        }
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for PipeServer {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.handle) };
    }
}

/// A payload may hold several newline-separated JSON commands; tolerate both
/// that and a single object.
fn dispatch(bytes: &[u8], bridge: &Bridge) {
    let Ok(text) = std::str::from_utf8(bytes) else { return };
    for line in text.split('\n').map(str::trim).filter(|l| !l.is_empty()) {
        match serde_json::from_str::<Command>(line) {
            Ok(cmd) => bridge.send(cmd),
            Err(_) => {
                // Not a command envelope — treat the raw text as a title so a
                // bare string written to the pipe still does something sensible.
                let req = crate::model::NotifyRequest {
                    title: line.to_string(),
                    ..Default::default()
                };
                bridge.send(Command::Notify(req));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Write one command to the daemon. `Err` means the pipe isn't there — the
/// caller decides whether to spawn the daemon and retry.
pub fn send(cmd: &Command) -> Result<(), String> {
    let payload = serde_json::to_vec(cmd).map_err(|e| e.to_string())?;
    let name = wide(&pipe_name());

    let handle = unsafe {
        CreateFileW(
            PCWSTR(name.as_ptr()),
            GENERIC_WRITE.0,
            FILE_SHARE_MODE(0),
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    }
    .map_err(|e| format!("pipe not available: {e}"))?;

    let mut written = 0u32;
    let res = unsafe { WriteFile(handle, Some(&payload), Some(&mut written), None) };
    let _ = unsafe { CloseHandle(handle) };
    res.map_err(|e| format!("pipe write failed: {e}"))
}

/// Block until the pipe exists, up to `timeout_ms`. Used right after spawning
/// the daemon so the very first notification isn't dropped on the floor.
///
/// `WaitNamedPipeW` alone is not enough here, and the reason is a genuine trap:
/// it waits for an *instance to become available*, but if no instance exists at
/// all it returns immediately with failure rather than honouring the timeout.
/// Right after spawning the daemon that is exactly the state we're in — the
/// process exists but hasn't reached `CreateNamedPipeW` yet — so a single call
/// always fails on first run. Poll instead.
pub fn wait_ready(timeout_ms: u32) -> bool {
    let name = wide(&pipe_name());
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms as u64);

    loop {
        if unsafe { WaitNamedPipeW(PCWSTR(name.as_ptr()), 50) }.as_bool() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(15));
    }
}
