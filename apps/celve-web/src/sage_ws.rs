//! Sage in the browser: a [`SageHost`] over a WebSocket to `celve-server`.
//!
//! The server runs the assistant; its spreadsheet requests come back here as `ctl` messages and
//! run on the live workbook through the same control methods as the desktop control channel
//! (so the person watching sees every change as it happens), and the replies go back as
//! `ctl.reply`. See `apps/celve-server/src/main.rs` for the message list.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::Receiver;

use gridcraft_ui_egui::ControlRequest;
use gridcraft_ui_egui::sage::{SageEvent, SageHost, SagePicture};
use serde_json::{Value as Json, json};
use wasm_bindgen::JsCast as _;
use wasm_bindgen::closure::Closure;

/// Seconds between reconnection attempts after the socket drops.
const RECONNECT_MS: i32 = 3000;

#[derive(Default)]
struct State {
    socket: Option<web_sys::WebSocket>,
    open: bool,
    /// From the server's hello: why Sage can't run, if it can't.
    unavailable: Option<String>,
    /// The page isn't served by celve-server (no /ws), or the access key was refused.
    offline: Option<String>,
    events: Vec<SageEvent>,
    /// Control requests from Sage waiting to run: (rid, method, params).
    requests: Vec<(u64, String, Json)>,
    warm_wanted: bool,
    /// The server's sign-in page, when Sage is behind sign-in (from the page's
    /// `<meta name="celve-signin">`).
    signin: Option<String>,
}

#[derive(Clone)]
pub struct WebSage {
    state: Rc<RefCell<State>>,
    ctx: egui::Context,
}

impl WebSage {
    pub fn connect(ctx: egui::Context) -> Self {
        let signin = signin_page();
        let s = WebSage { state: Rc::new(RefCell::new(State { signin, ..State::default() })), ctx };
        s.open_socket();
        s
    }

    fn url() -> Option<String> {
        let loc = web_sys::window()?.location();
        let proto = if loc.protocol().ok()? == "https:" { "wss:" } else { "ws:" };
        let host = loc.host().ok()?;
        let path = loc.pathname().ok().unwrap_or_default();
        let base = path.rsplit_once('/').map(|(b, _)| b).unwrap_or("");
        let key = loc.search().ok().unwrap_or_default().trim_start_matches('?').split('&').find_map(|kv| kv.strip_prefix("key=")).map(str::to_string);
        Some(format!("{proto}//{host}{base}/ws{}", key.map(|k| format!("?key={k}")).unwrap_or_default()))
    }

    fn open_socket(&self) {
        let Some(url) = Self::url() else { return };
        let ws = match web_sys::WebSocket::new(&url) {
            Ok(ws) => ws,
            Err(e) => {
                self.state.borrow_mut().offline = Some(format!("Sage can't connect: {e:?}"));
                return;
            }
        };
        let me = self.clone();
        let on_open = Closure::<dyn FnMut()>::new(move || {
            let mut st = me.state.borrow_mut();
            st.open = true;
            st.offline = None;
            let warm = st.warm_wanted;
            drop(st);
            if warm {
                me.send_json(&json!({"t": "sage.warm"}));
            }
            me.ctx.request_repaint();
        });
        ws.set_onopen(Some(on_open.as_ref().unchecked_ref()));
        on_open.forget();
        let me = self.clone();
        let on_message = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |e: web_sys::MessageEvent| {
            if let Some(text) = e.data().as_string() {
                me.on_message(&text);
            }
        });
        ws.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        on_message.forget();
        let me = self.clone();
        let on_close = Closure::<dyn FnMut(web_sys::CloseEvent)>::new(move |_e: web_sys::CloseEvent| {
            let was_open = {
                let mut st = me.state.borrow_mut();
                let was = st.open;
                st.open = false;
                st.socket = None;
                if was {
                    st.offline = Some("Sage got disconnected. Reconnecting…".into());
                    st.events.push(SageEvent::Error("Sage got disconnected. Reconnecting…".into()));
                } else if st.signin.is_some() {
                    st.offline = Some("Sign in to use Sage.".into());
                } else if st.offline.is_none() {
                    st.offline =
                        Some("Sage needs the Celve server: open Celve from its server address (and include ?key=… if it has an access key).".into());
                }
                was
            };
            me.ctx.request_repaint();
            let again = me.clone();
            let retry = Closure::once_into_js(move || again.open_socket());
            if let Some(w) = web_sys::window() {
                let _ = w.set_timeout_with_callback_and_timeout_and_arguments_0(retry.unchecked_ref(), if was_open { 500 } else { RECONNECT_MS });
            }
        });
        ws.set_onclose(Some(on_close.as_ref().unchecked_ref()));
        on_close.forget();
        self.state.borrow_mut().socket = Some(ws);
    }

    fn on_message(&self, text: &str) {
        let Ok(v) = serde_json::from_str::<Json>(text) else { return };
        let mut st = self.state.borrow_mut();
        match v.get("t").and_then(Json::as_str) {
            Some("hello") => {
                st.unavailable = if v.pointer("/sage/ok").and_then(Json::as_bool) == Some(true) {
                    None
                } else {
                    Some(v.pointer("/sage/why").and_then(Json::as_str).unwrap_or("Sage isn't available on this server.").to_string())
                };
            }
            Some("sage") => {
                if let Some(e) = v.get("event").and_then(event) {
                    st.events.push(e);
                }
            }
            Some("ctl") => {
                let rid = v.get("rid").and_then(Json::as_u64).unwrap_or(0);
                let method = v.get("method").and_then(Json::as_str).unwrap_or("").to_string();
                st.requests.push((rid, method, v.get("params").cloned().unwrap_or(json!({}))));
            }
            _ => {}
        }
        drop(st);
        self.ctx.request_repaint();
    }

    fn send_json(&self, v: &Json) -> bool {
        let st = self.state.borrow();
        match (&st.socket, st.open) {
            (Some(ws), true) => ws.send_with_str(&v.to_string()).is_ok(),
            _ => false,
        }
    }

    /// Control requests from Sage to run this frame.
    pub fn take_requests(&self) -> Vec<(u64, String, Json)> {
        std::mem::take(&mut self.state.borrow_mut().requests)
    }

    pub fn reply(&self, rid: u64, reply: Json) {
        self.send_json(&json!({"t": "ctl.reply", "rid": rid, "reply": reply}));
    }
}

