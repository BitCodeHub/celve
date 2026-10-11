//! Sage: the built-in assistant pane. Sage answers questions, looks at the window, and takes
//! over the workbook (creates sheets, writes data and formulas, formats, charts) through the same
//! commands as every other agent.
//!
//! The UI only holds the conversation and draws it. The work happens in a [`SageHost`] the
//! platform shell provides (the desktop app runs the assistant process; the web build has none).
//! A host reports progress as [`SageEvent`]s, picked up once per frame in [`poll`].

use egui::text::LayoutJob;
use egui::{Color32, FontId, Rect, RichText, TextFormat, Ui, pos2, vec2};
use serde_json::{Value as Json, json};

use crate::SheetApp;
use crate::icons::Icon;
use crate::theme::{self, Tokens};

/// Pictures attached to one message at most.
pub const MAX_ATTACHMENTS: usize = 6;
/// Largest picture accepted (bytes); the assistant rejects bigger ones anyway.
pub const MAX_PICTURE_BYTES: usize = 5 * 1024 * 1024;
/// Messages kept on screen; older ones scroll away (the assistant keeps its own history).
const MAX_MESSAGES: usize = 200;
/// Longest message accepted from the input box (characters).
const MAX_INPUT_CHARS: usize = 20_000;

/// A picture attached to a message.
#[derive(Clone, Debug, PartialEq)]
pub struct SagePicture {
    pub name: String,
    /// `image/png`, `image/jpeg`, `image/gif` or `image/webp`.
    pub mime: String,
    pub bytes: Vec<u8>,
}

impl SagePicture {
    /// A picture from a file's bytes; `None` when it isn't a supported image or is too large.
    pub fn from_bytes(name: &str, bytes: Vec<u8>) -> Option<Self> {
        if bytes.is_empty() || bytes.len() > MAX_PICTURE_BYTES {
            return None;
        }
        let mime = picture_mime(&bytes)?;
        Some(Self { name: name.to_string(), mime: mime.to_string(), bytes })
    }
}

/// The image type from the file's first bytes.
pub fn picture_mime(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(&[0x89, b'P', b'N', b'G']) {
        Some("image/png")
    } else if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if b.starts_with(b"GIF8") {
        Some("image/gif")
    } else if b.len() >= 12 && b.starts_with(b"RIFF") && b.get(8..12) == Some(b"WEBP") {
        Some("image/webp")
    } else {
        None
    }
}

/// Whether a file name looks like a picture (dropped files are routed to Sage by name first).
pub fn is_picture_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [".png", ".jpg", ".jpeg", ".gif", ".webp"].iter().any(|e| lower.ends_with(e))
}

/// Progress from the assistant.
#[derive(Clone, Debug, PartialEq)]
pub enum SageEvent {
    /// More text of the reply being written.
    Text(String),
    /// Sage did something (one short line: "Wrote B2:D9", "Looked at the screen").
    Step(String),
    /// Take back the last this-many characters of reply text (narration that turned out to be
    /// a note before a tool call rather than part of the answer).
    Retract(usize),
    /// The reply is complete.
    Done,
    /// Something went wrong; the turn is over.
    Error(String),
}

/// What the platform shell provides to run the assistant.
pub trait SageHost {
    /// `Err` explains why Sage can't run here (shown in the pane instead of the input box).
    fn availability(&self) -> Result<(), String>;
    /// Get ready (start the assistant early so the first answer comes quickly). Idempotent.
    fn warm_up(&mut self);
    /// Send a message with optional pictures; `context` describes the workbook right now.
    fn send(&mut self, text: &str, pictures: &[SagePicture], context: &str);
    /// Stop the current reply.
    fn stop(&mut self);
    /// Forget the conversation.
    fn reset(&mut self);
    /// Events since the last call.
    fn poll(&mut self) -> Vec<SageEvent>;
    /// Where to sign in when Sage needs it (shown as a button while Sage is unavailable).
    fn sign_in(&self) -> Option<String> {
        None
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    User,
    Sage,
    Error,
}

#[derive(Clone, Debug)]
pub struct Message {
    pub role: Role,
    pub text: String,
    pub steps: Vec<String>,
    pub pictures: Vec<String>,
}

/// The conversation and pane state.
#[derive(Default)]
pub struct Chat {
    pub open: bool,
    pub messages: Vec<Message>,
    pub input: String,
    pub attachments: Vec<SagePicture>,
    /// A reply is in progress.
    pub busy: bool,
    /// Pictures picked with the browser's (asynchronous) picker, attached when they arrive.
    picture_inbox: Option<crate::Inbox>,
    /// Where an agent is working right now (drawn on the grid so people can watch).
    pub focus: Option<Focus>,
    scroll_to_end: bool,
    focus_input: bool,
}

impl Chat {
    fn push(&mut self, m: Message) {
        self.messages.push(m);
        if self.messages.len() > MAX_MESSAGES {
            let extra = self.messages.len() - MAX_MESSAGES;
            self.messages.drain(..extra);
        }
        self.scroll_to_end = true;
    }

    /// The reply being written (created on its first event).
    fn reply(&mut self) -> Option<&mut Message> {
        if self.messages.last().is_none_or(|m| m.role != Role::Sage) {
            self.push(Message { role: Role::Sage, text: String::new(), steps: vec![], pictures: vec![] });
        }
        self.messages.last_mut()
    }

    /// Applies one event from the host.
    pub fn apply(&mut self, e: SageEvent) {
        match e {
            SageEvent::Text(t) => {
                if let Some(r) = self.reply() {
                    r.text.push_str(&t);
                }
                self.scroll_to_end = true;
            }
            SageEvent::Step(s) => {
                if let Some(r) = self.reply() {
                    r.steps.push(s);
                }
                self.scroll_to_end = true;
            }
            SageEvent::Retract(n) => {
                if let Some(r) = self.messages.last_mut().filter(|m| m.role == Role::Sage) {
                    let keep = r.text.chars().count().saturating_sub(n);
                    r.text = r.text.chars().take(keep).collect();
                }
            }
            SageEvent::Done => self.busy = false,
            SageEvent::Error(m) => {
                self.busy = false;
                self.push(Message { role: Role::Error, text: m, steps: vec![], pictures: vec![] });
            }
        }
    }

