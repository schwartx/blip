//! Notification sound, plus the decision of whether to make one at all.
//!
//! `PlaySoundW` with `SND_ASYNC` is the right tool here: the panel makes at most
//! one short sound at a time, and the alternative (a warm XAudio2 graph) buys
//! sub-10ms latency that nobody can perceive on a notification chime while
//! costing a permanently open audio device in a tray-resident process.

use windows::Win32::Media::Audio::{
    PlaySoundW, SND_ALIAS, SND_ASYNC, SND_FILENAME, SND_NODEFAULT, SND_NOSTOP,
};
use windows::Win32::UI::Shell::{
    QUNS_BUSY, QUNS_PRESENTATION_MODE, QUNS_RUNNING_D3D_FULL_SCREEN, SHQueryUserNotificationState,
};
use windows::core::PCWSTR;

use crate::config::Config;
use crate::model::Level;
use crate::wide;

/// True when the machine is in a state where interrupting would be rude:
/// presenting, playing a full-screen game, or inside a Focus Assist quiet
/// window. The panel still records the notification — it just stays silent.
///
/// `QUNS_QUIET_TIME` is deliberately *not* included: that state covers the
/// first-hours-after-install grace period as much as real do-not-disturb, and
/// treating it as "be quiet" surprises people.
pub fn should_stay_quiet() -> bool {
    match unsafe { SHQueryUserNotificationState() } {
        Ok(s) => {
            s == QUNS_BUSY || s == QUNS_RUNNING_D3D_FULL_SCREEN || s == QUNS_PRESENTATION_MODE
        }
        Err(_) => false,
    }
}

pub struct Audio {
    low: Option<Vec<u16>>,
    normal: Option<Vec<u16>>,
    critical: Option<Vec<u16>>,
    enabled: bool,
    respect_quiet: bool,
    fallback: Vec<u16>,
}

impl Audio {
    pub fn new(cfg: &Config) -> Self {
        let path = |s: &String| {
            if s.trim().is_empty() || !std::path::Path::new(s).exists() {
                None
            } else {
                Some(wide(s))
            }
        };
        Audio {
            low: path(&cfg.sound.low),
            normal: path(&cfg.sound.normal),
            critical: path(&cfg.sound.critical),
            enabled: cfg.sound.enabled,
            respect_quiet: cfg.respect_quiet_hours,
            // Ships with every Windows install, so there is always something to
            // play without bundling an audio asset.
            fallback: wide("Notification.Default"),
        }
    }

    pub fn play(&self, level: Level) {
        if !self.enabled {
            return;
        }
        if self.respect_quiet && should_stay_quiet() {
            return;
        }

        let custom = match level {
            Level::Low => &self.low,
            Level::Normal => &self.normal,
            Level::Critical => &self.critical,
        };

        // SND_NOSTOP keeps a chime already in flight from being chopped off when
        // two notifications land back to back.
        unsafe {
            match custom {
                Some(p) => {
                    let _ = PlaySoundW(
                        PCWSTR(p.as_ptr()),
                        None,
                        SND_FILENAME | SND_ASYNC | SND_NODEFAULT | SND_NOSTOP,
                    );
                }
                None => {
                    let _ = PlaySoundW(
                        PCWSTR(self.fallback.as_ptr()),
                        None,
                        SND_ALIAS | SND_ASYNC | SND_NODEFAULT | SND_NOSTOP,
                    );
                }
            }
        }
    }
}
