//! Celve Web server: serves the browser app (the `celve-web` build) and runs Sage for each tab.
//!
//! ```text
//! celve-server [--port 8780] [--bind 127.0.0.1] [--dist dist/web] [--access-key KEY]
//! ```
//!
//! Who may use Sage: either an access key (`?key=…` on the page address), or sign-in through
//! Cloudflare Access for a list of email addresses (see `access.rs`), which replaces the key.
//!
//! The workbook lives in the browser. Each tab opens a WebSocket (`/ws`) and gets its own Sage:
//! an assistant process whose only spreadsheet tools are Celve's MCP tools, served by this
//! executable in bridge mode (`celve-server --mcp-bridge <port>`). The bridge talks to a private,
//! token-protected loopback port owned by the tab's session, and every request it sends is
//! forwarded over the WebSocket to the browser, which runs it on the live workbook (the same
//! control methods as the desktop app's control channel) and sends the reply back. So whatever
//! Sage does, the person watching the tab sees it happen.
//!
//! WebSocket messages (JSON text):
//! - server → browser: `{"t":"hello","sage":{"ok":bool,"why":…}}`, `{"t":"sage","event":{kind,value}}`,
//!   `{"t":"ctl","rid":n,"method":…,"params":…}`
//! - browser → server: `{"t":"sage.send","text","context","pictures":[{name,mime,data}]}`,
//!   `{"t":"sage.stop"}`, `{"t":"sage.reset"}`, `{"t":"sage.warm"}`, `{"t":"ctl.reply","rid":n,"reply":{ok,…}}`
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

mod access;

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::Duration;

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::{Value as Json, json};
use tokio::sync::{Notify, mpsc as tmpsc};

/// Tabs with a Sage at the same time (each runs an assistant process).
const MAX_SESSIONS: usize = 16;
/// Longest WebSocket message accepted (pictures arrive base64-encoded).
const MAX_MESSAGE: usize = 40 * 1024 * 1024;
/// Longest line accepted from the bridge.
const MAX_LINE: u64 = 64 * 1024 * 1024;
/// How long the bridge waits for the browser to answer one request.
const REPLY_TIMEOUT: Duration = Duration::from_secs(90);
/// Pictures per message and bytes per picture.
const MAX_PICTURES: usize = 6;
const MAX_PICTURE_BYTES: usize = 5 * 1024 * 1024;