    /// Adds a picture to the next message; `Err` says why it was refused.
    pub fn attach(&mut self, p: SagePicture) -> Result<(), String> {
        if self.attachments.len() >= MAX_ATTACHMENTS {
            return Err(format!("Up to {MAX_ATTACHMENTS} pictures per message."));
        }
        self.attachments.push(p);
        Ok(())
    }
}

/// The range an agent just read or changed, shown on the grid for a moment.
#[derive(Clone, Debug, PartialEq)]
pub struct Focus {
    /// Sheet name; `None` for the active sheet.
    pub sheet: Option<String>,
    pub range: gridcraft_engine::core::RangeRef,
    /// It changed cells (solid outline) rather than reading them (dashed).
    pub write: bool,
    pub at: f64,
}

/// How long the outline stays after the last action (ms).
pub const FOCUS_MS: f64 = 2500.0;

/// Notes where a control request (an agent's command) is about to act, so the grid can show it.
pub fn note_request(app: &mut SheetApp, method: &str, p: &Json) {
    let (command, params) = match method {
        "engine.execute" | "command" | "ui.menu.invoke" => {
            (p.get("command").or_else(|| p.get("id")).and_then(Json::as_str).unwrap_or(""), p.get("params").unwrap_or(&Json::Null))
        }
        other => (other, p),
    };
    if let Some(f) = focus_of(command, params) {
        app.sage.focus = Some(Focus { at: crate::now_ms(), ..f });
    }
}

/// The range a command touches, from its `range`/`cell` parameter (and the size of `values`).
pub fn focus_of(command: &str, params: &Json) -> Option<Focus> {
    let write = !matches!(command, "sheet.read" | "cell.get" | "formulas.evaluate" | "selection.set");
    if command == "selection.set" || command.starts_with("ui.") || command.starts_with("file.") {
        return None;
    }
    let text = params.get("range").or_else(|| params.get("cell")).and_then(Json::as_str)?;
    let (sheet, body) = match text.rsplit_once('!') {
        Some((s, b)) => (Some(s.trim_matches('\'').replace("''", "'")), b),
        None => (None, text),
    };
    let first = body.split(',').next().unwrap_or(body);
    let mut range = gridcraft_engine::core::RangeRef::parse(first.trim())?;
    // `range.setValues {range: "A1", values: [[…]]}` fills a block from its top-left cell.
    if range.is_single()
        && let Some(rows) = params.get("values").and_then(Json::as_array)
    {
        let h = rows.len().clamp(1, 1_048_576) as i64;
        let w = rows.iter().filter_map(Json::as_array).map(Vec::len).max().unwrap_or(1).clamp(1, 16_384) as i64;
        range = gridcraft_engine::core::RangeRef::new(range.start, range.start.offset_clamped(h - 1, w - 1));
    }
    Some(Focus { sheet, range, write, at: 0.0 })
}

/// A short description of the workbook for the assistant (so it rarely needs to ask).
pub fn context(app: &SheetApp) -> String {
    let Some(d) = app.session.active() else { return "No workbook is open.".into() };
    let sheets: Vec<String> =
        d.wb.sheets
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let used = s.used_range().map(|r| r.a1()).unwrap_or_else(|| "empty".into());
                let active = if i == d.wb.active_sheet { ", active" } else { "" };
                format!("\"{}\" ({used}{active})", s.name)
            })
            .take(50)
            .collect();
    let path = d.path.as_deref().unwrap_or("not saved yet");
    format!(
        "Workbook \"{}\" ({path}). Sheets: {}. Selection: {} (active cell {}).",
        d.display_title(),
        sheets.join(", "),
        d.selection.current().a1(),
        d.selection.active.a1()
    )
}

/// UI commands: `view.sage {on?}`, `sage.ask {text, pictures?: [path]}`, `sage.draft {text}`, `sage.stop`, `sage.reset`,
/// `sage.state` (the conversation, for agents and tests).
pub fn run_command(app: &mut SheetApp, id: &str, p: &Json) -> Option<Result<Json, String>> {
    let r = match id {
        "view.sage" => {
            app.sage.open = p.get("on").and_then(Json::as_bool).unwrap_or(!app.sage.open);
            if app.sage.open {
                app.sage.focus_input = true;
                if let Some(h) = app.services.sage.as_mut() {
                    h.warm_up();
                }
            }
            Ok(json!({"on": app.sage.open}))
        }
        "sage.ask" => {
            let text = p.get("text").and_then(Json::as_str).unwrap_or("").trim().to_string();
            if text.is_empty() {
                return Some(Err("`text` is required".into()));
            }
            // `pictures`: file paths to attach (desktop).
            for path in p.get("pictures").and_then(Json::as_array).map(Vec::as_slice).unwrap_or_default().iter().filter_map(Json::as_str) {
                let name = std::path::Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or("picture").to_string();
                let picture = std::fs::read(path).ok().and_then(|b| SagePicture::from_bytes(&name, b));
                match picture {
                    Some(pic) => {
                        if let Err(e) = app.sage.attach(pic) {
                            return Some(Err(e));
                        }
                    }
                    None => return Some(Err(format!("{name} isn't a picture Sage can read (PNG, JPEG, GIF or WebP under 5 MB)"))),
                }
            }
            app.sage.open = true;
            app.sage.input = text;
            send(app).map(|()| json!({"sent": true}))
        }
        "sage.draft" => {
            // Fills in the message box without sending it (links can't make Sage act).
            app.sage.input = p.get("text").and_then(Json::as_str).unwrap_or("").chars().take(MAX_INPUT_CHARS).collect();
            app.sage.open = true;
            app.sage.focus_input = true;
            Ok(Json::Null)
        }
        "sage.stop" => {
            stop(app);
            Ok(Json::Null)
        }
        "sage.reset" => {
            reset(app);
            Ok(Json::Null)
        }
        "sage.state" => Ok(json!({
            "open": app.sage.open,
            "busy": app.sage.busy,
            "messages": app.sage.messages.iter().map(|m| json!({
                "role": match m.role { Role::User => "user", Role::Sage => "sage", Role::Error => "error" },
                "text": m.text,
                "steps": m.steps,
            })).collect::<Vec<_>>(),
        })),
        _ => return None,
    };
    Some(r)
}

