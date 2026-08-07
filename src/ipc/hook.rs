//! Claude Code hook payloads, translated into notifications.
//!
//! Claude Code's `http` hook handler POSTs the raw event JSON to a URL and does
//! nothing else — no body templating, no shell. Teaching blip to read that
//! shape is what lets the companion plugin be a twenty-line `hooks.json` with
//! no script in it: nothing to install, nothing on `PATH`, no encoding to get
//! wrong, and no process to spawn while you are already waiting.
//!
//! ```text
//! POST /hook/claude?level=critical
//! {"session_id":"abc","cwd":"E:\\Projects\\blip","message":"需要你的许可",
//!  "hook_event_name":"Notification"}
//! ```
//!
//! The level rides in the query string because the payload does not carry the
//! matcher that selected it: a permission prompt and an idle prompt are both
//! `hook_event_name: "Notification"`, so the plugin points its matchers at
//! different URLs. `if_idle` rides there for the same reason — it is a property
//! of the *event*, not of the payload. `Stop` fires on every turn, including the
//! ones you sat and watched, so it asks to pop only if you had walked away.

use serde::Deserialize;

use crate::model::{Level, NotifyRequest};

/// The fields of a hook payload that mean something to a notification panel.
///
/// Every one is optional on purpose. The set of events grows, and an unknown
/// payload should still produce *something* on screen rather than a 400 the
/// user never sees.
#[derive(Deserialize)]
struct ClaudeHook {
    session_id: Option<String>,
    cwd: Option<String>,
    message: Option<String>,
    hook_event_name: Option<String>,
    /// `Stop` only. The one event that says what actually happened, rather than
    /// that something happened.
    last_assistant_message: Option<String>,
}

pub fn from_claude(json: &str, level: Level, if_idle: Option<f32>) -> Result<NotifyRequest, String> {
    let hook: ClaudeHook = serde_json::from_str(json).map_err(|e| format!("bad json: {e}"))?;

    Ok(NotifyRequest {
        if_idle,
        // The project directory, not "Claude Code": with several sessions open
        // it is the only thing that answers "which one wants me?".
        title: hook.cwd.as_deref().and_then(leaf).unwrap_or("Claude Code").to_string(),
        // `message` first because when it exists it is the thing being asked;
        // `last_assistant_message` is what `Stop` has instead, and it is what
        // makes a finished turn readable without switching to the terminal.
        body: pick(hook.message)
            .or_else(|| pick(hook.last_assistant_message).map(|m| first_line(&m, 160)))
            .or_else(|| hook.hook_event_name.as_deref().map(describe).map(str::to_string)),
        level: Some(level),
        // One row per session, updated in place. Without this a session that
        // asks three times leaves three rows, which is the stacking behaviour
        // this panel exists to get away from.
        id: hook.session_id.map(|s| format!("cc-{s}")),
        source: Some("claude".into()),
        ..Default::default()
    })
}

/// Last resort when an event carries no text of its own.
///
/// Only events blip actually routes are spelled out; anything else falls
/// through as its raw name, which is ugly but never wrong. `StopFailure` is the
/// one that matters: it says *why* nothing is on screen, and "StopFailure" is
/// not something anyone should have to decode mid-alert.
///
/// Deliberately not a guess at the payload's error field. The matcher
/// (`rate_limit`, `overloaded`, …) is what carries the reason, and the matcher
/// is not in the payload — same asymmetry that puts `level` in the query
/// string. Whether some other field has it is unverified.
fn describe(event: &str) -> &str {
    match event {
        "StopFailure" => "Turn ended on an API error",
        "Stop" => "Turn ended",
        other => other,
    }
}

/// A trimmed field, or `None` if it was absent or blank.
fn pick(s: Option<String>) -> Option<String> {
    s.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// First line, truncated.
///
/// `last_assistant_message` is a whole reply — code blocks, lists, the lot. A
/// row is two lines of body; the rest belongs in the terminal you're about to
/// go back to anyway.
fn first_line(s: &str, max: usize) -> String {
    let line = s.lines().next().unwrap_or("").trim();
    match line.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &line[..i]),
        None => line.to_string(),
    }
}

