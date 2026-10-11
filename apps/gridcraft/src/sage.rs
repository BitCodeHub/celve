//! Sage, the built-in assistant (desktop only): the [`SageHost`] behind the Sage pane.
//!
//! Sage runs as a separate assistant process speaking stream-JSON on stdin/stdout. Its only
//! spreadsheet tools are GridCraft's own MCP tools, served by this same executable in bridge mode
//! (`gridcraft --mcp-bridge <port>`), which talks to the window through a private control channel
//! that requires a random per-launch token. So Sage drives the workbook with exactly the commands
//! every other agent uses, and sees the window through the `screenshot` tool.
//!
//! The process is started when the pane opens (it takes a few seconds), kept for the whole
//! conversation, killed on Stop and resumed with the same conversation on the next message.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use gridcraft_ui_egui::sage::{SageEvent, SageHost, SagePicture};
use serde_json::{Value as Json, json};

/// Environment variable naming the assistant executable (overrides the search).
pub const CLI_ENV: &str = "GRIDCRAFT_SAGE_CLI";
/// Environment variable carrying the private channel's token to the bridge process.
pub const TOKEN_ENV: &str = "GRIDCRAFT_SAGE_TOKEN";
/// Optional model override (`GRIDCRAFT_SAGE_MODEL=…`); never shown in the UI.
const MODEL_ENV: &str = "GRIDCRAFT_SAGE_MODEL";
/// Name of the MCP server in the assistant's config (tool names are `mcp__gridcraft__…`).
const SERVER: &str = "gridcraft";
/// Lines of the assistant's standard error kept for error messages.
const STDERR_TAIL: usize = 20;
/// Longest stdout line accepted (a screenshot echoed back can be large).
const MAX_LINE: u64 = 64 * 1024 * 1024;

const PERSONA: &str = r#"You are Sage, the assistant built into GridCraft, a spreadsheet app. You are warm, sharp and brief.

What you do:
- Answer anything the user asks: general knowledge, maths, writing, planning, explanations. Search the web when the answer depends on current facts.
- See and drive the open workbook with your gridcraft tools. When the user asks you to make or change something, DO IT yourself with the tools; never tell them which buttons to click unless they ask how.
- Each user message starts with a <workbook> line describing the open workbook and selection right now. Use it; call inspect_workbook or read_range when you need the actual contents.
- To see the window (charts, formatting, "what's on my screen", "this"), call screenshot with no path: it returns an image you can look at. After building something substantial, take one screenshot to check it looks right, and fix what doesn't. If the screenshot fails (the computer's screen is locked or the window is hidden), don't keep retrying: check your work with read_range and inspect_workbook instead, and mention in one short clause that you couldn't look.
- Pictures the user attaches (receipts, photos of tables, handwritten notes, screenshots): read them carefully and, when asked, turn them into clean sheets. Never invent values you can't read; mark them for the user to check.

How to build well:
- Real spreadsheets: a header row in bold, numbers as numbers (not text), formulas for every total/derived value (=SUM, =AVERAGE, =XLOOKUP…) rather than typed results, number formats (currency, percent, dates), column widths that fit (execute_command home.autofitColumnWidth {range: "A1:H20"} AFTER number formats are set), a table (create_table) for lists, and a chart when it helps (insert_chart takes one block such as "A1:G6", or areas that line up such as "A1:G1,A7:G7" for a header row plus a totals row).
- Write data in bulk with write_range (a 2-D array) instead of many set_cell calls.
- New sheets: execute_command home.insertSheet {name}; rename with sheet.rename {sheet, name}; switch with sheet.activate {sheet}. New workbook: new_workbook. Use list_commands {search} to find any other command (there are about 630).
- Before deleting sheets, clearing large ranges, or overwriting existing data the user didn't mention, ask first. Don't save or overwrite files unless asked; if asked to save, use save_workbook (it needs a path the first time; suggest ~/Documents/<name>.xlsx).