/// Sends the input box (and attachments) to the assistant.
pub fn send(app: &mut SheetApp) -> Result<(), String> {
    let text: String = app.sage.input.trim().chars().take(MAX_INPUT_CHARS).collect();
    if text.is_empty() && app.sage.attachments.is_empty() {
        return Err("Type a message first.".into());
    }
    if app.sage.busy {
        return Err("Sage is still working on the last message. Stop it first, or wait.".into());
    }
    let ctx = context(app);
    let Some(host) = app.services.sage.as_mut() else { return Err("Sage isn't available in this build.".into()) };
    host.availability()?;
    let pictures = std::mem::take(&mut app.sage.attachments);
    let text = if text.is_empty() { "What do you see in this picture?".to_string() } else { text };
    host.send(&text, &pictures, &ctx);
    app.sage.busy = true;
    app.sage.input.clear();
    app.sage.push(Message { role: Role::User, text, steps: vec![], pictures: pictures.iter().map(|p| p.name.clone()).collect() });
    Ok(())
}

fn stop(app: &mut SheetApp) {
    if let Some(h) = app.services.sage.as_mut() {
        h.stop();
    }
    if app.sage.busy {
        app.sage.busy = false;
        if let Some(r) = app.sage.reply() {
            r.steps.push("Stopped.".into());
        }
    }
}

fn reset(app: &mut SheetApp) {
    if let Some(h) = app.services.sage.as_mut() {
        h.reset();
    }
    app.sage.messages.clear();
    app.sage.attachments.clear();
    app.sage.busy = false;
}

/// Picks up the host's events (call every frame).
pub fn poll(app: &mut SheetApp, ctx: &egui::Context) {
    let picked: Vec<(String, Vec<u8>)> = app
        .sage
        .picture_inbox
        .as_ref()
        .map(|i| std::mem::take(&mut *i.lock().unwrap_or_else(std::sync::PoisonError::into_inner)))
        .unwrap_or_default();
    for (name, bytes) in picked {
        let r =
            SagePicture::from_bytes(&name, bytes).ok_or_else(|| format!("{name} isn't a picture Sage can read.")).and_then(|p| app.sage.attach(p));
        if let Err(e) = r {
            app.toast = Some((e, crate::now_ms()));
        }
    }
    let Some(h) = app.services.sage.as_mut() else { return };
    let events = h.poll();
    let changed = !events.is_empty();
    for e in events {
        app.sage.apply(e);
    }
    if changed {
        // Sage's commands changed the workbook through the control channel; keep the grid fresh.
        app.after_engine();
        ctx.request_repaint();
    }
    if app.sage.busy {
        ctx.request_repaint_after(std::time::Duration::from_millis(120));
    }
    if app.sage.focus.as_ref().is_some_and(|f| crate::now_ms() - f.at < FOCUS_MS) {
        ctx.request_repaint_after(std::time::Duration::from_millis(33));
    }
}

/// The pane, docked on the right.
pub fn show(app: &mut SheetApp, ui: &mut Ui) {
    if !app.sage.open {
        return;
    }
    let t = Tokens::get(ui.ctx());
    egui::Panel::right("sage_pane")
        .default_size(380.0)
        .min_size(300.0)
        .resizable(true)
        .frame(
            egui::Frame::NONE
                .fill(t.window)
                .inner_margin(egui::Margin { left: 12, right: 12, top: 10, bottom: 10 })
                .stroke(egui::Stroke::new(1.0, t.ribbon_border)),
        )
        .show(ui, |ui| {
            let available = app.services.sage.as_ref().map(|h| h.availability()).unwrap_or_else(|| Err("Sage runs in the Celve desktop app.".into()));
            header(app, ui, &t, &available);
            ui.add_space(6.0);
            let signs_in = available.is_err() && app.services.sage.as_ref().is_some_and(|h| h.sign_in().is_some());
            // Room under the list for the composer, or for the card saying why Sage can't run.
            let composer_h = match &available {
                Ok(()) => 132.0 + if app.sage.attachments.is_empty() { 0.0 } else { 30.0 },
                Err(_) if signs_in => 112.0,
                Err(_) => 80.0,
            };
            let list_h = (ui.available_height() - composer_h).max(80.0);
            egui::ScrollArea::vertical().max_height(list_h).auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
                ui.set_width(ui.available_width());
                if app.sage.messages.is_empty() {
                    welcome(app, ui, &t);
                }
                let last = app.sage.messages.len().saturating_sub(1);
                let busy = app.sage.busy;
                for (i, m) in app.sage.messages.iter().enumerate() {
                    message(ui, m, &t, busy && i == last);
                }
                if busy {
                    working(ui, &t, app.sage.messages.last().and_then(|m| m.steps.last()).map(String::as_str));
                }
                if std::mem::take(&mut app.sage.scroll_to_end) {
                    ui.scroll_to_cursor(Some(egui::Align::BOTTOM));
                }
            });
            match available {
                Ok(()) => composer(app, ui, &t),
                Err(why) => {
                    let sign_in = app.services.sage.as_ref().and_then(|h| h.sign_in());
                    ui.add_space(8.0);
                    card(ui, &t, |ui| {
                        ui.label(RichText::new(why).color(t.text_dim));
                        if let Some(url) = sign_in {
                            ui.add_space(8.0);
                            if sign_in_button(ui, &t).clicked() {
                                ui.ctx().open_url(egui::OpenUrl::same_tab(url));
                            }
                        }
                    });
                }
            }
        });
}

