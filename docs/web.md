# Celve Web

> **Last updated:** 2026-10-11

Celve runs in any modern browser, with nothing to install. The workbook lives in the page (the
same engine as the desktop app, compiled to WebAssembly), and **Sage** lives inside it: open the
pane with **Ask Sage**, ask anything, and watch Sage build and edit the sheet in real time. A green
outline tagged "Sage" marks the range it is reading or changing, the status bar says what it is
doing, and it can look at the page itself (screenshots) to check its work.

## Run it

```sh
cd apps/celve-web && trunk build --release        # → dist/web
cargo run --release -p celve-server               # prints: open http://127.0.0.1:8780/?key=…
```

Sage always needs an access key. Without `CELVE_ACCESS_KEY` the server makes one for the run and
prints the link that carries it; open that link (bookmark it, or set `CELVE_ACCESS_KEY` to keep a
fixed key).

| Option | Env | Default | |
|---|---|---|---|
| `--port` | `CELVE_SERVER_PORT` | 8780 | |
| `--bind` | `CELVE_SERVER_BIND` | 127.0.0.1 | use `0.0.0.0` behind a TLS proxy |
| `--dist` | `CELVE_WEB_DIST` | `dist/web` | the trunk build |
| `--access-key` | `CELVE_ACCESS_KEY` | generated per run | required as `?key=…` to use Sage; set a long random one for any server others can reach |
| | `CELVE_ALLOWED_ORIGINS` | none | extra page origins allowed to connect (comma-separated) |
| | `CELVE_ACCESS_TEAM`, `CELVE_ACCESS_AUD`, `CELVE_ALLOWED_EMAILS` | none | sign-in through Cloudflare Access instead of a key (all three together; see below) |

Links: `?sample=sales` opens a sample, `?sage` opens the Sage pane, `?ask=…` opens it with a
message typed in (never sent automatically, so a link can't make Sage act on your sheet).

## Sign-in instead of a key

A key in a link can leak (history, screenshots, a pasted URL). For a server on the internet, put
Sage behind sign-in: a Cloudflare Access application covering `<host>/ws` and `<host>/signin`,
with a policy allowing the people's email addresses (one-time code by email works without any
other account). Then set the team domain (`https://<team>.cloudflareaccess.com`), the
application's audience tag and the same emails. The server then:

- ignores `?key=`, and refuses `/ws` unless the request carries a valid Access pass
  (`Cf-Access-Jwt-Assertion`): RS256, signed by the team's published keys, for this audience and
  issuer, not expired, for an allowed email. Checking it here, not only at the edge, means a
  request that reaches the server another way still gets no Sage;
- marks the page (`<meta name="celve-signin">`) so the Sage pane shows a **Sign in** button,
  which opens `/signin`; after sign-in Access returns there and the server sends you back to the
  app.

The spreadsheet itself stays open to everyone; only Sage is private.

## How it works

```
Browser tab (WASM: grid + workbook)          celve-server
  Sage pane ──── WebSocket /ws ──────────►  one Sage per tab (assistant process)
  live workbook ◄── ctl {method, params} ──  Sage's tool call → MCP bridge → tab's private port
                ──── ctl.reply ───────────►  … answer (data, or a screenshot for vision)
```

- Sage's tools are Celve's MCP tools. The server runs `celve-server --mcp-bridge <port>` for the
  assistant; the bridge talks to a token-protected loopback port owned by the tab's session, and
  each request is forwarded to the browser, which runs it with the same control methods as the
  desktop control channel (`docs/control-protocol.md`). That's why every change shows up live.
- Only Celve's own page may open a session: the server checks the WebSocket `Origin` against the
  `Host` (plus `CELVE_ALLOWED_ORIGINS`), so other websites can't drive Sage.
- Every Sage session needs the access key (`?key=…`), so no page you happen to visit can use it.
  With a generated key the server is local-only: it also accepts only loopback host names
  (`127.0.0.1`, `localhost`, `[::1]`) and the hosts in `CELVE_ALLOWED_ORIGINS`, which blocks
  DNS-rebinding pages that point their own domain at your machine.
- At most 16 tabs run a Sage at once; closing a tab stops its assistant process.

## Security notes

Sage can read everything in the open workbook and can search and fetch web pages. Text inside a
workbook or picture could try to instruct it; it is told to ask before destructive changes, but
treat workbooks from strangers with care. Anyone who can use Sage on a server uses the server's
assistant account, so keep an access key on any server others can reach.
