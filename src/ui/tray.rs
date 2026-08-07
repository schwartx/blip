//! System tray presence.
//!
//! The tray icon is the panel's only permanent surface. It matters most for
//! `low` notifications, which land in the list without pulling the panel open —
//! the badge is how you find out they're there.

use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
    Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, GetCursorPos, HICON, IDI_APPLICATION,
    IDI_INFORMATION, LoadIconW, MF_CHECKED, MF_GRAYED, MF_SEPARATOR, MF_STRING,
    SetForegroundWindow,
    TPM_BOTTOMALIGN,
    TPM_RETURNCMD, TPM_RIGHTALIGN, TrackPopupMenu,
};
use windows::core::PCWSTR;

use crate::wide;

pub const ICON_ID: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuAction {
    Show,
    ClearAll,
    ResetPosition,
    OpenConfig,
    ToggleAutostart,
    Quit,
    None,
}

const CMD_SHOW: u32 = 1;
const CMD_CLEAR: u32 = 2;
const CMD_RESET: u32 = 3;
const CMD_CONFIG: u32 = 4;
const CMD_AUTOSTART: u32 = 5;
const CMD_QUIT: u32 = 6;

pub struct Tray {
    hwnd: HWND,
    icon: HICON,
    added: bool,
}

impl Tray {
    pub fn new(hwnd: HWND, callback_msg: u32) -> Self {
        let icon = unsafe { LoadIconW(None, IDI_INFORMATION) }
            .or_else(|_| unsafe { LoadIconW(None, IDI_APPLICATION) })
            .unwrap_or_default();

        let mut t = Tray { hwnd, icon, added: false };
        t.apply(callback_msg, NIM_ADD, 0, false);
        t.added = true;
        t
    }

    /// Refresh the tooltip to reflect the current unread count.
    pub fn update(&self, callback_msg: u32, unread: usize, pinned: bool) {
        self.apply(callback_msg, NIM_MODIFY, unread, pinned);
    }

    fn apply(
        &self,
        callback_msg: u32,
        op: windows::Win32::UI::Shell::NOTIFY_ICON_MESSAGE,
        unread: usize,
        pinned: bool,
    ) {
        let tip = match (unread, pinned) {
            (0, false) => "blip · 无新通知".to_string(),
            (0, true) => "blip · 无新通知（已钉住）".to_string(),
            (n, false) => format!("blip · {n} 条通知"),
            (n, true) => format!("blip · {n} 条通知（已钉住）"),
        };

        let mut data = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: ICON_ID,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: callback_msg,
            hIcon: self.icon,
            ..Default::default()
        };
        let w = wide(&tip);
        let n = w.len().min(data.szTip.len());
        data.szTip[..n].copy_from_slice(&w[..n]);

        let _ = unsafe { Shell_NotifyIconW(op, &data) };
    }

    /// Right-click menu. `SetForegroundWindow` before `TrackPopupMenu` is the
    /// documented workaround for the menu refusing to dismiss when clicked away
    /// from — it has been required since Windows 95 and still is.
    pub fn show_menu(&self, pinned: bool, count: usize, autostart: bool) -> MenuAction {
        let Ok(menu) = (unsafe { CreatePopupMenu() }) else { return MenuAction::None };

        // Carrying the count in the label means the menu answers "is there
        // anything to look at?" before you click, and greying out when there
        // isn't turns "I clicked and nothing happened" into "I can see why".
        let show = wide(&format!("查看通知（{count}）"));
        let clear = wide(&format!("清空全部（{count}）"));
        // Unpinned, this is a statement of fact rather than an action, so it is
        // greyed. The parenthetical is doing real work: drag-to-pin is an
        // entirely invisible gesture, and a line that is already sitting here in
        // grey is the only natural place to reveal it.
        let reset = wide(if pinned {
            "取消钉住 · 回到光标模式"
        } else {
            "位置：跟随光标（拖动面板可固定）"
        });
        let config = wide("打开配置文件");
        let boot = wide("开机自动启动");
        let quit = wide("退出 blip");

        // The rule for this whole menu: if an item can't do anything in the
        // current state, it must not be clickable. Finding that out by clicking
        // is indistinguishable from a bug.
        let when_any = if count == 0 { MF_STRING | MF_GRAYED } else { MF_STRING };
        let when_pinned = if pinned { MF_STRING } else { MF_STRING | MF_GRAYED };
        // A checkmark, not a label that flips between "启用"/"禁用" — the latter
        // is always ambiguous about whether it names the state or the action.
        let boot_flags = if autostart { MF_STRING | MF_CHECKED } else { MF_STRING };

        unsafe {
            let _ = AppendMenuW(menu, when_any, CMD_SHOW as usize, PCWSTR(show.as_ptr()));
            let _ = AppendMenuW(menu, when_any, CMD_CLEAR as usize, PCWSTR(clear.as_ptr()));
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            let _ = AppendMenuW(menu, when_pinned, CMD_RESET as usize, PCWSTR(reset.as_ptr()));
            let _ = AppendMenuW(menu, MF_STRING, CMD_CONFIG as usize, PCWSTR(config.as_ptr()));
            let _ = AppendMenuW(menu, boot_flags, CMD_AUTOSTART as usize, PCWSTR(boot.as_ptr()));
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            let _ = AppendMenuW(menu, MF_STRING, CMD_QUIT as usize, PCWSTR(quit.as_ptr()));

            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let _ = SetForegroundWindow(self.hwnd);

            let cmd = TrackPopupMenu(
                menu,
                TPM_RIGHTALIGN | TPM_BOTTOMALIGN | TPM_RETURNCMD,
                pt.x,
                pt.y,
                None,
                self.hwnd,
                None,
            );
            let _ = DestroyMenu(menu);

            match cmd.0 as u32 {
                CMD_SHOW => MenuAction::Show,
                CMD_CLEAR => MenuAction::ClearAll,
                CMD_RESET => MenuAction::ResetPosition,
                CMD_CONFIG => MenuAction::OpenConfig,
                CMD_AUTOSTART => MenuAction::ToggleAutostart,
                CMD_QUIT => MenuAction::Quit,
                _ => MenuAction::None,
            }
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        if !self.added {
            return;
        }
        // Without this the ghost icon lingers until the user hovers the tray.
        let data = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: ICON_ID,
            ..Default::default()
        };
        let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &data) };
    }
}

/// Decode the tray callback's LPARAM into the mouse message it wraps.
pub fn tray_event(lparam: LPARAM) -> u32 {
    (lparam.0 as u32) & 0xFFFF
}

pub fn _unused(_: WPARAM) {}