/// The Celve mark and wordmark for the title bar.
pub fn brand(ui: &mut Ui) {
    let t = Tokens::get(ui.ctx());
    if let Some(tex) = logo(ui.ctx()) {
        ui.add(egui::Image::new((tex.id(), vec2(20.0, 20.0))));
    }
    ui.label(RichText::new("Celve").font(theme::ui_bold(14.0)).color(if t.dark { t.text } else { Color32::from_rgb(0x0B, 0x5A, 0x2F) }));
}

/// The app icon as a texture (decoded once).
fn logo(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    let id = egui::Id::new("celve_logo_texture");
    if let Some(t) = ctx.data(|d| d.get_temp::<egui::TextureHandle>(id)) {
        return Some(t);
    }
    let bytes = include_bytes!("../../../assets/app-icon/hicolor/64x64/apps/cloud.agentlumi.celve.png");
    let img = image::load_from_memory(bytes).ok()?.to_rgba8();
    let size = [img.width() as usize, img.height() as usize];
    let tex = ctx.load_texture("celve_logo", egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw()), egui::TextureOptions::LINEAR);
    ctx.data_mut(|d| d.insert_temp(id, tex.clone()));
    Some(tex)
}

/// The title bar's "Ask Sage" pill.
pub fn ask_button(ui: &mut Ui, open: bool, busy: bool) -> egui::Response {
    let t = Tokens::get(ui.ctx());
    let galley = ui.painter().layout_no_wrap("Ask Sage".into(), theme::ui_bold(12.5), Color32::WHITE);
    let (r, resp) = ui.allocate_exact_size(vec2(galley.size().x + 40.0, 26.0), egui::Sense::click());
    let p = ui.painter();
    let base = if busy {
        t.sage
    } else if open {
        t.accent_dark
    } else {
        t.accent
    };
    let fill = if resp.hovered() { t.accent_dark } else { base };
    p.rect_filled(r, 13.0, fill);
    crate::icons::paint(p, Rect::from_min_size(r.min + vec2(9.0, 5.0), vec2(16.0, 16.0)), Icon::Sage, Color32::WHITE);
    p.galley(pos2(r.left() + 29.0, r.center().y - galley.size().y / 2.0), galley, Color32::WHITE);
    resp.on_hover_text("Sage: ask anything, or let Sage build it (⌘⇧I)").on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// The pill that opens the sign-in page.
fn sign_in_button(ui: &mut Ui, t: &Tokens) -> egui::Response {
    let galley = ui.painter().layout_no_wrap("Sign in".into(), theme::ui_bold(13.0), Color32::WHITE);
    let (r, resp) = ui.allocate_exact_size(vec2(galley.size().x + 36.0, 30.0), egui::Sense::click());
    ui.painter().rect_filled(r, 15.0, if resp.hovered() { t.accent_dark } else { t.accent });
    ui.painter().galley(r.center() - galley.size() / 2.0, galley, Color32::WHITE);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// The status bar's live line while Sage works ("Sage is editing B2:D9").
pub fn status(app: &SheetApp, ui: &mut Ui) {
    if !app.sage.busy {
        return;
    }
    let t = Tokens::get(ui.ctx());
    ui.add_space(14.0);
    let (r, _) = ui.allocate_exact_size(vec2(14.0, 14.0), egui::Sense::hover());
    crate::icons::paint(ui.painter(), r, Icon::Sage, t.text);
    let what = match &app.sage.focus {
        Some(f) if crate::now_ms() - f.at < FOCUS_MS => {
            let place = match &f.sheet {
                Some(s) => format!("{s}!{}", f.range.a1()),
                None => f.range.a1(),
            };
            format!("Sage is {} {place}", if f.write { "editing" } else { "reading" })
        }
        _ => "Sage is working…".into(),
    };
    ui.label(RichText::new(what).font(theme::ui_font(12.5)).color(t.sage));
}

/// Sage's avatar: the sparkle on a green disc.
pub fn avatar(ui: &mut Ui, size: f32) -> egui::Response {
    let (r, resp) = ui.allocate_exact_size(vec2(size, size), egui::Sense::hover());
    let p = ui.painter();
    p.circle_filled(r.center(), size / 2.0, Color32::from_rgb(0x0B, 0x5A, 0x2F));
    p.circle_filled(r.center() - vec2(0.0, size * 0.06), size / 2.0 - size * 0.08, Color32::from_rgb(0x12, 0x8A, 0x4B));
    crate::icons::paint(p, r.shrink(size * 0.2), Icon::Sage, Color32::WHITE);
    resp
}

fn card<R>(ui: &mut Ui, t: &Tokens, add: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::NONE
        .fill(t.ribbon)
        .stroke(egui::Stroke::new(1.0, t.ribbon_border))
        .corner_radius(12.0)
        .inner_margin(egui::Margin::symmetric(12, 10))
        .shadow(egui::Shadow { offset: [0, 1], blur: 6, spread: 0, color: t.shadow.gamma_multiply(0.35) })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

fn header(app: &mut SheetApp, ui: &mut Ui, t: &Tokens, available: &Result<(), String>) {
    ui.horizontal(|ui| {
        avatar(ui, 32.0);
        ui.add_space(4.0);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            ui.label(RichText::new("Sage").font(theme::ui_bold(16.0)).color(t.text));
            let (dot, status) = match available {
                Ok(()) if app.sage.busy => (t.sage, "Working on your sheet"),
                Ok(()) => (Color32::from_rgb(0x2E, 0xB6, 0x6B), "Ready · sees this workbook"),
                Err(why) if why.starts_with("Connecting") => (Color32::from_rgb(0xE8, 0xA3, 0x17), "Connecting…"),
                Err(_) => (t.text_disabled, "Offline"),
            };
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(vec2(8.0, 12.0), egui::Sense::hover());
                let pulse = if app.sage.busy { 0.6 + 0.4 * ((crate::now_ms() / 300.0).sin() as f32).abs() } else { 1.0 };
                ui.painter().circle_filled(r.center(), 3.5, dot.gamma_multiply(pulse));
                ui.label(RichText::new(status).size(11.5).color(t.text_dim));
            });
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if crate::widgets::icon_button(ui, Icon::Close, t.text_dim, "Close Sage", vec2(24.0, 24.0)).clicked() {
                app.sage.open = false;
            }
            if !app.sage.messages.is_empty() {
                let b = egui::Button::new(RichText::new("New chat").size(12.0).color(t.text))
                    .fill(t.ribbon)
                    .stroke(egui::Stroke::new(1.0, t.ribbon_border))
                    .corner_radius(8.0);
                if ui.add(b).on_hover_text("Start a new conversation").clicked() {
                    reset(app);
                }
            }
        });
    });
}

