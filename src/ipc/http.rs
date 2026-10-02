//! HTTP transport — the remote entry point.
//!
//! Deliberately hand-rolled against `std::net` rather than pulling in an async
//! runtime. The whole surface is four routes with short-lived requests, and a
//! thread parked in `accept()` costs literally zero CPU while idle, which
//! matters for something that sits in the tray all day.
//!
//! ```text
//! curl -d "构建完成" http://127.0.0.1:7788/notify
//! curl -X POST http://127.0.0.1:7788/notify -H 'Content-Type: application/json' \
//!      -d '{"title":"部署失败","body":"3 个健康检查未通过","level":"critical","id":"deploy"}'
//! ```

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use crate::ipc::Bridge;
use crate::model::{Command, NotifyRequest};

const MAX_BODY: usize = 256 * 1024;

pub fn serve(bind: &str, bridge: Bridge) -> Result<(), String> {
    let listener = TcpListener::bind(bind).map_err(|e| format!("bind {bind} failed: {e}"))?;
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let bridge = bridge.clone();
        // Thread-per-connection: requests are tiny and short-lived, and this
        // keeps one hung client from stalling every other notification.
        std::thread::spawn(move || {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
            handle(stream, &bridge);
        });
    }
    Ok(())
}

struct Request {
    method: String,
    path: String,
    content_type: String,
    body: Vec<u8>,
}

fn handle(mut stream: TcpStream, bridge: &Bridge) {
    let req = match parse(&mut stream) {
        Some(r) => r,
        None => return respond(&mut stream, 400, "bad request"),
    };

    let (path, query) = match req.path.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (req.path.clone(), String::new()),
    };

    match (req.method.as_str(), path.as_str()) {
        ("GET", "/health") => {
            let body = format!("{{\"ok\":true,\"version\":\"{}\"}}", crate::VERSION);
            respond_json(&mut stream, 200, &body)
        }

        ("POST", "/notify") | ("PUT", "/notify") => match decode_notify(&req) {
            Ok(n) if n.title.trim().is_empty() && n.body.is_none() => {
                respond(&mut stream, 400, "title is required")
            }
            Ok(n) => {
                bridge.send(Command::Notify(n));
                respond_json(&mut stream, 200, "{\"ok\":true}")
            }
            Err(e) => respond(&mut stream, 400, &e),
        },

        // Claude Code's `http` hook handler posts here directly, with no script
        // in between. See `ipc::hook`.
        ("POST", "/hook/claude") => {
            let level = param(&query, "level")
                .and_then(|v| crate::model::Level::parse(&v))
                .unwrap_or_default();
            // Unparseable means "pop normally". A typo in a hook URL should not
            // silently make a notification stop appearing.
            let if_idle = param(&query, "if_idle").and_then(|v| v.parse::<f32>().ok());
            match crate::ipc::hook::from_claude(&String::from_utf8_lossy(&req.body), level, if_idle)
            {
                Ok(n) => {
                    bridge.send(Command::Notify(n));
                    // Empty body, not `{"ok":true}`: Claude Code parses a 2xx
                    // JSON body as a hook *decision*, and an unknown-shaped
                    // object there is asking for trouble. Empty means "fine,
                    // nothing to say".
                    respond_empty(&mut stream)
                }
                Err(e) => respond(&mut stream, 400, &e),
            }
        }

        ("DELETE", p) if p.starts_with("/notify/") => {
            let id = p.trim_start_matches("/notify/");
            if id.is_empty() {
                respond(&mut stream, 400, "missing id")
            } else {
                bridge.send(Command::Dismiss { id: id.to_string() });
                respond_json(&mut stream, 200, "{\"ok\":true}")
            }
        }

        ("POST", "/clear") => {
            bridge.send(Command::Clear);
            respond_json(&mut stream, 200, "{\"ok\":true}")
        }
        ("POST", "/show") => {
            bridge.send(Command::Show);
            respond_json(&mut stream, 200, "{\"ok\":true}")
        }

        _ => respond(&mut stream, 404, "not found"),
    }
}

/// JSON when the caller says so, otherwise the raw body becomes the title.
///
/// That fallback is what makes `curl -d "文本" .../notify` work, and it's the
/// difference between "anything that can speak HTTP can use this" and "you must
/// first learn my schema".
fn decode_notify(req: &Request) -> Result<NotifyRequest, String> {
    let text = String::from_utf8_lossy(&req.body);
    if req.content_type.contains("json") {
        serde_json::from_str::<NotifyRequest>(&text).map_err(|e| format!("bad json: {e}"))
    } else {
        let text = text.trim();
        // A bare body that happens to be a JSON object is almost certainly
        // someone who forgot the header. Accept it.
        if text.starts_with('{')
            && let Ok(n) = serde_json::from_str::<NotifyRequest>(text)
        {
            return Ok(n);
        }
        let mut lines = text.splitn(2, '\n');
        let title = lines.next().unwrap_or("").trim().to_string();
        let body = lines
            .next()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        Ok(NotifyRequest {
            title,
            body,
            ..Default::default()
        })
    }
}