#[derive(Clone)]
struct Config {
    access_key: String,
    /// No key was configured (one was generated for this run): a local-only server, so only
    /// loopback host names may connect.
    local_only: bool,
    /// Extra page origins allowed to connect (`CELVE_ALLOWED_ORIGINS`, comma-separated).
    allowed_origins: Arc<Vec<String>>,
    /// Sign-in through Cloudflare Access; when set, it replaces the access key.
    gate: Option<Arc<access::Gate>>,
    /// The app's index page (with the sign-in marker when sign-in is on).
    index: Arc<String>,
    workroot: PathBuf,
    sessions: Arc<AtomicUsize>,
    next_id: Arc<AtomicU64>,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--mcp-bridge") {
        std::process::exit(celve_sage::run_bridge(args.get(1).map(String::as_str).unwrap_or("")));
    }
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let mut port: u16 = std::env::var("CELVE_SERVER_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8780);
    let mut bind = std::env::var("CELVE_SERVER_BIND").unwrap_or_else(|_| "127.0.0.1".into());
    let mut dist = std::env::var("CELVE_WEB_DIST").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("dist/web"));
    let mut access_key = std::env::var("CELVE_ACCESS_KEY").ok().filter(|k| !k.is_empty());
    let mut i = 0;
    while i < args.len() {
        let next = args.get(i + 1).cloned();
        match args.get(i).map(String::as_str) {
            Some("--port") => port = next.and_then(|p| p.parse().ok()).unwrap_or(port),
            Some("--bind") => bind = next.unwrap_or(bind),
            Some("--dist") => dist = next.map(PathBuf::from).unwrap_or(dist),
            Some("--access-key") => access_key = next.filter(|k| !k.is_empty()),
            Some("--help" | "-h") => {
                println!("celve-server [--port 8780] [--bind 127.0.0.1] [--dist DIR] [--access-key KEY]");
                return;
            }
            _ => {
                i += 1;
                continue;
            }
        }
        i += 2;
    }
    if !dist.join("index.html").is_file() {
        log::error!("no web build at {} (run `trunk build --release` in apps/celve-web, or pass --dist)", dist.display());
        std::process::exit(2);
    }
    let gate = match access::Gate::from_env() {
        Some(Ok(g)) => Some(g),
        Some(Err(e)) => {
            eprintln!("celve-server: {e}");
            std::process::exit(2);
        }
        None => None,
    };
    let index = match std::fs::read_to_string(dist.join("index.html")) {
        Ok(page) if gate.is_some() => page.replacen("</head>", "<meta name=\"celve-signin\" content=\"/signin\">\n</head>", 1),
        Ok(page) => page,
        Err(e) => {
            eprintln!("celve-server: {}: {e} (build the web app first)", dist.join("index.html").display());
            std::process::exit(2);
        }
    };
    // Sage always needs a key: without one, any page running in your browser could reach a local
    // server. When none is configured, make one for this run and print the link that carries it.
    let generated = access_key.is_none() && gate.is_none();
    let access_key = access_key.unwrap_or_else(|| celve_sage::new_token().chars().take(24).collect());
    let access_key_shown = access_key.clone();
    let config = Config {
        access_key,
        local_only: generated,
        allowed_origins: Arc::new(
            std::env::var("CELVE_ALLOWED_ORIGINS")
                .unwrap_or_default()
                .split(',')
                .map(|o| o.trim().trim_end_matches('/').to_string())
                .filter(|o| !o.is_empty())
                .collect(),
        ),
        gate: gate.clone(),
        index: Arc::new(index),
        workroot: std::env::temp_dir().join("celve-sage"),
        sessions: Arc::new(AtomicUsize::new(0)),
        next_id: Arc::new(AtomicU64::new(1)),
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(r) => r,
        Err(e) => {
            log::error!("runtime: {e}");
            std::process::exit(1);
        }
    };
    runtime.block_on(async move {
        let app = Router::new()
            .route("/ws", get(ws_handler))
            .route("/healthz", get(|| async { "ok" }))
            .route("/", get(index_page))
            .route("/index.html", get(index_page))
            // Cloudflare Access guards this page: arriving here means sign-in is done.
            .route("/signin", get(|| async { axum::response::Redirect::to("/") }))
            .fallback_service(tower_http::services::ServeDir::new(&dist).append_index_html_on_directories(true))
            .with_state(config);
        let listener = match tokio::net::TcpListener::bind((bind.as_str(), port)).await {
            Ok(l) => l,
            Err(e) => {
                log::error!("bind {bind}:{port}: {e}");
                std::process::exit(1);
            }
        };
        let shown = if bind == "0.0.0.0" { "127.0.0.1" } else { bind.as_str() };
        log::info!("Celve Web (app from {})", dist.display());
        if let Some(g) = &gate {
            log::info!("Sage needs sign-in through {} for: {}", g.team(), g.emails().join(", "));
        } else if generated {
            log::info!("open http://{shown}:{port}/?key={access_key_shown}  (key generated for this run; set CELVE_ACCESS_KEY to keep one)");
        } else {
            log::info!("open http://{shown}:{port}/?key=<your CELVE_ACCESS_KEY>");
        }
        if let Err(e) = axum::serve(listener, app).await {
            log::error!("server: {e}");
        }
    });
}

