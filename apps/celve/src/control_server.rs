//! Localhost JSON-lines control server. Each connection may send many requests; each line
//! `{"id":…, "method":…, "params":…}` gets one reply line `{"id":…, "ok":…, …}`.
//!
//! A channel started with a token (the private one Sage's tools use) requires every connection
//! to open with `{"method":"auth","params":{"token":…}}`; anything else closes the connection.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::Sender;
use std::time::Duration;

use gridcraft_ui_egui::ControlRequest;
use serde_json::{Value as Json, json};

const MAX_LINE: usize = 64 * 1024 * 1024;

/// Listens on `127.0.0.1:port` (0 picks a free port) and forwards requests to `tx`. Returns the
/// port actually bound.
pub fn start(port: u16, ctx: egui::Context, tx: Sender<ControlRequest>, token: Option<String>) -> std::io::Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    let bound = listener.local_addr()?.port();
    let private = token.is_some();
    std::thread::Builder::new().name("control".into()).spawn(move || {
        for stream in listener.incoming().flatten() {
            let tx = tx.clone();
            let ctx = ctx.clone();
            let token = token.clone();
            let _ = std::thread::Builder::new().name("control-conn".into()).spawn(move || serve(stream, tx, ctx, token));
        }
    })?;
    if private {
        log::info!("private control channel listening on 127.0.0.1:{bound}");
    } else {
        log::info!("control channel listening on 127.0.0.1:{bound}");
    }
    Ok(bound)
}

/// Whether the first line of a connection presents the expected token.
fn authorized(line: &str, token: &str) -> bool {
    let Ok(msg) = serde_json::from_str::<Json>(line) else { return false };
    msg.get("method").and_then(Json::as_str) == Some("auth")
        && msg.get("params").and_then(|p| p.get("token")).and_then(Json::as_str).is_some_and(|t| constant_time_eq(t.as_bytes(), token.as_bytes()))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn serve(stream: TcpStream, tx: Sender<ControlRequest>, ctx: egui::Context, token: Option<String>) {
    let Ok(read) = stream.try_clone() else { return };
    let mut out = stream;
    let mut reader = BufReader::new(read);
    if let Some(token) = &token {
        // The handshake must arrive promptly; a silent peer doesn't hold a thread forever.
        out.set_read_timeout(Some(Duration::from_secs(10))).ok();
        let mut first = String::new();
        let n = std::io::Read::take(&mut reader, 4096).read_line(&mut first).unwrap_or(0);
        if n == 0 || !authorized(first.trim(), token) {
            let _ = writeln!(out, "{}", json!({"id": 0, "ok": false, "error": "unauthorized"}));
            return;
        }
        out.set_read_timeout(None).ok();
        if writeln!(out, "{}", json!({"id": 0, "ok": true, "result": null})).is_err() {
            return;
        }
    }
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        if line.len() > MAX_LINE {
            let _ = writeln!(out, "{}", json!({"ok": false, "error": "request too large"}));
            continue;
        }
        let msg: Json = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let _ = writeln!(out, "{}", json!({"ok": false, "error": format!("invalid JSON: {e}")}));
                continue;
            }
        };
        let id = msg.get("id").cloned().unwrap_or(Json::Null);
        let method = msg.get("method").and_then(Json::as_str).unwrap_or("").to_string();
        let params = msg.get("params").cloned().unwrap_or(json!({}));
        let (req, rx) = ControlRequest::new(method, params);
        if tx.send(req).is_err() {
            break;
        }
        // Wake the UI thread: egui only runs frames when something happens.
        ctx.request_repaint();
        let mut reply = rx
            .recv_timeout(Duration::from_secs(60))
            .unwrap_or_else(|_| json!({"ok": false, "error": "timed out waiting for the UI (is the window minimised?)"}));
        if let Some(o) = reply.as_object_mut() {
            o.insert("id".into(), id);
        }
        if writeln!(out, "{reply}").is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn token_handshake() {
        assert!(authorized(r#"{"method":"auth","params":{"token":"abc"}}"#, "abc"));
        assert!(!authorized(r#"{"method":"auth","params":{"token":"abd"}}"#, "abc"));
        assert!(!authorized(r#"{"method":"auth","params":{"token":"ab"}}"#, "abc"));
        assert!(!authorized(r#"{"method":"engine.execute","params":{"token":"abc"}}"#, "abc"));
        assert!(!authorized("not json", "abc"));
        assert!(!authorized(r#"{"method":"auth"}"#, "abc"));
    }

    #[test]
    fn private_channel_refuses_strangers_and_serves_the_token_holder() {
        let ctx = egui::Context::default();
        let (tx, rx) = std::sync::mpsc::channel::<ControlRequest>();
        let port = start(0, ctx, tx, Some("s3cret".into())).unwrap();
        // A UI thread stand-in: answers every request with its method name.
        std::thread::spawn(move || {
            while let Ok(req) = rx.recv() {
                let _ = req.reply.send(json!({"ok": true, "result": req.method}));
            }
        });
        let talk = |lines: &[&str]| -> Vec<String> {
            let s = TcpStream::connect(("127.0.0.1", port)).unwrap();
            let mut w = s.try_clone().unwrap();
            let mut r = BufReader::new(s);
            let mut out = vec![];
            for l in lines {
                // A refused connection may already be closed: a failed write ends the talk.
                if writeln!(w, "{l}").is_err() {
                    break;
                }
                let mut reply = String::new();
                if r.read_line(&mut reply).unwrap_or(0) == 0 {
                    break;
                }
                out.push(reply);
            }
            out
        };
        let stranger = talk(&[r#"{"id":1,"method":"document.inspect"}"#, r#"{"id":2,"method":"document.inspect"}"#]);
        assert_eq!(stranger.len(), 1);
        assert!(stranger[0].contains("unauthorized"));
        let friend = talk(&[r#"{"id":0,"method":"auth","params":{"token":"s3cret"}}"#, r#"{"id":5,"method":"document.inspect"}"#]);
        assert_eq!(friend.len(), 2);
        assert!(friend[1].contains("document.inspect") && friend[1].contains("\"id\":5"), "{}", friend[1]);
    }
}