const SUGGESTIONS: [(Icon, &str, &str); 4] = [
    (Icon::Chart, "Build a budget", "Build a monthly budget with income, expenses and a chart"),
    (Icon::Search, "Explain this sheet", "Explain what's on this sheet"),
    (Icon::Brush, "Tidy my table", "Clean up and format the selected table"),
    (Icon::Sheet, "Summarise", "Add a summary sheet with totals by category"),
];

fn welcome(app: &mut SheetApp, ui: &mut Ui, t: &Tokens) {
    ui.add_space(18.0);
    ui.vertical_centered(|ui| {
        avatar(ui, 56.0);
        ui.add_space(8.0);
        ui.label(RichText::new("Hi, I'm Sage").font(theme::ui_bold(20.0)).color(t.text));
        ui.add_space(2.0);
        ui.label(
            RichText::new(
                "I see everything in this workbook and can build it with you. Ask a question, or tell me what to make, and watch it happen.",
            )
            .size(12.5)
            .color(t.text_dim),
        );
    });
    ui.add_space(14.0);
    let w = (ui.available_width() - 8.0) / 2.0;
    let mut chosen = None;
    egui::Grid::new("sage_suggestions").num_columns(2).spacing(vec2(8.0, 8.0)).show(ui, |ui| {
        for (i, (icon, title, prompt)) in SUGGESTIONS.iter().enumerate() {
            let (r, resp) = ui.allocate_exact_size(vec2(w, 64.0), egui::Sense::click());
            let hovered = resp.hovered();
            let p = ui.painter();
            p.rect(
                r,
                12.0,
                if hovered { t.accent_soft } else { t.ribbon },
                egui::Stroke::new(1.0, if hovered { t.accent } else { t.ribbon_border }),
                egui::StrokeKind::Inside,
            );
            crate::icons::paint(p, Rect::from_min_size(r.min + vec2(10.0, 10.0), vec2(18.0, 18.0)), *icon, t.accent);
            p.text(r.min + vec2(10.0, 36.0), egui::Align2::LEFT_TOP, *title, theme::ui_bold(12.5), t.text);
            if resp.on_hover_text(*prompt).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                chosen = Some(*prompt);
            }
            if i % 2 == 1 {
                ui.end_row();
            }
        }
    });
    if let Some(prompt) = chosen {
        app.sage.input = prompt.to_string();
        if let Err(e) = send(app) {
            app.sage.apply(SageEvent::Error(e));
        }
    }
    ui.add_space(10.0);
    ui.vertical_centered(|ui| {
        ui.label(RichText::new("Tip: drop a photo of a receipt or a table here and ask for a sheet.").size(11.5).color(t.text_disabled));
    });
}

fn message(ui: &mut Ui, m: &Message, t: &Tokens, live: bool) {
    ui.add_space(8.0);
    match m.role {
        Role::User => {
            // Right-aligned bubble.
            let max = ui.available_width() * 0.85;
            // Top-down, right-aligned: the text wraps inside the bubble instead of widening the pane.
            ui.with_layout(egui::Layout::top_down(egui::Align::Max), |ui| {
                egui::Frame::NONE
                    .fill(t.accent)
                    .corner_radius(egui::CornerRadius { nw: 14, ne: 14, sw: 14, se: 4 })
                    .inner_margin(egui::Margin::symmetric(12, 8))
                    .show(ui, |ui| {
                        ui.set_max_width(max - 24.0);
                        ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                            for p in &m.pictures {
                                ui.label(RichText::new(format!("🖼 {p}")).size(11.5).color(Color32::WHITE.gamma_multiply(0.85)));
                            }
                            markdown(ui, &m.text, t, Color32::WHITE);
                        });
                    });
            });
        }
        Role::Sage => {
            ui.horizontal_top(|ui| {
                avatar(ui, 24.0);
                ui.add_space(2.0);
                ui.vertical(|ui| {
                    card(ui, t, |ui| {
                        steps(ui, m, t, live);
                        if !m.text.is_empty() {
                            if !m.steps.is_empty() {
                                ui.add_space(4.0);
                            }
                            markdown(ui, &m.text, t, t.text);
                        }
                    });
                });
            });
        }
        Role::Error => {
            egui::Frame::NONE
                .fill(t.danger.gamma_multiply(0.10))
                .stroke(egui::Stroke::new(1.0, t.danger.gamma_multiply(0.6)))
                .corner_radius(12.0)
                .inner_margin(egui::Margin::symmetric(12, 8))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new(&m.text).color(t.danger));
                });
        }
    }
}