Style:
- Keep replies short. After doing work, summarise what you did in 1-3 bullets and offer one useful next step.
- Never mention your tools, MCP, commands' internal ids, the underlying AI model or its maker. If asked what you are or which model powers you, say you're Sage, GridCraft's assistant, and leave it there."#;

/// What the reader thread shares with the UI side.
#[derive(Default)]
struct Shared {
    events: Vec<SageEvent>,
    /// The conversation to resume after a restart.
    session_id: Option<String>,
    /// A reply is in progress (an unexpected exit then reports an error).
    in_turn: bool,
    /// Last lines of the process's standard error.
    stderr: Vec<String>,
}

struct Running {
    child: Child,
    stdin: ChildStdin,
}

pub struct Sage {
    cli: Option<PathBuf>,
    /// This executable, started by the assistant as the MCP bridge.
    exe: Option<PathBuf>,
    port: u16,
    token: String,
    workdir: PathBuf,
    ctx: egui::Context,
    shared: Arc<Mutex<Shared>>,
    /// Bumped whenever a process is replaced; readers of older ones stop reporting.
    generation: Arc<AtomicU64>,
    running: Option<Running>,
}

impl Sage {
    pub fn new(port: u16, token: String, workdir: PathBuf, ctx: egui::Context) -> Self {
        Sage {
            cli: find_cli(),
            exe: std::env::current_exe().ok(),
            port,
            token,
            workdir,
            ctx,
            shared: Arc::new(Mutex::new(Shared::default())),
            generation: Arc::new(AtomicU64::new(0)),
            running: None,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn push(&self, e: SageEvent) {
        self.lock().events.push(e);
        self.ctx.request_repaint();
    }

    fn alive(&mut self) -> bool {
        match self.running.as_mut().map(|r| r.child.try_wait()) {
            Some(Ok(None)) => true,
            Some(_) => {
                self.running = None;
                false
            }
            None => false,
        }
    }

    fn kill(&mut self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        if let Some(mut r) = self.running.take() {
            let _ = r.child.kill();
            // Reap it off the UI thread.
            let _ = std::thread::Builder::new().name("sage-reap".into()).spawn(move || {
                let _ = r.child.wait();
            });
        }
        self.lock().in_turn = false;
    }

    /// The assistant's MCP config: this executable in bridge mode. Holds no secrets: it goes on
    /// the assistant's command line.
    fn mcp_config(&self) -> Json {
        let exe = self.exe.as_deref().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
        json!({"mcpServers": {SERVER: {"command": exe, "args": ["--mcp-bridge", self.port.to_string()]}}})
    }

    fn spawn(&mut self) -> Result<(), String> {
        if self.alive() {
            return Ok(());
        }
        let cli = self.cli.clone().ok_or_else(not_installed)?;
        if self.exe.is_none() {
            return Err("Sage couldn't find GridCraft's own executable.".into());
        }
        std::fs::create_dir_all(&self.workdir).map_err(|e| format!("Sage couldn't create its folder: {e}"))?;
        let mcp = self.mcp_config();
        let resume = self.lock().session_id.clone();
        let args = command_args(&mcp.to_string(), resume.as_deref(), std::env::var(MODEL_ENV).ok().as_deref());
        let mut cmd = Command::new(&cli);
        cmd.args(&args)
            .current_dir(&self.workdir)
            .env("PATH", search_path())
            // The bridge inherits the token from here: never on a command line (visible in `ps`).
            .env(TOKEN_ENV, &self.token)
            // Started from inside another assistant session (development), the child would refuse.
            .env_remove("CLAUDECODE")
            .env_remove("CLAUDE_CODE_ENTRYPOINT")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| format!("Sage couldn't start: {e}"))?;
        let (Some(stdin), Some(stdout), Some(stderr)) = (child.stdin.take(), child.stdout.take(), child.stderr.take()) else {
            let _ = child.kill();
            return Err("Sage couldn't connect to its assistant process.".into());
        };
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        {
            let shared = self.shared.clone();
            let gen_now = self.generation.clone();
            let ctx = self.ctx.clone();
            std::thread::Builder::new()
                .name("sage-out".into())
                .spawn(move || read_stdout(stdout, shared, gen_now, generation, ctx))
                .map_err(|e| e.to_string())?;
        }
        {
            let shared = self.shared.clone();
            std::thread::Builder::new().name("sage-err".into()).spawn(move || read_stderr(stderr, shared)).map_err(|e| e.to_string())?;
        }
        self.running = Some(Running { child, stdin });
        log::info!("Sage started{}", if resume.is_some() { " (resuming the conversation)" } else { "" });
        Ok(())
    }
}

impl SageHost for Sage {
    fn availability(&self) -> Result<(), String> {
        if self.cli.is_none() {
            return Err(not_installed());
        }
        Ok(())
    }

