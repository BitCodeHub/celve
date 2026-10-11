//! Sage: the built-in assistant pane. Sage answers questions, looks at the window, and takes
//! over the workbook (creates sheets, writes data and formulas, formats, charts) through the same
//! commands as every other agent.
//!
//! The UI only holds the conversation and draws it. The work happens in a [`SageHost`] the
//! platform shell provides (the desktop app runs the assistant process; the web build has none).
//! A host reports progress as [`SageEvent`]s, picked up once per frame in [`poll`].

use egui::text::LayoutJob;
use egui::{Color32, FontId, RichText, TextFormat, Ui, vec2};
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

/// UI commands: `view.sage {on?}`, `sage.ask {text, pictures?: [path]}`, `sage.stop`, `sage.reset`,
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
}

/// The pane, docked on the right.
pub fn show(app: &mut SheetApp, ui: &mut Ui) {
    if !app.sage.open {
        return;
    }
    let t = Tokens::get(ui.ctx());
    egui::Panel::right("sage_pane")
        .default_size(360.0)
        .min_size(280.0)
        .resizable(true)
        .frame(egui::Frame::NONE.fill(t.ribbon).inner_margin(10))
        .show(ui, |ui| {
            header(app, ui, &t);
            ui.separator();
            let available =
                app.services.sage.as_ref().map(|h| h.availability()).unwrap_or_else(|| Err("Sage runs in the GridCraft desktop app.".into()));
            let composer_h = if available.is_ok() { 118.0 + if app.sage.attachments.is_empty() { 0.0 } else { 26.0 } } else { 0.0 };
            let list_h = (ui.available_height() - composer_h).max(80.0);
            egui::ScrollArea::vertical().max_height(list_h).auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
                ui.set_width(ui.available_width());
                if app.sage.messages.is_empty() {
                    welcome(app, ui, &t);
                }
                for m in &app.sage.messages {
                    message(ui, m, &t);
                }
                if app.sage.busy {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(14.0));
                        ui.label(RichText::new("Sage is working…").color(t.text_dim).italics());
                    });
                }
                if std::mem::take(&mut app.sage.scroll_to_end) {
                    ui.scroll_to_cursor(Some(egui::Align::BOTTOM));
                }
            });
            match available {
                Ok(()) => composer(app, ui, &t),
                Err(why) => {
                    ui.add_space(8.0);
                    ui.label(RichText::new(why).color(t.text_dim));
                }
            }
        });
}

fn header(app: &mut SheetApp, ui: &mut Ui, t: &Tokens) {
    ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(vec2(20.0, 20.0), egui::Sense::hover());
        crate::icons::paint(ui.painter(), r, Icon::Sage, t.text);
        ui.label(RichText::new("Sage").font(theme::ui_bold(15.0)));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if crate::widgets::icon_button(ui, Icon::Close, t.text_dim, "Close Sage", vec2(22.0, 22.0)).clicked() {
                app.sage.open = false;
            }
            if !app.sage.messages.is_empty() && ui.small_button("New chat").on_hover_text("Start a new conversation").clicked() {
                reset(app);
            }
        });
    });
}

const SUGGESTIONS: [&str; 4] = [
    "Build a monthly budget with income, expenses and a chart",
    "Explain what's on this sheet",
    "Clean up and format the selected table",
    "Add a summary sheet with totals by category",
];

fn welcome(app: &mut SheetApp, ui: &mut Ui, t: &Tokens) {
    ui.add_space(6.0);
    ui.label(RichText::new("Hi, I'm Sage.").font(theme::ui_bold(16.0)));
    ui.label(
        RichText::new(
            "Ask me anything. I can see this window, build workbooks and sheets, write formulas, format, sort and chart. Attach a photo of a receipt or a table and I'll turn it into a sheet.",
        )
        .color(t.text_dim),
    );
    ui.add_space(8.0);
    for s in SUGGESTIONS {
        let b = egui::Button::new(RichText::new(s).color(t.text)).fill(t.window).corner_radius(8.0).min_size(vec2(ui.available_width(), 30.0));
        if ui.add(b).clicked() {
            app.sage.input = s.to_string();
            if let Err(e) = send(app) {
                app.sage.apply(SageEvent::Error(e));
            }
        }
    }
}