/// What Sage did: the latest few while it works, folded into "N actions" afterwards.
fn steps(ui: &mut Ui, m: &Message, t: &Tokens, live: bool) {
    if m.steps.is_empty() {
        return;
    }
    let line = |ui: &mut Ui, s: &str| {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let (r, _) = ui.allocate_exact_size(vec2(12.0, 14.0), egui::Sense::hover());
            ui.painter().circle_filled(r.center(), 5.0, t.accent_soft);
            crate::icons::paint(ui.painter(), r.shrink(2.5), Icon::Check, t.accent);
            ui.label(RichText::new(s).size(11.5).color(t.text_dim));
        });
    };
    if live {
        let skip = m.steps.len().saturating_sub(4);
        if skip > 0 {
            ui.label(RichText::new(format!("+{skip} earlier")).size(11.0).color(t.text_disabled));
        }
        for s in m.steps.iter().skip(skip) {
            line(ui, s);
        }
    } else {
        let n = m.steps.len();
        let title = RichText::new(format!("{n} action{}", if n == 1 { "" } else { "s" })).size(11.5).color(t.text_dim);
        egui::CollapsingHeader::new(title).id_salt(m as *const Message as usize).default_open(false).show(ui, |ui| {
            for s in &m.steps {
                line(ui, s);
            }
        });
    }
}

/// "Sage is working" with animated dots and the latest action.
fn working(ui: &mut Ui, t: &Tokens, latest: Option<&str>) {
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.add_space(30.0);
        let (r, _) = ui.allocate_exact_size(vec2(28.0, 14.0), egui::Sense::hover());
        let phase = crate::now_ms() / 180.0;
        for i in 0..3 {
            let a = (((phase - i as f64 * 0.9).sin() + 1.0) / 2.0) as f32;
            ui.painter().circle_filled(pos2(r.left() + 5.0 + i as f32 * 9.0, r.center().y - a * 2.5), 3.0, t.sage.gamma_multiply(0.45 + 0.55 * a));
        }
        let text = latest.map(|s| format!("Sage is working · {s}")).unwrap_or_else(|| "Sage is thinking…".into());
        ui.label(RichText::new(text).size(11.5).color(t.text_dim));
    });
}

fn composer(app: &mut SheetApp, ui: &mut Ui, t: &Tokens) {
    ui.add_space(8.0);
    if !app.sage.attachments.is_empty() {
        let mut remove = None;
        ui.horizontal_wrapped(|ui| {
            for (i, p) in app.sage.attachments.iter().enumerate() {
                let b = egui::Button::new(RichText::new(format!("🖼 {}  ✕", short(&p.name, 18))).size(11.5)).fill(t.accent_soft).corner_radius(10.0);
                if ui.add(b).on_hover_text("Remove").clicked() {
                    remove = Some(i);
                }
            }
        });
        if let Some(i) = remove.filter(|&i| i < app.sage.attachments.len()) {
            app.sage.attachments.remove(i);
        }
        ui.add_space(4.0);
    }
    let mut do_send = false;
    let focused = ui.memory(|m| m.focused()).is_some_and(|id| id == egui::Id::new("sage_input"));
    egui::Frame::NONE
        .fill(t.input_bg)
        .stroke(egui::Stroke::new(if focused { 1.5 } else { 1.0 }, if focused { t.accent } else { t.input_border }))
        .corner_radius(14.0)
        .inner_margin(egui::Margin { left: 12, right: 8, top: 10, bottom: 8 })
        .shadow(egui::Shadow { offset: [0, 2], blur: 10, spread: 0, color: t.shadow.gamma_multiply(if focused { 0.5 } else { 0.25 }) })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let edit = egui::TextEdit::multiline(&mut app.sage.input)
                .id(egui::Id::new("sage_input"))
                .hint_text("Ask Sage anything, or tell it what to build…")
                .desired_rows(2)
                .frame(egui::Frame::NONE)
                .desired_width(f32::INFINITY)
                .font(FontId::new(13.5, egui::FontFamily::Proportional));
            let resp = ui.add(edit);
            if std::mem::take(&mut app.sage.focus_input) {
                resp.request_focus();
            }
            // Enter sends, Shift+Enter makes a new line.
            let enter = resp.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift);
            if enter {
                // The multiline edit already inserted the newline; drop it.
                while app.sage.input.ends_with('\n') {
                    app.sage.input.pop();
                }
                do_send = true;
            }
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if crate::widgets::icon_button(ui, Icon::Picture, t.text_dim, "Attach a picture (or drop one here)", vec2(28.0, 26.0)).clicked() {
                    attach_from_picker(app);
                }
                let look = egui::Button::new(RichText::new("👁 Look").size(12.0).color(t.text_dim))
                    .fill(Color32::TRANSPARENT)
                    .stroke(egui::Stroke::new(1.0, t.ribbon_border))
                    .corner_radius(10.0);
                if ui.add(look).on_hover_text("Ask Sage to look at the window").clicked() && !app.sage.busy {
                    if app.sage.input.trim().is_empty() {
                        app.sage.input = "Look at my screen and tell me what you see.".into();
                    }
                    do_send = true;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let (r, resp) = ui.allocate_exact_size(vec2(32.0, 32.0), egui::Sense::click());
                    let busy = app.sage.busy;
                    let can = busy || !app.sage.input.trim().is_empty() || !app.sage.attachments.is_empty();
                    let fill = if !can {
                        t.text_disabled
                    } else if resp.hovered() {
                        t.accent_dark
                    } else if busy {
                        t.sage
                    } else {
                        t.accent
                    };
                    let p = ui.painter();
                    p.circle_filled(r.center(), 16.0, fill);
                    let c = r.center();
                    if busy {
                        p.rect_filled(Rect::from_center_size(c, vec2(10.0, 10.0)), 2.0, Color32::WHITE);
                    } else {
                        // An up arrow.
                        let s = egui::Stroke::new(2.2, Color32::WHITE);
                        p.line_segment([c + vec2(0.0, 7.0), c + vec2(0.0, -7.0)], s);
                        p.line_segment([c + vec2(-5.5, -1.5), c + vec2(0.0, -7.0)], s);
                        p.line_segment([c + vec2(5.5, -1.5), c + vec2(0.0, -7.0)], s);
                    }
                    let resp = resp.on_hover_text(if busy { "Stop" } else { "Send (Enter)" }).on_hover_cursor(egui::CursorIcon::PointingHand);
                    if resp.clicked() {
                        if busy {
                            stop(app);
                        } else if can {
                            do_send = true;
                        }
                    }
                });
            });
        });
    ui.add_space(4.0);
    ui.vertical_centered(|ui| {
        ui.label(RichText::new("Sage can make mistakes. Check important numbers.").size(10.5).color(t.text_disabled));
    });
    if do_send && !app.sage.busy {
        if let Err(e) = send(app) {
            app.sage.apply(SageEvent::Error(e));
        }
        app.sage.focus_input = true;
    }
}