    fn warm_up(&mut self) {
        if self.cli.is_some()
            && let Err(e) = self.spawn()
        {
            log::warn!("Sage warm-up: {e}");
        }
    }

    fn send(&mut self, text: &str, pictures: &[SagePicture], context: &str) {
        if let Err(e) = self.spawn() {
            self.push(SageEvent::Error(e));
            return;
        }
        let line = user_message(text, pictures, context).to_string();
        {
            let mut s = self.lock();
            s.in_turn = true;
            s.stderr.clear();
        }
        let wrote =
            self.running.as_mut().map(|r| r.stdin.write_all(line.as_bytes()).and_then(|()| r.stdin.write_all(b"\n")).and_then(|()| r.stdin.flush()));
        if !matches!(wrote, Some(Ok(()))) {
            self.kill();
            self.push(SageEvent::Error("Sage lost its connection. Send the message again.".into()));
        }
    }

    fn stop(&mut self) {
        self.kill();
        // Come back with the same conversation, ready for the next message.
        self.warm_up();
    }

    fn reset(&mut self) {
        self.kill();
        self.lock().session_id = None;
        self.warm_up();
    }

    fn poll(&mut self) -> Vec<SageEvent> {
        std::mem::take(&mut self.lock().events)
    }
}

impl Drop for Sage {
    fn drop(&mut self) {
        self.kill();
    }
}

fn not_installed() -> String {
    format!(
        "Sage isn't set up on this computer yet: its assistant runtime wasn't found. Install it and sign in, or point {CLI_ENV} at it, then reopen GridCraft."
    )
}

/// The assistant's command line: stream-JSON both ways, no built-in tools except web search and
/// fetch, only GridCraft's MCP server, none of the user's own settings, hooks or plugins.
fn command_args(mcp_config: &str, resume: Option<&str>, model: Option<&str>) -> Vec<String> {
    let mut a: Vec<String> = [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--setting-sources",
        "",
        "--strict-mcp-config",
        "--mcp-config",
        mcp_config,
        "--tools",
        "WebSearch,WebFetch",
        "--allowedTools",
        "mcp__gridcraft",
        "WebSearch",
        "WebFetch",
        "--system-prompt",
        PERSONA,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if let Some(id) = resume.filter(|id| is_session_id(id)) {
        a.push("--resume".into());
        a.push(id.to_string());
    }
    if let Some(m) = model.map(str::trim).filter(|m| !m.is_empty() && m.len() < 100 && !m.starts_with('-')) {
        a.push("--model".into());
        a.push(m.to_string());
    }
    a
}

/// Session ids are UUIDs; anything else is ignored rather than passed on a command line.
fn is_session_id(s: &str) -> bool {
    s.len() == 36 && s.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// One user turn: the workbook context, the text, and any pictures as image blocks.
fn user_message(text: &str, pictures: &[SagePicture], context: &str) -> Json {
    let mut content = vec![json!({"type": "text", "text": format!("<workbook>{context}</workbook>\n\n{text}")})];
    for p in pictures {
        content.push(
            json!({"type": "image", "source": {"type": "base64", "media_type": p.mime, "data": gridcraft_engine::io::base64_encode(&p.bytes)}}),
        );
    }
    json!({"type": "user", "message": {"role": "user", "content": content}})
}

/// Turns the assistant's stream into pane events.
#[derive(Default)]
struct Translator {
    /// Text of this message arrived as deltas (so the full message isn't repeated).
    streamed: bool,
    /// The reply already has text (later text blocks start a new paragraph).
    has_text: bool,
}

impl Translator {
    fn line(&mut self, v: &Json, shared: &Mutex<Shared>) -> Vec<SageEvent> {
        let mut out = vec![];
        match v.get("type").and_then(Json::as_str) {
            Some("system") => {
                if let Some(id) = v.get("session_id").and_then(Json::as_str).filter(|id| is_session_id(id)) {
                    shared.lock().unwrap_or_else(PoisonError::into_inner).session_id = Some(id.to_string());
                }
            }
            Some("stream_event") => {
                let e = v.get("event").unwrap_or(&Json::Null);
                match e.get("type").and_then(Json::as_str) {
                    Some("message_start") => self.streamed = false,
                    Some("content_block_start") if e.pointer("/content_block/type").and_then(Json::as_str) == Some("text") && self.has_text => {
                        out.push(SageEvent::Text("\n\n".into()));
                    }
                    Some("content_block_delta") => {
                        if let Some(t) = e
                            .pointer("/delta/text")
                            .and_then(Json::as_str)
                            .filter(|_| e.pointer("/delta/type").and_then(Json::as_str) == Some("text_delta"))
                        {
                            self.streamed = true;
                            self.has_text = true;
                            out.push(SageEvent::Text(t.to_string()));
                        }
                    }
                    _ => {}
                }
            }
            Some("assistant") => {
                let blocks = v.pointer("/message/content").and_then(Json::as_array).cloned().unwrap_or_default();
                for b in &blocks {
                    match b.get("type").and_then(Json::as_str) {
                        Some("text") if !self.streamed => {
                            if let Some(t) = b.get("text").and_then(Json::as_str).filter(|t| !t.is_empty()) {
                                if self.has_text {
                                    out.push(SageEvent::Text("\n\n".into()));
                                }
                                self.has_text = true;
                                out.push(SageEvent::Text(t.to_string()));
                            }
                        }
                        Some("tool_use") => {
                            let name = b.get("name").and_then(Json::as_str).unwrap_or("");
                            out.push(SageEvent::Step(describe_step(name, b.get("input").unwrap_or(&Json::Null))));
                        }
                        _ => {}
                    }
                }
                self.streamed = false;
            }
            Some("result") => {
                self.has_text = false;
                self.streamed = false;
                let mut s = shared.lock().unwrap_or_else(PoisonError::into_inner);
                s.in_turn = false;
                if let Some(id) = v.get("session_id").and_then(Json::as_str).filter(|id| is_session_id(id)) {
                    s.session_id = Some(id.to_string());
                }
                drop(s);
                if v.get("is_error").and_then(Json::as_bool) == Some(true) {
                    let why = v.get("result").and_then(Json::as_str).filter(|r| !r.is_empty()).unwrap_or("Sage ran into a problem. Try again.");
                    out.push(SageEvent::Error(friendly_error(why)));
                } else {
                    out.push(SageEvent::Done);
                }
            }
            _ => {}
        }
        out
    }
}

fn read_stdout(stdout: impl Read, shared: Arc<Mutex<Shared>>, gen_now: Arc<AtomicU64>, generation: u64, ctx: egui::Context) {
    let mut reader = BufReader::new(stdout);
    let mut tr = Translator::default();
    loop {
        let mut line = String::new();
        match (&mut reader).take(MAX_LINE).read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if gen_now.load(Ordering::SeqCst) != generation {
            return; // replaced or stopped: stay quiet
        }
        let Ok(v) = serde_json::from_str::<Json>(line.trim()) else { continue };
        let events = tr.line(&v, &shared);
        if !events.is_empty() {
            shared.lock().unwrap_or_else(PoisonError::into_inner).events.extend(events);
            ctx.request_repaint();
        }
    }
    if gen_now.load(Ordering::SeqCst) != generation {
        return;
    }
    let mut s = shared.lock().unwrap_or_else(PoisonError::into_inner);
    if s.in_turn {
        s.in_turn = false;
        let tail = s.stderr.iter().rev().find(|l| !l.trim().is_empty()).cloned().unwrap_or_default();
        let msg = if tail.is_empty() { "Sage stopped unexpectedly. Send the message again.".to_string() } else { friendly_error(&tail) };
        s.events.push(SageEvent::Error(msg));
        ctx.request_repaint();
    }
}

fn read_stderr(stderr: impl Read, shared: Arc<Mutex<Shared>>) {
    for line in BufReader::new(stderr).lines() {
        let Ok(line) = line else { break };
        log::debug!("sage: {line}");
        let mut s = shared.lock().unwrap_or_else(PoisonError::into_inner);
        s.stderr.push(line.chars().take(500).collect());
        let extra = s.stderr.len().saturating_sub(STDERR_TAIL);
        s.stderr.drain(..extra);
    }
}

/// Keeps vendor and plumbing details out of what the user reads.
fn friendly_error(raw: &str) -> String {
    let lower = raw.to_ascii_lowercase();
    if lower.contains("log in") || lower.contains("login") || lower.contains("authenticat") || lower.contains("oauth") || lower.contains("401") {
        "Sage needs you to sign in again on this computer, then try once more.".into()
    } else if lower.contains("rate limit") || lower.contains("usage limit") || lower.contains("429") || lower.contains("overloaded") {
        "Sage is busy right now (usage limit reached). Try again in a little while.".into()
    } else if lower.contains("network") || lower.contains("enotfound") || lower.contains("econn") || lower.contains("timed out") {
        "Sage can't reach the internet right now. Check your connection and try again.".into()
    } else {
        "Sage ran into a problem. Try again.".into()
    }
}

/// One short line for the pane per tool call.
fn describe_step(tool: &str, input: &Json) -> String {
    let s = |k: &str| input.get(k).and_then(Json::as_str).unwrap_or("").to_string();
    let name = tool.strip_prefix("mcp__gridcraft__").unwrap_or(tool);
    match name {
        "screenshot" => "Looked at the screen".into(),
        "inspect_workbook" => "Looked over the workbook".into(),
        "read_range" => format!("Read {}", or(&s("range"), "the sheet")),
        "write_range" => format!("Wrote {}", or(&s("range"), "cells")),
        "set_cell" => format!("Set {}", or(&s("cell"), "a cell")),
        "get_cell" => format!("Checked {}", or(&s("cell"), "a cell")),
        "evaluate_formula" => "Worked out a formula".into(),
        "select" => format!("Selected {}", or(&s("range"), "cells")),
        "format_range" => format!("Formatted {}", or(&s("range"), "cells")),
        "insert_chart" => format!("Added a {} chart", or(&s("type"), "new")),
        "create_table" => format!("Made a table at {}", or(&s("range"), "the data")),
        "sort_range" => "Sorted the data".into(),
        "filter" => "Filtered the data".into(),
        "open_workbook" => "Opened a workbook".into(),
        "save_workbook" => "Saved the workbook".into(),
        "new_workbook" => "Started a new workbook".into(),
        "undo" => "Undid a change".into(),
        "redo" => "Redid a change".into(),
        "list_commands" | "list_functions" => "Looked up what's available".into(),
        "execute_command" => {
            let cmd = s("command");
            let params = input.get("params").unwrap_or(&Json::Null);
            let p = |k: &str| params.get(k).and_then(Json::as_str).unwrap_or("").to_string();
            match cmd.as_str() {
                "home.insertSheet" => format!("Added sheet {}", or(&p("name"), "")).trim_end().to_string(),
                "sheet.rename" => format!("Renamed a sheet to {}", p("name")),
                "sheet.activate" => "Switched sheets".into(),
                "home.deleteSheet" => "Deleted a sheet".into(),
                _ => gridcraft_engine::find_command(&cmd).map(|c| format!("Used {}", c.label)).unwrap_or_else(|| "Ran a command".into()),
            }
        }
        "WebSearch" => format!("Searched the web for “{}”", short(&s("query"), 60)),
        "WebFetch" => "Read a web page".into(),
        _ => "Worked on it".into(),
    }
}

fn or(s: &str, fallback: &str) -> String {
    if s.is_empty() { fallback.to_string() } else { short(s, 40) }
}

fn short(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { format!("{}…", s.chars().take(n).collect::<String>()) }
}

/// The assistant executable: the override, then `PATH`, then the usual install locations (apps
/// started from the Finder get a minimal `PATH`).
fn find_cli() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(CLI_ENV).map(PathBuf::from).filter(|p| p.is_file()) {
        return Some(p);
    }
    let name = if cfg!(windows) { "claude.exe" } else { "claude" };
    std::env::split_paths(&search_path()).map(|d| d.join(name)).find(|p| p.is_file())
}

/// `PATH` plus the common install folders.
fn search_path() -> std::ffi::OsString {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from) {
        for d in [".local/bin", ".claude/local", ".npm-global/bin", ".bun/bin"] {
            dirs.push(home.join(d));
        }
    }
    for d in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"] {
        dirs.push(Path::new(d).to_path_buf());
    }
    std::env::join_paths(dirs).unwrap_or_default()
}

