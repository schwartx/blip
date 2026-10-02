//! The resident daemon.
//!
//! Windows subsystem, so there is never a console flash no matter how it gets
//! launched. Owns the panel, the notification list, the tray icon, and both
//! inbound transports.
//!
//! Single-instance is enforced by the named pipe itself rather than a separate
//! mutex: whoever wins `FILE_FLAG_FIRST_PIPE_INSTANCE` is the daemon, and the
//! loser exits quietly. That matters because the CLI spawns the daemon
//! optimistically, so two racing `blip` invocations will both try.

#![cfg_attr(windows, windows_subsystem = "windows")]

use std::sync::Arc;
use std::sync::mpsc::channel;

use blip::config::Config;
use blip::ipc::pipe::PipeServer;
use blip::ipc::{Bridge, http};
use blip::model::{Command, Level, NotifyRequest};

fn main() {
    let (cfg, config_warning) = Config::load();

    // Claim the pipe before anything else. If another daemon holds it we are
    // the loser of a spawn race and should leave without a word.
    let pipe = match PipeServer::bind() {
        Ok(p) => Arc::new(p),
        Err(_) => return,
    };

    // We are the daemon, so whatever path the Run key holds should be ours.
    // Corrects the entry after a move or reinstall, which would otherwise leave
    // a checked menu item pointing at an exe that no longer exists.
    #[cfg(windows)]
    blip::ui::autostart::heal();

    let (tx, rx) = channel::<Command>();
    let (bridge, wake) = Bridge::new(tx);

    let local_thread = {
        let pipe = pipe.clone();
        let bridge = bridge.clone();
        std::thread::spawn(move || pipe.serve(bridge))
    };

    {
        let bind = cfg.bind.clone();
        let bridge = bridge.clone();
        std::thread::spawn(move || {
            if let Err(e) = http::serve(&bind, bridge.clone()) {
                // Report the failure through the panel itself. A tray app that
                // silently isn't listening is the worst kind of broken.
                bridge.send(Command::Notify(NotifyRequest {
                    title: "blip: HTTP 未能启动".into(),
                    body: Some(e),
                    level: Some(Level::Critical),
                    id: Some("blip-http-error".into()),
                    source: Some("blip".into()),
                    ..Default::default()
                }));
            }
        });
    }

    // Surface a bad config the same way — through the product, not a log file
    // nobody reads.
    if let Some(w) = config_warning {
        bridge.send(Command::Notify(NotifyRequest {
            title: "blip: 配置有误，已回退到默认值".into(),
            body: Some(w),
            level: Some(Level::Critical),
            id: Some("blip-config-error".into()),
            source: Some("blip".into()),
            ..Default::default()
        }));
    }

    #[cfg(windows)]
    if let Err(e) = blip::ui::window::run(cfg, rx, wake, pipe) {
        fatal(&format!("blip: 面板初始化失败\n\n{e}"));
    }
    #[cfg(windows)]
    let _ = local_thread;

    #[cfg(unix)]
    {
        let _ = wake;
        #[cfg(target_os = "macos")]
        if let Err(e) = blip::macos::run(cfg, rx, pipe.clone()) {
            fatal(&format!("blip: panel initialization failed: {e}"));
        }
        #[cfg(not(target_os = "macos"))]
        run_headless(cfg, rx);
        pipe.stop();
        let _ = local_thread.join();
        drop(pipe);
    }
}

/// The one place a message box is justified: the panel can't report on itself
/// when the panel is what failed.
#[cfg(windows)]
fn fatal(msg: &str) {
    use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    use windows::core::PCWSTR;
    let text = blip::wide(msg);
    let title = blip::wide("blip");
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(text.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}

#[cfg(target_os = "macos")]
fn fatal(msg: &str) {
    eprintln!("{msg}");
}

/// Unix development hosts can exercise the shared CLI and HTTP transports.
/// The desktop panel is provided by the Windows and macOS backends.
#[cfg(all(unix, not(target_os = "macos")))]
fn run_headless(cfg: Config, rx: std::sync::mpsc::Receiver<Command>) {
    let mut store = blip::store::Store::new();
    for cmd in rx {
        match cmd {
            Command::Notify(req) => {
                store.push(req, &cfg);
            }
            Command::Dismiss { id } => store.dismiss_id(&id),
            Command::Clear => store.clear(),
            Command::Quit => break,
            Command::Show | Command::Ping => {}
        }
    }
}