fn attach_from_picker(app: &mut SheetApp) {
    if app.services.pick_picture.is_none()
        && let Some(pick) = &app.services.pick_picture_async
    {
        let inbox = crate::Inbox::default();
        app.sage.picture_inbox = Some(inbox.clone());
        pick(inbox);
        return;
    }
    let Some(pick) = &app.services.pick_picture else { return };
    let Some(path) = pick() else { return };
    let name = std::path::Path::new(&path).file_name().and_then(|n| n.to_str()).unwrap_or("picture").to_string();
    let result = match std::fs::read(&path) {
        Ok(bytes) => SagePicture::from_bytes(&name, bytes).ok_or_else(|| "That file isn't a PNG, JPEG, GIF or WebP picture under 5 MB.".to_string()),
        Err(e) => Err(format!("Couldn't read {name}: {e}")),
    }
    .and_then(|p| app.sage.attach(p));
    if let Err(e) = result {
        app.toast = Some((e, crate::now_ms()));
    }
}

fn short(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { format!("{}…", s.chars().take(n).collect::<String>()) }
}

/// Just enough Markdown for chat replies: headings, bullets, numbered lists, code blocks,
/// **bold** and `code` spans.
fn markdown(ui: &mut Ui, text: &str, t: &Tokens, color: Color32) {
    let mut in_code = false;
    let mut code = String::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while let Some(&line) = lines.get(i) {
        i += 1;
        // A Markdown table: its rows, aligned in a monospace block (separator row dropped).
        if !in_code && line.trim_start().starts_with('|') {
            let mut rows = vec![table_cells(line)];
            while let Some(next) = lines.get(i).filter(|l| l.trim_start().starts_with('|')) {
                i += 1;
                let cells = table_cells(next);
                if !cells.iter().all(|c| !c.is_empty() && c.chars().all(|ch| matches!(ch, '-' | ':'))) {
                    rows.push(cells);
                }
            }
            table(ui, &rows, t, color);
            continue;
        }
        if line.trim_start().starts_with("```") {
            if in_code {
                egui::Frame::NONE.fill(t.input_bg).corner_radius(4.0).inner_margin(6.0).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new(code.trim_end()).monospace().size(12.0).color(color));
                });
                code.clear();
            }
            in_code = !in_code;
            continue;
        }
        if in_code {
            code.push_str(line);
            code.push('\n');
            continue;
        }
        let trimmed = line.trim_start();
        if let Some(h) = trimmed.strip_prefix("### ").or_else(|| trimmed.strip_prefix("## ")).or_else(|| trimmed.strip_prefix("# ")) {
            ui.label(RichText::new(h).font(theme::ui_bold(14.0)).color(color));
        } else if let Some(b) = trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* ")) {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                ui.label(RichText::new("•").size(15.0).color(color.gamma_multiply(0.7)));
                ui.label(inline(b, t, color));
            });
        } else if trimmed.is_empty() {
            ui.add_space(4.0);
        } else {
            ui.label(inline(line, t, color));
        }
    }
    if in_code && !code.is_empty() {
        ui.label(RichText::new(code.trim_end()).monospace().size(12.0).color(color));
    }
}

/// The cells of one Markdown table row, trimmed, without inline markers.
fn table_cells(line: &str) -> Vec<String> {
    let inner = line.trim().trim_start_matches('|').trim_end_matches('|');
    inner.split('|').map(|c| c.trim().replace("**", "").replace('`', "")).collect()
}

fn table(ui: &mut Ui, rows: &[Vec<String>], t: &Tokens, color: Color32) {
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0).min(12);
    let widths: Vec<usize> = (0..cols).map(|c| rows.iter().filter_map(|r| r.get(c)).map(|s| s.chars().count().min(40)).max().unwrap_or(0)).collect();
    let mut out = String::new();
    for (ri, r) in rows.iter().enumerate().take(200) {
        let cells: Vec<String> = (0..cols)
            .map(|c| {
                let s: String = r.get(c).map(String::as_str).unwrap_or("").chars().take(40).collect();
                let pad = widths.get(c).copied().unwrap_or(0).saturating_sub(s.chars().count());
                format!("{s}{}", " ".repeat(pad))
            })
            .collect();
        out.push_str(cells.join("  ").trim_end());
        out.push('\n');
        if ri == 0 && rows.len() > 1 {
            out.push_str(&widths.iter().map(|w| "─".repeat(*w)).collect::<Vec<_>>().join("  "));
            out.push('\n');
        }
    }
    egui::Frame::NONE.fill(t.input_bg).corner_radius(4.0).inner_margin(6.0).show(ui, |ui| {
        ui.set_width(ui.available_width());
        egui::ScrollArea::horizontal().id_salt(ui.next_auto_id()).show(ui, |ui| {
            ui.add(egui::Label::new(RichText::new(out.trim_end()).monospace().size(12.0).color(color)).extend());
        });
    });
}