fn message(ui: &mut Ui, m: &Message, t: &Tokens) {
    ui.add_space(6.0);
    let (fill, stroke) = match m.role {
        Role::User => (t.accent_soft, Color32::TRANSPARENT),
        Role::Sage => (t.window, t.separator),
        Role::Error => (Color32::from_rgba_unmultiplied(0xD1, 0x3B, 0x2F, 28), Color32::from_rgb(0xD1, 0x3B, 0x2F)),
    };
    egui::Frame::NONE.fill(fill).stroke(egui::Stroke::new(1.0, stroke)).corner_radius(8.0).inner_margin(8.0).show(ui, |ui| {
        ui.set_width(ui.available_width());
        if m.role == Role::Sage {
            ui.label(RichText::new("Sage").font(theme::ui_bold(12.0)).color(icon_green(t)));
        }
        for p in &m.pictures {
            ui.label(RichText::new(format!("🖼 {p}")).color(t.text_dim).small());
        }
        for s in &m.steps {
            ui.label(RichText::new(format!("✓ {s}")).color(t.text_dim).small());
        }
        if !m.text.is_empty() {
            markdown(ui, &m.text, t);
        }
    });
}

fn icon_green(t: &Tokens) -> Color32 {
    if t.dark { Color32::from_rgb(0x4C, 0xC3, 0x84) } else { crate::icons::GREEN }
}

fn composer(app: &mut SheetApp, ui: &mut Ui, t: &Tokens) {
    ui.add_space(6.0);
    if !app.sage.attachments.is_empty() {
        let mut remove = None;
        ui.horizontal_wrapped(|ui| {
            for (i, p) in app.sage.attachments.iter().enumerate() {
                if ui.small_button(format!("🖼 {} ✕", short(&p.name, 18))).on_hover_text("Remove").clicked() {
                    remove = Some(i);
                }
            }
        });
        if let Some(i) = remove.filter(|&i| i < app.sage.attachments.len()) {
            app.sage.attachments.remove(i);
        }
    }
    let edit = egui::TextEdit::multiline(&mut app.sage.input)
        .hint_text("Ask Sage anything, or tell it what to build…")
        .desired_rows(3)
        .desired_width(f32::INFINITY)
        .font(FontId::new(13.0, egui::FontFamily::Proportional));
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
    }
    let mut do_send = enter;
    ui.horizontal(|ui| {
        if crate::widgets::icon_button(ui, Icon::Picture, t.text_dim, "Attach a picture", vec2(26.0, 24.0)).clicked() {
            attach_from_picker(app);
        }
        if ui.small_button("👁 Look").on_hover_text("Ask Sage to look at the window").clicked() && !app.sage.busy {
            if app.sage.input.trim().is_empty() {
                app.sage.input = "Look at my screen and tell me what you see.".into();
            }
            do_send = true;
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if app.sage.busy {
                if ui.button("■ Stop").clicked() {
                    stop(app);
                }
            } else {
                let can = !app.sage.input.trim().is_empty() || !app.sage.attachments.is_empty();
                if ui.add_enabled(can, egui::Button::new(RichText::new("Send").strong())).clicked() {
                    do_send = true;
                }
            }
        });
    });
    if do_send && !app.sage.busy {
        if let Err(e) = send(app) {
            app.sage.apply(SageEvent::Error(e));
        }
        app.sage.focus_input = true;
    }
}

fn attach_from_picker(app: &mut SheetApp) {
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
fn markdown(ui: &mut Ui, text: &str, t: &Tokens) {
    let mut in_code = false;
    let mut code = String::new();
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            if in_code {
                egui::Frame::NONE.fill(t.input_bg).corner_radius(4.0).inner_margin(6.0).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new(code.trim_end()).monospace().size(12.0).color(t.text));
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
            ui.label(RichText::new(h).font(theme::ui_bold(14.0)).color(t.text));
        } else if let Some(b) = trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* ")) {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                ui.label(RichText::new("•").color(t.text_dim));
                ui.label(inline(b, t));
            });
        } else if trimmed.is_empty() {
            ui.add_space(4.0);
        } else {
            ui.label(inline(line, t));
        }
    }
    if in_code && !code.is_empty() {
        ui.label(RichText::new(code.trim_end()).monospace().size(12.0).color(t.text));
    }
}

fn inline(s: &str, t: &Tokens) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;
    let normal = TextFormat { font_id: FontId::new(13.0, egui::FontFamily::Proportional), color: t.text, ..Default::default() };
    let bold = TextFormat { font_id: theme::ui_bold(13.0), color: t.text, ..Default::default() };
    let mono = TextFormat { font_id: FontId::new(12.0, egui::FontFamily::Monospace), color: t.text, background: t.input_bg, ..Default::default() };
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
    fn inline_markdown_never_panics() {
        let t = Tokens::light();
        for s in ["**bold** and `code`", "**unclosed", "`unclosed", "a ** b ` c", "", "é**ü**`ß`", "****", "``"] {
            let job = inline(s, &t);
            let plain: String = job.text.clone();
            assert!(!plain.contains("**") || s.contains("****") || s == "a ** b ` c" || s == "**unclosed", "{s:?} → {plain:?}");
        }
    }
}