fn parse(stream: &mut TcpStream) -> Option<Request> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);

    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();

    let mut len = 0usize;
    let mut content_type = String::new();

    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).ok()? == 0 {
            break;
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        let Some((k, v)) = h.split_once(':') else {
            continue;
        };
        let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
        match k.as_str() {
            "content-length" => len = v.parse().unwrap_or(0),
            "content-type" => content_type = v.to_ascii_lowercase(),
            _ => {}
        }
    }

    if len > MAX_BODY {
        return None;
    }
    let mut body = vec![0u8; len];
    if len > 0 {
        reader.read_exact(&mut body).ok()?;
    }

    Some(Request {
        method,
        path,
        content_type,
        body,
    })
}

fn respond(stream: &mut TcpStream, code: u16, msg: &str) {
    let reason = match code {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Error",
    };
    let payload = format!(
        "HTTP/1.1 {code} {reason}\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{msg}",
        msg.len()
    );
    let _ = stream.write_all(payload.as_bytes());
}

/// One query-string value, or `None`.
///
/// No percent-decoding: the only parameter read here is a level keyword, and a
/// decoder that is never exercised is a decoder that is quietly wrong.
fn param(query: &str, key: &str) -> Option<String> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.to_string())
}

fn respond_empty(stream: &mut TcpStream) {
    let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
}

fn respond_json(stream: &mut TcpStream, code: u16, body: &str) {
    let payload = format!(
        "HTTP/1.1 {code} OK\r\n\
         Content-Type: application/json; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(payload.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Level;
    use std::sync::mpsc::{Receiver, channel};

    // Exercise the real socket parser and command bridge, with one connection
    // per test so no background listener leaks into the next test.
    fn request(
        method: &str,
        path: &str,
        content_type: &str,
        body: &str,
    ) -> (String, Receiver<Command>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, rx) = channel();
        let (bridge, _) = Bridge::new(tx);
        let worker = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            handle(stream, &bridge);
        });
        let mut stream = TcpStream::connect(address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        worker.join().unwrap();
        (response, rx)
    }

    #[test]
    fn health_reports_version_without_dispatching_a_command() {
        let (response, rx) = request("GET", "/health", "", "");
        assert!(response.starts_with("HTTP/1.1 200"));
        let body: serde_json::Value =
            serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(body["ok"], true);
        assert_eq!(body["version"], crate::VERSION);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn unicode_plain_text_reaches_the_notification_bridge() {
        let (response, rx) = request(
            "POST",
            "/notify",
            "text/plain",
            "构建完成\nmacOS 与 Windows",
        );
        assert!(response.starts_with("HTTP/1.1 200"));
        let Command::Notify(note) = rx.try_recv().unwrap() else {
            panic!("expected notification")
        };
        assert_eq!(note.title, "构建完成");
        assert_eq!(note.body.as_deref(), Some("macOS 与 Windows"));
    }

    #[test]
    fn json_progress_and_idle_condition_are_preserved() {
        let (response, rx) = request(
            "POST",
            "/notify",
            "application/json",
            r#"{"title":"Build","id":"build","progress":60,"level":"critical","if_idle":15}"#,
        );
        assert!(response.starts_with("HTTP/1.1 200"));
        let Command::Notify(note) = rx.try_recv().unwrap() else {
            panic!("expected notification")
        };
        assert_eq!(note.id.as_deref(), Some("build"));
        assert_eq!(note.progress, Some(60));
        assert_eq!(note.level, Some(Level::Critical));
        assert_eq!(note.if_idle, Some(15.0));
    }

    #[test]
    fn macos_claude_hook_dispatches_and_returns_an_empty_success() {
        let (response, rx) = request(
            "POST",
            "/hook/claude?level=normal&if_idle=15",
            "application/json",
            r#"{"cwd":"/Users/me/projects/blip","session_id":"mac-session","last_assistant_message":"Done","hook_event_name":"Stop"}"#,
        );
        assert!(response.starts_with("HTTP/1.1 200"));
        assert_eq!(response.split_once("\r\n\r\n").unwrap().1, "");
        let Command::Notify(note) = rx.try_recv().unwrap() else {
            panic!("expected notification")
        };
        assert_eq!(note.title, "blip");
        assert_eq!(note.id.as_deref(), Some("cc-mac-session"));
        assert_eq!(note.body.as_deref(), Some("Done"));
        assert_eq!(note.if_idle, Some(15.0));
    }

    #[test]
    fn dismiss_and_clear_dispatch_their_commands() {
        let (response, rx) = request("DELETE", "/notify/build", "", "");
        assert!(response.starts_with("HTTP/1.1 200"));
        assert!(matches!(rx.try_recv().unwrap(), Command::Dismiss { id } if id == "build"));
        let (response, rx) = request("POST", "/clear", "", "");
        assert!(response.starts_with("HTTP/1.1 200"));
        assert!(matches!(rx.try_recv().unwrap(), Command::Clear));
    }

    #[test]
    fn malformed_json_does_not_dispatch() {
        let (response, rx) = request("POST", "/notify", "application/json", "{broken");
        assert!(response.starts_with("HTTP/1.1 400"));
        assert!(rx.try_recv().is_err());
    }
}