fn inline(s: &str, t: &Tokens, color: Color32) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;
    let normal = TextFormat { font_id: FontId::new(13.0, egui::FontFamily::Proportional), color, ..Default::default() };
    let bold = TextFormat { font_id: theme::ui_bold(13.0), color, ..Default::default() };
    let mono = TextFormat {
        font_id: FontId::new(12.0, egui::FontFamily::Monospace),
        color,
        background: if color == t.text { t.input_bg } else { Color32::from_white_alpha(40) },
        ..Default::default()
    };
    let mut rest = s;
    while !rest.is_empty() {
        let next_bold = rest.find("**");
        let next_code = rest.find('`');
        match (next_bold, next_code) {
            (Some(b), c) if c.is_none_or(|c| b < c) => {
                let (before, after) = rest.split_at(b);
                job.append(before, 0.0, normal.clone());
                let after = after.get(2..).unwrap_or("");
                match after.find("**") {
                    Some(end) => {
                        job.append(after.get(..end).unwrap_or(""), 0.0, bold.clone());
                        rest = after.get(end + 2..).unwrap_or("");
                    }
                    None => {
                        job.append(after, 0.0, normal.clone());
                        rest = "";
                    }
                }
            }
            (_, Some(c)) => {
                let (before, after) = rest.split_at(c);
                job.append(before, 0.0, normal.clone());
                let after = after.get(1..).unwrap_or("");
                match after.find('`') {
                    Some(end) => {
                        job.append(after.get(..end).unwrap_or(""), 0.0, mono.clone());
                        rest = after.get(end + 1..).unwrap_or("");
                    }
                    None => {
                        job.append(after, 0.0, normal.clone());
                        rest = "";
                    }
                }
            }
            _ => {
                job.append(rest, 0.0, normal.clone());
                rest = "";
            }
        }
    }
    job
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn events_build_one_reply() {
        let mut c = Chat { busy: true, ..Default::default() };
        c.apply(SageEvent::Step("Looked at the screen".into()));
        c.apply(SageEvent::Text("Hello".into()));
        c.apply(SageEvent::Text(" there".into()));
        c.apply(SageEvent::Text(" — note to self".into()));
        c.apply(SageEvent::Retract(" — note to self".chars().count()));
        c.apply(SageEvent::Retract(10_000));
        c.apply(SageEvent::Text("Hello there".into()));
        c.apply(SageEvent::Done);
        assert!(!c.busy);
        assert_eq!(c.messages.len(), 1);
        assert_eq!(c.messages[0].text, "Hello there");
        assert_eq!(c.messages[0].steps, ["Looked at the screen"]);
        c.apply(SageEvent::Error("boom".into()));
        assert_eq!(c.messages.last().unwrap().role, Role::Error);
    }

    #[test]
    fn history_is_capped() {
        let mut c = Chat::default();
        for i in 0..(MAX_MESSAGES + 30) {
            c.push(Message { role: Role::User, text: i.to_string(), steps: vec![], pictures: vec![] });
        }
        assert_eq!(c.messages.len(), MAX_MESSAGES);
        assert_eq!(c.messages[0].text, "30");
    }

    #[test]
    fn pictures_are_sniffed_and_capped() {
        assert_eq!(picture_mime(&[0x89, b'P', b'N', b'G', 0, 0]), Some("image/png"));
        assert_eq!(picture_mime(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(picture_mime(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(picture_mime(b"PK\x03\x04"), None);
        assert!(SagePicture::from_bytes("a.png", vec![]).is_none());
        assert!(SagePicture::from_bytes("big.png", vec![0x89; MAX_PICTURE_BYTES + 1]).is_none());
        let mut c = Chat::default();
        let p = SagePicture::from_bytes("a.png", vec![0x89, b'P', b'N', b'G']).unwrap();
        for _ in 0..MAX_ATTACHMENTS {
            c.attach(p.clone()).unwrap();
        }
        assert!(c.attach(p).is_err());
        assert!(is_picture_name("Receipt.JPG"));
        assert!(!is_picture_name("book.xlsx"));
    }

    #[test]
    fn focus_follows_the_command() {
        let f = focus_of("range.setValues", &json!({"range": "Budget!B2", "values": [[1, 2, 3], [4, 5, 6]]})).unwrap();
        assert_eq!((f.sheet.as_deref(), f.range.a1().as_str(), f.write), (Some("Budget"), "B2:D3", true));
        let f = focus_of("sheet.read", &json!({"range": "A1:C9"})).unwrap();
        assert!(!f.write);
        assert_eq!(focus_of("home.bold", &json!({"range": "'My Sheet'!A1:A2"})).unwrap().sheet.as_deref(), Some("My Sheet"));
        assert_eq!(focus_of("insert.chart", &json!({"range": "A1:G1,A7:G7"})).unwrap().range.a1(), "A1:G1");
        assert!(focus_of("home.bold", &json!({})).is_none());
        assert!(focus_of("selection.set", &json!({"range": "A1"})).is_none());
        assert!(focus_of("cell.set", &json!({"cell": "not a cell"})).is_none());
        // Hostile sizes are clamped to the sheet.
        let huge: Vec<Json> = vec![json!([1]); 3];
        assert!(focus_of("range.setValues", &json!({"range": "XFD1048576", "values": huge})).is_some());
    }

    #[test]
    fn table_rows_parse() {
        assert_eq!(table_cells("| **Planet** | `Moons` |"), ["Planet", "Moons"]);
        assert_eq!(table_cells("|---|:--:|"), ["---", ":--:"]);
        assert_eq!(table_cells("|"), [""]);
    }

    #[test]
    fn inline_markdown_never_panics() {
        let t = Tokens::light();
        for s in ["**bold** and `code`", "**unclosed", "`unclosed", "a ** b ` c", "", "é**ü**`ß`", "****", "``"] {
            let job = inline(s, &t, t.text);
            let plain: String = job.text.clone();
            assert!(!plain.contains("**") || s.contains("****") || s == "a ** b ` c" || s == "**unclosed", "{s:?} → {plain:?}");
        }
    }
}