async fn ws_handler(ws: WebSocketUpgrade, headers: HeaderMap, Query(q): Query<HashMap<String, String>>, State(config): State<Config>) -> Response {
    // Browsers send their page's origin: only Celve's own page may open a Sage session, so a
    // website you happen to visit can't connect to this server and drive Sage.
    if !origin_allowed(
        headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()),
        headers.get(header::HOST).and_then(|v| v.to_str().ok()),
        &config.allowed_origins,
    ) {
        log::warn!("refused a WebSocket from another site's page");
        return (StatusCode::FORBIDDEN, "cross-site connection refused").into_response();
    }
    // DNS rebinding: an attacker's domain resolving to this machine would pass the origin check
    // (its page's Origin and Host agree). A local-only server (generated key) also accepts only
    // loopback host names and the origins you allow.
    if config.local_only && !local_host_allowed(headers.get(header::HOST).and_then(|v| v.to_str().ok()), &config.allowed_origins) {
        log::warn!("refused a WebSocket for a non-local host name without an access key");
        return (StatusCode::FORBIDDEN, "set CELVE_ACCESS_KEY to serve Sage under this host name").into_response();
    }
    if let Some(gate) = &config.gate {
        match gate.check(&headers).await {
            Ok(email) => log::info!("Sage session for {email}"),
            Err(why) => {
                log::warn!("refused a WebSocket: {why}");
                return (StatusCode::UNAUTHORIZED, "sign in to use Sage").into_response();
            }
        }
    } else if !q.get("key").is_some_and(|k| constant_time_eq(k.as_bytes(), config.access_key.as_bytes())) {
        return (StatusCode::UNAUTHORIZED, "access key required").into_response();
    }
    ws.max_message_size(MAX_MESSAGE).on_upgrade(move |socket| session(socket, config))
}

async fn index_page(State(config): State<Config>) -> Response {
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8"), (header::CACHE_CONTROL, "no-cache")], config.index.as_str().to_string()).into_response()
}

/// Same-origin check: the `Origin` header's host must be the `Host` the page was served from (or
/// one of `CELVE_ALLOWED_ORIGINS`). No `Origin` (not a browser) is allowed; the access key still
/// applies.
fn origin_allowed(origin: Option<&str>, host: Option<&str>, extra: &[String]) -> bool {
    let Some(origin) = origin else { return true };
    if extra.iter().any(|o| o.eq_ignore_ascii_case(origin)) {
        return true;
    }
    let origin_host = origin.split_once("://").map(|(_, rest)| rest).unwrap_or("");
    host.is_some_and(|h| !h.is_empty() && origin_host.eq_ignore_ascii_case(h))
}