/// The sign-in page the server named in `<meta name="celve-signin" content="/signin">`.
fn signin_page() -> Option<String> {
    let meta = web_sys::window()?.document()?.query_selector("meta[name=celve-signin]").ok()??;
    meta.get_attribute("content").filter(|c| c.starts_with('/'))
}

/// The pane's event for one `{kind, value}` from the server.
fn event(v: &Json) -> Option<SageEvent> {
    let value = v.get("value");
    Some(match v.get("kind").and_then(Json::as_str)? {
        "text" => SageEvent::Text(value?.as_str()?.to_string()),
        "step" => SageEvent::Step(value?.as_str()?.to_string()),
        "retract" => SageEvent::Retract(usize::try_from(value?.as_u64()?).ok()?),
        "done" => SageEvent::Done,
        "error" => SageEvent::Error(value?.as_str()?.to_string()),
        _ => return None,
    })
}

impl SageHost for WebSage {
    fn availability(&self) -> Result<(), String> {
        let st = self.state.borrow();
        if let Some(why) = &st.offline {
            return Err(why.clone());
        }
        if !st.open {
            return Err("Connecting to Sage…".into());
        }
        match &st.unavailable {
            Some(why) => Err(why.clone()),
            None => Ok(()),
        }
    }

    fn warm_up(&mut self) {
        self.state.borrow_mut().warm_wanted = true;
        self.send_json(&json!({"t": "sage.warm"}));
    }

    fn send(&mut self, text: &str, pictures: &[SagePicture], context: &str) {
        let pictures: Vec<Json> =
            pictures.iter().map(|p| json!({"name": p.name, "mime": p.mime, "data": gridcraft_engine::io::base64_encode(&p.bytes)})).collect();
        if !self.send_json(&json!({"t": "sage.send", "text": text, "context": context, "pictures": pictures})) {
            self.state.borrow_mut().events.push(SageEvent::Error("Sage isn't connected right now. Try again in a moment.".into()));
        }
    }

    fn stop(&mut self) {
        self.send_json(&json!({"t": "sage.stop"}));
    }

    fn reset(&mut self) {
        self.send_json(&json!({"t": "sage.reset"}));
    }

    fn poll(&mut self) -> Vec<SageEvent> {
        std::mem::take(&mut self.state.borrow_mut().events)
    }

    fn sign_in(&self) -> Option<String> {
        let st = self.state.borrow();
        if st.open { None } else { st.signin.clone() }
    }
}

/// Runs Sage's control requests on the app (through its control queue) and returns their
/// answers when ready (screenshots answer a few frames later).
#[derive(Default)]
pub struct Requests {
    waiting: Vec<(u64, Receiver<Json>)>,
}

impl Requests {
    pub fn submit(&mut self, tx: &std::sync::mpsc::Sender<ControlRequest>, rid: u64, method: String, params: Json) -> Option<Json> {
        let (req, rx) = ControlRequest::new(method, params);
        if tx.send(req).is_err() {
            return Some(json!({"ok": false, "error": "Celve isn't ready"}));
        }
        self.waiting.push((rid, rx));
        None
    }

    /// Answers that arrived since the last call.
    pub fn ready(&mut self) -> Vec<(u64, Json)> {
        let mut out = vec![];
        self.waiting.retain(|(rid, rx)| match rx.try_recv() {
            Ok(v) => {
                out.push((*rid, v));
                false
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => true,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                out.push((*rid, json!({"ok": false, "error": "request dropped"})));
                false
            }
        });
        out
    }

    pub fn busy(&self) -> bool {
        !self.waiting.is_empty()
    }
}