/// A random per-launch token (std's hasher keys are seeded from the OS's random source).
#[allow(clippy::disallowed_methods)] // desktop only: the clock just adds entropy
pub fn new_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut out = String::new();
    for i in 0..4u64 {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(i);
        h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
        h.write_u32(std::process::id());
        out.push_str(&format!("{:016x}", h.finish()));
    }
    out
}

/// `gridcraft --mcp-bridge <port>`: GridCraft's MCP server on stdio, connected to the window's
/// private control channel (token from [`TOKEN_ENV`]). Started by the assistant, not by people.
pub fn run_bridge(port: &str) -> i32 {
    let addr = gridcraft_mcp::control_addr(port);
    let token = std::env::var(TOKEN_ENV).ok().filter(|t| !t.is_empty());
    let backend = match gridcraft_mcp::Remote::connect_with_token(&addr, token) {
        Ok(b) => b,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "gridcraft: can't reach the GridCraft window at {addr}: {e}");
            return 1;
        }
    };
    let mut server = gridcraft_mcp::Server::new(Box::new(backend));
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    match server.serve(stdin.lock(), stdout.lock()) {
        Ok(()) => 0,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "gridcraft: MCP bridge: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn run(lines: &[Json]) -> (Vec<SageEvent>, Shared) {
        let shared = Mutex::new(Shared { in_turn: true, ..Default::default() });
        let mut tr = Translator::default();
        let events = lines.iter().flat_map(|l| tr.line(l, &shared)).collect();
        (events, shared.into_inner().unwrap())
    }

    const SID: &str = "d320de34-83eb-4d44-a0dc-343c44277170";

    #[test]
    fn streamed_text_tool_steps_and_result() {
        let delta = |t: &str| json!({"type": "stream_event", "event": {"type": "content_block_delta", "delta": {"type": "text_delta", "text": t}}});
        let (events, shared) = run(&[
            json!({"type": "system", "subtype": "init", "session_id": SID}),
            json!({"type": "stream_event", "event": {"type": "message_start"}}),
            json!({"type": "stream_event", "event": {"type": "content_block_start", "content_block": {"type": "text"}}}),
            delta("On it"),
            delta("."),
            json!({"type": "assistant", "message": {"content": [
                {"type": "text", "text": "On it."},
                {"type": "tool_use", "name": "mcp__gridcraft__write_range", "input": {"range": "A1:C4", "values": [[1]]}},
            ]}}),
            json!({"type": "stream_event", "event": {"type": "message_start"}}),
            json!({"type": "stream_event", "event": {"type": "content_block_start", "content_block": {"type": "text"}}}),
            delta("Done!"),
            json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "Done!"}]}}),
            json!({"type": "result", "subtype": "success", "is_error": false, "session_id": SID}),
        ]);
        assert_eq!(
            events,
            vec![
                SageEvent::Text("On it".into()),
                SageEvent::Text(".".into()),
                SageEvent::Step("Wrote A1:C4".into()),
                SageEvent::Text("\n\n".into()),
                SageEvent::Text("Done!".into()),
                SageEvent::Done,
            ]
        );
        assert_eq!(shared.session_id.as_deref(), Some(SID));
        assert!(!shared.in_turn);
    }

    #[test]
    fn unstreamed_text_still_arrives_and_errors_are_friendly() {
        let (events, _) = run(&[
            json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "Hi"}]}}),
            json!({"type": "result", "is_error": true, "result": "API Error: 401 OAuth token has expired"}),
        ]);
        assert_eq!(events[0], SageEvent::Text("Hi".into()));
        match &events[1] {
            SageEvent::Error(m) => {
                assert!(m.contains("sign in"), "{m}");
                assert!(!m.contains("OAuth") && !m.contains("API"), "{m}");
            }
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn steps_read_like_plain_english() {
        assert_eq!(describe_step("mcp__gridcraft__screenshot", &json!({})), "Looked at the screen");
        assert_eq!(
            describe_step("mcp__gridcraft__execute_command", &json!({"command": "home.insertSheet", "params": {"name": "Budget"}})),
            "Added sheet Budget"
        );
        assert_eq!(describe_step("mcp__gridcraft__execute_command", &json!({"command": "home.bold"})), "Used Bold");
        assert_eq!(describe_step("mcp__gridcraft__execute_command", &json!({"command": 7})), "Ran a command");
        assert_eq!(describe_step("mcp__gridcraft__insert_chart", &json!({"type": "pie"})), "Added a pie chart");
        assert_eq!(describe_step("WebSearch", &json!({"query": "fed rate"})), "Searched the web for “fed rate”");
        assert_eq!(describe_step("mystery", &Json::Null), "Worked on it");
    }

    #[test]
    fn the_token_never_reaches_a_command_line() {
        let sage = Sage::new(4242, "tok-123-secret".into(), std::env::temp_dir().join("gc-sage-test"), egui::Context::default());
        let mcp = sage.mcp_config();
        assert!(!mcp.to_string().contains("tok-123-secret"), "{mcp}");
        let args = command_args(&mcp.to_string(), None, None);
        assert!(!args.iter().any(|a| a.contains("tok-123-secret")));
    }

    #[test]
    fn command_line_is_locked_down() {
        let a = command_args("{}", Some(SID), Some("x"));
        let has = |s: &str| a.iter().any(|x| x == s);
        assert!(has("--strict-mcp-config") && has("--setting-sources") && has("--system-prompt"));
        let tools = a.iter().position(|x| x == "--tools").unwrap();
        assert_eq!(a[tools + 1], "WebSearch,WebFetch");
        assert!(a.windows(2).any(|w| w[0] == "--resume" && w[1] == SID));
        // Hostile ids and model names never reach the command line.
        let a = command_args("{}", Some("--dangerously-skip-permissions"), Some("--evil"));
        assert!(!a.iter().any(|x| x == "--resume" || x == "--model" || x == "--evil"));
    }

    #[test]
    fn user_message_carries_context_and_pictures() {
        let p = SagePicture { name: "r.png".into(), mime: "image/png".into(), bytes: vec![1, 2, 3] };
        let m = user_message("Make a sheet", &[p], "Workbook \"Book1\"");
        assert_eq!(m["type"], "user");
        let c = m["message"]["content"].as_array().unwrap();
        assert!(c[0]["text"].as_str().unwrap().starts_with("<workbook>Workbook \"Book1\"</workbook>"));
        assert_eq!(c[1]["source"]["media_type"], "image/png");
        assert_eq!(c[1]["source"]["data"], "AQID");
    }

    #[test]
    fn tokens_are_long_and_differ() {
        let (a, b) = (new_token(), new_token());
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
    }
}