/// Last path segment, for either separator.
///
/// Not `std::path::Path::file_name`: the payload's separators are whatever the
/// sending machine uses, which need not be this machine's.
fn leaf(path: &str) -> Option<&str> {
    let seg = path.trim_end_matches(['/', '\\']).rsplit(['/', '\\']).next()?.trim();
    (!seg.is_empty()).then_some(seg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_a_permission_prompt() {
        let n = from_claude(
            r#"{"session_id":"abc","cwd":"E:\\Projects\\blip",
                "message":"需要你的许可","hook_event_name":"Notification"}"#,
            Level::Critical,
            None,
        )
        .unwrap();

        assert_eq!(n.title, "blip");
        assert_eq!(n.if_idle, None);
        assert_eq!(n.body.as_deref(), Some("需要你的许可"));
        assert_eq!(n.id.as_deref(), Some("cc-abc"));
        assert_eq!(n.level, Some(Level::Critical));
        assert_eq!(n.source.as_deref(), Some("claude"));
    }

    #[test]
    fn a_finished_turn_shows_what_was_said() {
        let n = from_claude(
            r#"{"session_id":"s1","cwd":"/home/me/api","hook_event_name":"Stop",
                "last_assistant_message":"  改完了，25 个测试全过。\n\n细节见 diff。  "}"#,
            Level::Normal,
            Some(15.0),
        )
        .unwrap();

        assert_eq!(n.title, "api");
        assert_eq!(n.body.as_deref(), Some("改完了，25 个测试全过。"));
        assert_eq!(n.if_idle, Some(15.0));
    }

    #[test]
    fn a_long_reply_is_cut_on_a_character_boundary() {
        let long = "字".repeat(200);
        let n = from_claude(
            &format!(r#"{{"hook_event_name":"Stop","last_assistant_message":"{long}"}}"#),
            Level::Normal,
            None,
        )
        .unwrap();

        let body = n.body.unwrap();
        assert!(body.ends_with('…'));
        assert_eq!(body.chars().count(), 161);
    }

    #[test]
    fn falls_back_to_the_event_name_when_there_is_nothing_to_say() {
        // SessionEnd carries neither field, but a row with a title and no body
        // reads as a bug rather than as "the turn ended".
        let n =
            from_claude(r#"{"cwd":"/home/me/api","hook_event_name":"Stop"}"#, Level::Low, None).unwrap();
        assert_eq!(n.title, "api");
        assert_eq!(n.body.as_deref(), Some("Turn ended"));
        assert_eq!(n.id, None);
    }

    #[test]
    fn an_api_error_says_so_rather_than_naming_the_event() {
        let n = from_claude(
            r#"{"session_id":"s2","cwd":"E:\\Projects\\blip","hook_event_name":"StopFailure"}"#,
            Level::Critical,
            None,
        )
        .unwrap();

        assert_eq!(n.body.as_deref(), Some("Turn ended on an API error"));
        // No `if_idle`: whether you were watching says nothing about whether you
        // noticed the turn died. This one always pops.
        assert_eq!(n.if_idle, None);
    }

    #[test]
    fn an_unrouted_event_falls_through_as_its_own_name() {
        let n = from_claude(r#"{"hook_event_name":"SessionEnd"}"#, Level::Low, None).unwrap();
        assert_eq!(n.body.as_deref(), Some("SessionEnd"));
    }

    #[test]
    fn survives_a_payload_with_nothing_useful_in_it() {
        let n = from_claude("{}", Level::Normal, None).unwrap();
        assert_eq!(n.title, "Claude Code");
        assert_eq!(n.body, None);
    }

    #[test]
    fn rejects_what_is_not_a_hook_payload() {
        assert!(from_claude("not json", Level::Normal, None).is_err());
        assert!(from_claude("[1,2]", Level::Normal, None).is_err());
    }

    #[test]
    fn leaf_handles_both_separators_and_trailing_ones() {
        assert_eq!(leaf(r"E:\Projects\blip"), Some("blip"));
        assert_eq!(leaf("/home/me/api/"), Some("api"));
        // A session opened at a drive root has no directory name to show, and
        // "C:" is a better answer than falling back to "Claude Code".
        assert_eq!(leaf(r"C:\"), Some("C:"));
        assert_eq!(leaf(""), None);
        assert_eq!(leaf("/"), None);
    }
}
