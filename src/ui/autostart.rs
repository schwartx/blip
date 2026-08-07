//! Start-with-Windows, via the per-user `Run` key.
//!
//! `HKCU\...\CurrentVersion\Run` rather than a scheduled task or a service:
//! it needs no elevation, it is the one place users already know to look when
//! they want to audit what starts with their machine, and — decisively — the
//! panel must run *in the user's session* to be able to draw at all. A service
//! sits in session 0 and physically cannot put pixels on your desktop.

use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_SZ, RegCloseKey, RegDeleteValueW,
    RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
};
use windows::core::PCWSTR;

use crate::wide;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE: &str = "blip";

/// RAII around an open key, so no early return can leak a handle.
struct Key(HKEY);

impl Key {
    fn open(write: bool) -> Option<Key> {
        let sub = wide(RUN_KEY);
        let access = if write { KEY_READ | KEY_WRITE } else { KEY_READ };
        let mut h = HKEY::default();
        let rc = unsafe {
            RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(sub.as_ptr()), None, access, &mut h)
        };
        (rc == ERROR_SUCCESS).then_some(Key(h))
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        let _ = unsafe { RegCloseKey(self.0) };
    }
}

/// The command line we would register: our own path, quoted so a install
/// directory containing a space doesn't turn into two arguments.
fn command() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    Some(format!("\"{}\"", exe.to_string_lossy()))
}

/// The currently registered command, if any.
fn current() -> Option<String> {
    let key = Key::open(false)?;
    let name = wide(VALUE);

    // Two-call pattern: ask for the size, then read. The value is a path, so
    // its length is not something we get to assume.
    let mut len: u32 = 0;
    let rc = unsafe {
        RegQueryValueExW(key.0, PCWSTR(name.as_ptr()), None, None, None, Some(&mut len))
    };
    if rc != ERROR_SUCCESS || len == 0 {
        return None;
    }

    let mut buf = vec![0u8; len as usize];
    let rc = unsafe {
        RegQueryValueExW(
            key.0,
            PCWSTR(name.as_ptr()),
            None,
            None,
            Some(buf.as_mut_ptr()),
            Some(&mut len),
        )
    };
    if rc != ERROR_SUCCESS {
        return None;
    }

    let units: Vec<u16> =
        buf.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    let text = String::from_utf16_lossy(&units);
    // REG_SZ is not required to be terminated, and when it is, the terminator
    // is part of the returned length.
    Some(text.trim_end_matches('\0').to_string())
}

pub fn is_enabled() -> bool {
    current().is_some()
}

pub fn set(on: bool) -> bool {
    let Some(key) = Key::open(true) else { return false };
    let name = wide(VALUE);

    if !on {
        let rc = unsafe { RegDeleteValueW(key.0, PCWSTR(name.as_ptr())) };
        return rc == ERROR_SUCCESS;
    }

    let Some(cmd) = command() else { return false };
    let data = wide(&cmd);
    let bytes: &[u8] = unsafe {
        std::slice::from_raw_parts(data.as_ptr() as *const u8, std::mem::size_of_val(&data[..]))
    };
    let rc =
        unsafe { RegSetValueExW(key.0, PCWSTR(name.as_ptr()), None, REG_SZ, Some(bytes)) };
    rc == ERROR_SUCCESS
}

/// Rewrite a stale entry to point at wherever we are now.
///
/// Without this, moving or reinstalling `blipd.exe` leaves a Run entry aimed at
/// a path that no longer exists: the menu keeps its checkmark, the user keeps
/// believing autostart is on, and it silently isn't. Called once at startup —
/// if we are running, the path in the registry should be *this* path.
pub fn heal() {
    let Some(existing) = current() else { return };
    let Some(want) = command() else { return };
    let norm = |s: &str| s.trim().trim_matches('"').to_ascii_lowercase();
    if norm(&existing) != norm(&want) {
        set(true);
    }
}
