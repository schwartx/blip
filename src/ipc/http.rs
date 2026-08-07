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

    let path = req.path.split('?').next().unwrap_or("").to_string();

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
        let body = lines.next().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        Ok(NotifyRequest { title, body, ..Default::default() })
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
        let Some((k, v)) = h.split_once(':') else { continue };
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

    Some(Request { method, path, content_type, body })
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
