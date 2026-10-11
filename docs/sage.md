# Sage

> **Last updated:** 2026-10-11

Sage is Celve's built-in assistant. It lives in a pane on the right of the window
(**Home ▸ Sage**, the sparkle button in the title bar, or **⌘⇧I**) and can:

- **Answer anything**: questions about the workbook, formulas, maths, writing, general knowledge, and
  current facts (it can search the web).
- **See**: it looks at the window when you refer to what's on screen, and checks its own work visually
  after building something. You can attach pictures (picture button, or drop a PNG/JPEG/GIF/WebP on the
  window while the pane is open): receipts, photos of tables, handwritten notes, screenshots.
- **Take over**: create workbooks and sheets, write data and formulas, format, make tables, sort,
  filter and chart — through exactly the same commands as the ribbon, the control channel, the CLI and
  MCP, so every change is undoable (⌘Z) and shows up in **Automate ▸ Action Journal**.

Stop ends the current reply; **New chat** starts a fresh conversation. Sage asks before deleting
sheets or overwriting data you didn't mention, and doesn't save files unless asked.

## How it works

```
Sage pane (ui-egui/src/sage.rs) ──SageHost──► apps/celve/src/sage.rs
                                                │  starts the assistant process (stream-JSON on stdio)
                                                │  with one MCP server: this executable in bridge mode
                                                ▼
                                   celve --mcp-bridge <port>   (crates/mcp, Remote backend)
                                                │  token-authenticated loopback control channel
                                                ▼
                                   the window's control queue (same as --control)
```

- The pane only draws the conversation; a `SageHost` supplied by the platform shell does the work.
  The web build has no host and shows a note instead.
- The assistant gets **only** Celve's MCP tools plus web search/fetch. It runs in its own working
  folder (`<settings>/Sage`) with none of the user's assistant settings, hooks, plugins or other MCP
  servers (`--setting-sources ""`, `--strict-mcp-config`, `--tools WebSearch,WebFetch`).
- Its tools reach the window through a **private control channel**: a second loopback listener on a
  free port that requires a random per-launch token as the first line of each connection
  ([control-protocol.md](control-protocol.md#private-channel)). The token travels to the bridge in its
  environment, never on a command line.
- Each message carries a one-line description of the workbook (title, sheets with used ranges,
  selection), so Sage rarely needs to ask where things are.
- Vision: the MCP `screenshot` tool without a `path` returns the window as an image block (scaled to
  1568 px). When the window can't be captured (screen locked, window hidden), Sage checks its work by
  reading cells instead and says so.
- The process starts when the pane opens (a few seconds), lives for the conversation, is killed on
  Stop and resumes the same conversation on the next message.

## Setup

Sage needs its assistant runtime installed and signed in on the computer (found on `PATH`, in
`~/.local/bin`, `/opt/homebrew/bin` or `/usr/local/bin`, or wherever `CELVE_SAGE_CLI` points).
Without it, the pane explains that Sage isn't set up. Other settings:

| Variable | Effect |
|---|---|
| `CELVE_SAGE_CLI` | Path to the assistant executable |
| `CELVE_SAGE_MODEL` | Model override passed to it |
| `CELVE_NO_SAGE` | Turns Sage and its private channel off |

## Driving Sage programmatically

`view.sage {on?}`, `sage.ask {text, pictures?: [path]}`, `sage.stop`, `sage.reset`, `sage.state`
(UI commands, reachable over the control channel). Offscreen renders of a conversation:
`cargo run -p gridcraft-ui-egui --example snapshot -- --sage state.json out.png` (the `sage.state` result).