/// The `Host` is a loopback name (`127.0.0.1`, `localhost`, `[::1]`, any port) or the host of
/// one of the allowed origins.
fn local_host_allowed(host: Option<&str>, extra: &[String]) -> bool {
    let Some(host) = host.map(str::to_ascii_lowercase) else { return false };
    let name = if host.starts_with('[') {
        host.split(']').next().map(|h| format!("{h}]")).unwrap_or_default()
    } else {
        host.split(':').next().unwrap_or("").to_string()
    };
    matches!(name.as_str(), "127.0.0.1" | "localhost" | "[::1]")
        || extra.iter().any(|o| o.split_once("://").map(|(_, h)| h).is_some_and(|h| h.eq_ignore_ascii_case(&host)))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Requests from the bridge waiting for the browser's answer, by request id.
type Pending = Arc<Mutex<HashMap<u64, mpsc::Sender<Json>>>>;

async fn session(mut socket: WebSocket, config: Config) {
    let id = config.next_id.fetch_add(1, Ordering::SeqCst);
    let over_limit = config.sessions.fetch_add(1, Ordering::SeqCst) >= MAX_SESSIONS;
    let _guard = SessionGuard(config.sessions.clone());
    let (out_tx, mut out_rx) = tmpsc::unbounded_channel::<Json>();
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
    let closed = Arc::new(AtomicBool::new(false));
    let notify = Arc::new(Notify::new());

    // The tab's private control port: the bridge's requests go to the browser.
    let token = celve_sage::new_token();
    let port = if over_limit {
        Err("Sage is busy with other tabs right now. Try again in a little while.".to_string())
    } else {
        start_bridge_port(token.clone(), out_tx.clone(), pending.clone(), closed.clone())
    };
    let mut sage = match &port {
        Ok(port) => {
            let wake_notify = notify.clone();
            let wake: celve_sage::Wake = Arc::new(move || wake_notify.notify_one());
            Some(celve_sage::Sage::new(*port, token, config.workroot.join(format!("tab-{id}")), wake))
        }
        Err(_) => None,
    };
    let availability = match (&sage, &port) {
        (Some(s), _) => s.availability(),
        (None, Err(e)) => Err(e.clone()),
        (None, Ok(_)) => Err("Sage isn't available.".into()),
    };
    let hello = json!({"t": "hello", "sage": {"ok": availability.is_ok(), "why": availability.err()}});
    if socket.send(Message::Text(hello.to_string().into())).await.is_err() {
        return;
    }
    log::info!("tab {id} connected");
    loop {
        tokio::select! {
            msg = socket.recv() => {
                let Some(Ok(msg)) = msg else { break };
                let text = match msg {
                    Message::Text(t) => t.to_string(),
                    Message::Close(_) => break,
                    _ => continue,
                };
                let Ok(v) = serde_json::from_str::<Json>(&text) else { continue };
                if let Some(sage) = sage.as_mut() {
                    handle(&v, sage, &pending);
                } else if v.get("t").and_then(Json::as_str) == Some("ctl.reply") {
                    handle_reply(&v, &pending);
                }
            }
            Some(out) = out_rx.recv() => {
                if socket.send(Message::Text(out.to_string().into())).await.is_err() {
                    break;
                }
            }
            () = notify.notified() => {
                let events = sage.as_mut().map(celve_sage::Sage::poll).unwrap_or_default();
                for e in events {
                    let m = json!({"t": "sage", "event": e});
                    if socket.send(Message::Text(m.to_string().into())).await.is_err() {
                        break;
                    }
                }
            }
        }
    }
    closed.store(true, Ordering::SeqCst);
    drop(sage); // kills the assistant process
    if let Ok(port) = port {
        // Wake the accept loop so it sees `closed`.
        let _ = TcpStream::connect(("127.0.0.1", port));
    }
    // Fail everything still waiting for this tab.
    pending.lock().unwrap_or_else(PoisonError::into_inner).clear();
    log::info!("tab {id} closed");
}

struct SessionGuard(Arc<AtomicUsize>);

impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn handle(v: &Json, sage: &mut celve_sage::Sage, pending: &Pending) {
    match v.get("t").and_then(Json::as_str) {
        Some("ctl.reply") => handle_reply(v, pending),
        Some("sage.warm") => sage.warm_up(),
        Some("sage.stop") => sage.stop(),
        Some("sage.reset") => sage.reset(),
        Some("sage.send") => {
            let text = v.get("text").and_then(Json::as_str).unwrap_or("");
            let context = v.get("context").and_then(Json::as_str).unwrap_or("");
            sage.send(text, &pictures(v), context);
        }
        _ => {}
    }
}

/// Pictures from a `sage.send` message (bad or oversized ones are dropped).
fn pictures(v: &Json) -> Vec<celve_sage::Picture> {
    v.get("pictures")
        .and_then(Json::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .take(MAX_PICTURES)
        .filter_map(|p| {
            let mime = p.get("mime").and_then(Json::as_str).filter(|m| matches!(*m, "image/png" | "image/jpeg" | "image/gif" | "image/webp"))?;
            let bytes = p
                .get("data")
                .and_then(Json::as_str)
                .and_then(gridcraft_engine::io::base64_decode)
                .filter(|b| !b.is_empty() && b.len() <= MAX_PICTURE_BYTES)?;
            let name = p.get("name").and_then(Json::as_str).unwrap_or("picture").chars().take(120).collect();
            Some(celve_sage::Picture { name, mime: mime.to_string(), bytes })
        })
        .collect()
}

fn handle_reply(v: &Json, pending: &Pending) {
    let Some(rid) = v.get("rid").and_then(Json::as_u64) else { return };
    let waiter = pending.lock().unwrap_or_else(PoisonError::into_inner).remove(&rid);
    if let Some(tx) = waiter {
        let _ = tx.send(v.get("reply").cloned().unwrap_or(json!({"ok": false, "error": "empty reply"})));
    }
}

/// Opens the tab's private loopback port for the MCP bridge and returns it.
fn start_bridge_port(token: String, out: tmpsc::UnboundedSender<Json>, pending: Pending, closed: Arc<AtomicBool>) -> Result<u16, String> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(|e| e.to_string())?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let next_rid = Arc::new(AtomicU64::new(1));
    std::thread::Builder::new()
        .name("celve-bridge-port".into())
        .spawn(move || {
            for stream in listener.incoming() {
                if closed.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let (token, out, pending, closed, next_rid) = (token.clone(), out.clone(), pending.clone(), closed.clone(), next_rid.clone());
                let _ = std::thread::Builder::new()
                    .name("celve-bridge-conn".into())
                    .spawn(move || serve_bridge(stream, &token, &out, &pending, &closed, &next_rid));
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(port)
}

/// One bridge connection: token handshake, then each request line is answered by the browser.
fn serve_bridge(stream: TcpStream, token: &str, out: &tmpsc::UnboundedSender<Json>, pending: &Pending, closed: &AtomicBool, next_rid: &AtomicU64) {
    let Ok(read) = stream.try_clone() else { return };
    let mut w = stream;
    let mut r = BufReader::new(read);
    w.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let mut first = String::new();
    let _ = std::io::Read::take(&mut r, 4096).read_line(&mut first);
    let ok = serde_json::from_str::<Json>(first.trim()).ok().is_some_and(|m| {
        m.get("method").and_then(Json::as_str) == Some("auth")
            && m.pointer("/params/token").and_then(Json::as_str).is_some_and(|t| constant_time_eq(t.as_bytes(), token.as_bytes()))
    });
    if !ok {
        let _ = writeln!(w, "{}", json!({"id": 0, "ok": false, "error": "unauthorized"}));
        return;
    }
    w.set_read_timeout(None).ok();
    if writeln!(w, "{}", json!({"id": 0, "ok": true, "result": null})).is_err() {
        return;
    }
    loop {
        let mut line = String::new();
        match std::io::Read::take(&mut r, MAX_LINE).read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if closed.load(Ordering::SeqCst) {
            break;
        }
        let Ok(msg) = serde_json::from_str::<Json>(line.trim()) else {
            let _ = writeln!(w, "{}", json!({"ok": false, "error": "invalid JSON"}));
            continue;
        };
        let id = msg.get("id").cloned().unwrap_or(Json::Null);
        let rid = next_rid.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel();
        pending.lock().unwrap_or_else(PoisonError::into_inner).insert(rid, tx);
        let request = json!({"t": "ctl", "rid": rid, "method": msg.get("method").cloned().unwrap_or(Json::Null), "params": msg.get("params").cloned().unwrap_or(json!({}))});
        let mut reply = if out.send(request).is_err() {
            json!({"ok": false, "error": "the Celve tab closed"})
        } else {
            rx.recv_timeout(REPLY_TIMEOUT)
                .unwrap_or_else(|_| json!({"ok": false, "error": "the Celve tab didn't answer (is it in the background or closed?)"}))
        };
        pending.lock().unwrap_or_else(PoisonError::into_inner).remove(&rid);
        if let Some(o) = reply.as_object_mut() {
            o.insert("id".into(), id);
        }
        if writeln!(w, "{reply}").is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn only_celve_pages_may_connect() {
        let none: Vec<String> = vec![];
        assert!(origin_allowed(Some("http://127.0.0.1:8780"), Some("127.0.0.1:8780"), &none));
        assert!(origin_allowed(Some("https://celve.example.com"), Some("celve.example.com"), &none));
        assert!(!origin_allowed(Some("https://evil.example"), Some("127.0.0.1:8780"), &none));
        assert!(!origin_allowed(Some("http://127.0.0.1:8780.evil.example"), Some("127.0.0.1:8780"), &none));
        assert!(!origin_allowed(Some("null"), Some("127.0.0.1:8780"), &none));
        assert!(!origin_allowed(Some("http://127.0.0.1:8780"), None, &none));
        assert!(origin_allowed(None, Some("127.0.0.1:8780"), &none));
        assert!(origin_allowed(Some("https://app.example"), Some("api.example"), &["https://app.example".into()]));
    }

    #[test]
    fn rebinding_names_are_refused_without_a_key() {
        let none: Vec<String> = vec![];
        assert!(local_host_allowed(Some("127.0.0.1:8780"), &none));
        assert!(local_host_allowed(Some("localhost:8780"), &none));
        assert!(local_host_allowed(Some("[::1]:8780"), &none));
        assert!(!local_host_allowed(Some("evil.example:8780"), &none));
        assert!(!local_host_allowed(Some("127.0.0.1.evil.example:8780"), &none));
        assert!(!local_host_allowed(Some("localhost.evil.example"), &none));
        assert!(!local_host_allowed(None, &none));
        assert!(local_host_allowed(Some("celve.example.com"), &["https://celve.example.com".into()]));
    }

    #[test]
    fn pictures_are_checked() {
        let ok = gridcraft_engine::io::base64_encode(&[1, 2, 3]);
        let v = json!({"pictures": [
            {"name": "a.png", "mime": "image/png", "data": ok},
            {"name": "b.svg", "mime": "image/svg+xml", "data": ok},
            {"name": "c.png", "mime": "image/png", "data": "!!!"},
            {"name": "d.png", "mime": "image/png", "data": ""},
        ]});
        let p = pictures(&v);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].bytes, [1, 2, 3]);
        assert!(pictures(&json!({})).is_empty());
        assert!(pictures(&json!({"pictures": 7})).is_empty());
    }

    #[test]
    fn bridge_requests_reach_the_browser_and_come_back() {
        let (out_tx, mut out_rx) = tmpsc::unbounded_channel::<Json>();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let closed = Arc::new(AtomicBool::new(false));
        let port = start_bridge_port("tok".into(), out_tx, pending.clone(), closed.clone()).unwrap();
        // A fake browser: answers each forwarded request with its method name.
        let p2 = pending.clone();
        let browser = std::thread::spawn(move || {
            while let Some(req) = out_rx.blocking_recv() {
                handle_reply(&json!({"t": "ctl.reply", "rid": req["rid"], "reply": {"ok": true, "result": req["method"]}}), &p2);
            }
        });
        // Strangers are refused.
        let mut b = gridcraft_engine_free_remote(port, "nope");
        assert!(b.is_none());
        b = gridcraft_engine_free_remote(port, "tok");
        let (mut w, mut r) = b.unwrap();
        writeln!(w, "{}", json!({"id": 42, "method": "document.inspect", "params": {}})).unwrap();
        let mut line = String::new();
        r.read_line(&mut line).unwrap();
        let v: Json = serde_json::from_str(&line).unwrap();
        assert_eq!(v["id"], 42);
        assert_eq!(v["result"], "document.inspect");
        closed.store(true, Ordering::SeqCst);
        drop((w, r));
        let _ = TcpStream::connect(("127.0.0.1", port));
        drop(browser);
    }

    /// Connects and authenticates like the MCP bridge does; `None` when refused.
    fn gridcraft_engine_free_remote(port: u16, token: &str) -> Option<(TcpStream, BufReader<TcpStream>)> {
        let s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let mut w = s.try_clone().unwrap();
        let mut r = BufReader::new(s);
        writeln!(w, "{}", json!({"id": 0, "method": "auth", "params": {"token": token}})).unwrap();
        let mut line = String::new();
        r.read_line(&mut line).unwrap();
        line.contains("\"ok\":true").then_some((w, r))
    }
}
