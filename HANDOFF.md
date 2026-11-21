# HANDOFF.md — Craft Anything

Status handoff for **Craft Anything** (repo `nodaysidle-everything`).
Milestones M1–M6 are implemented (2026-11-21). 

## 1. Product goal

A user points at a **website, desktop app, phone app, game, or file**, describes in plain English what to recreate, and the app **studies** the original, **builds** a new version with the user's own installed coding agent, **checks** parity, and **refines** until close.

Contracts that must keep holding (see `AGENTS.md` for the full rules):

- **Clean Room is the default.** Study records only observable behaviour into `CleanRoomSpec`. Build receives only the spec, the user's prompt, earlier parity gaps, and clean-room-safe reference screenshots.
- **Deep mode is opt-in.** It runs only with `DeepAuthorization { confirmed_by_user: true, .. }`; nothing escalates to it automatically.
- **Simple Mode** shows `study → build → check → ready` and nothing else. **Advanced Mode** is a view over the same `CraftEvent` stream and `EvidenceStore`.
- **The user's own agent** (`claude`, `codex`, `opencode`, `gemini`, `agy`, or a custom command) runs headlessly; no API key is needed.
- **Self-contained Rust.** Optional tools (browser, `adb`, `grim`, `sqlite3`, …) are discovered at run time and their absence only skips a probe, with a message.

## 2. What exists

| Crate | What it does |
|---|---|
| `craft-core` | Domain model, `RunStatus`, `CleanRoomSpec` (with checkable `expect_text` requirements and `reference_images`), `EvidenceStore`, `EventSink` (broadcast + per-run `events.ndjson`), and the `Pipeline` with `execute_with_id` and `CancellationToken`. |
| `craft-browser` | Finds a Chromium-family browser (`CRAFT_BROWSER`, plain Chromium/Chrome on `PATH`, Playwright cache, macOS app paths, then hardened browsers such as Brave or Helium last) and drives it over the DevTools protocol: open at a viewport, evaluate JS, screenshot. |
| `craft-inspect` | Study stage. `website.rs` (screenshots, visible structure, computed tokens; Deep: network, bundles, source maps, frameworks), `file.rs`, `app.rs` (desktop app, phone app, game), with `binary.rs`, `capture.rs`, `package.rs` helpers. |
| `craft-agents` | Detects agents, builds headless arguments, streams stdout/stderr line by line, and turns Claude/Codex NDJSON into readable log events. |
| `craft-compare` | Check stage: build verification, visual similarity + heatmap, spec checklist, combined into a weighted `ParityReport` with ordered gaps. |
| `craft-app` | `craft-anything` binary (`ui`, `run`, `agents`, `doctor`) and the local HTTP + SSE server with the embedded `assets/index.html`. |

### HTTP API (`craft-anything ui`, default `http://127.0.0.1:7420`)

`GET /api/agents`, `GET /api/doctor`, `GET|POST /api/runs`, `GET /api/runs/{id}`, `…/evidence`, `…/events`, `POST …/cancel`, `POST …/open`, `GET …/artifacts/{*path}`, `GET …/output/{*path}`, `GET /api/events` (SSE).

There is deliberately no CORS layer, and on loopback the server rejects foreign `Host` headers: this API starts coding agents. Served artifacts and previews carry `Content-Security-Policy: sandbox allow-scripts`.

### Evidence id conventions (the UI relies on these)

`clean-room-spec`, `deep-findings` (Deep only, not clean-room safe), `screenshot-*` for originals (`screenshot-desktop`, `screenshot-mobile`, `screenshot-N`, `screenshot-device`, `screenshot-frame-N`), and per pass `agent-log-N`, `recreation-N`, `heatmap-N`, `comparison-N`.

### Headless agent invocations

- Claude Code: `claude --output-format stream-json --verbose --permission-mode acceptEdits -p <prompt>`
- Codex: `codex -a never exec --json --skip-git-repo-check --sandbox workspace-write <prompt>`
- OpenCode: `opencode run <prompt>`
- Gemini: `gemini --approval-mode auto_edit -p <prompt>`
- Antigravity: `agy --output-format stream-json --mode accept-edits -p <prompt>`
- Custom: `<command and its arguments> <prompt>`

## 3. How parity is scored

Weights: build 0.3, visual 0.4, checklist 0.3, averaged over the signals that could be measured.

- **Build:** `cargo check`, `npm run build` (only when `node_modules` exists), `node --check`, Python syntax, JSON validity, and missing local files referenced by HTML. Any error caps the score at 50% and is listed first in the gaps.
- **Visual:** the output's `index.html` is rendered at the original screenshot's size and compared block by block (SSIM on brightness, scaled by colour distance, content blocks weighted 4×).
- **Checklist:** every `expect_text` string in the spec must appear in the rendered text or the generated files.

## 4. Known limits

- Visual comparison only renders a static entry page. Output that needs a dev server or a build step is not rendered.
- The checklist is text presence only; it does not exercise behaviour (clicks, forms, game rules).
- Claude, Codex and `agy` have format-specific output parsers; Gemini and OpenCode output is passed through as plain lines (with terminal colour codes stripped). The runner sets `PWD` to the output folder because OpenCode takes its project folder from it.
- Phone capture records whatever is on screen; it never launches the app. Old devices without `adb exec-out` fall back to `adb shell`.
- Desktop/game window capture works on Hyprland (`hyprctl` + `grim`); on macOS it captures the whole screen; other Linux compositors need user-supplied screenshots.
- Binary `Info.plist` files in IPAs yield only a strings sample. The CSV row count counts lines.
- A game folder containing `index.html` (or an `.html` file) is studied by rendering it, through the website inspector.
- When neither a screenshot comparison nor a checklist was possible, `RunState.compared` is false and the run is reported as "build only".
- Verified live on 2026-11-21 with real agents: website, file, desktop app, phone app, game, a forced second pass, and `codex`. `opencode` and `agy` were also run live. Not exercised live: `gemini`, native games, real `.apk`/`.ipa` files.
- The UI's "Pass N of 3" assumes three passes (what the UI sends); `RunState` does not record the pass limit.
- A custom agent command is split on whitespace, so its path cannot contain spaces.

## 5. Commands

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings

cargo run -p craft-app -- doctor
cargo run -p craft-app -- agents
cargo run -p craft-app -- run --target website --source https://example.com --prompt "Recreate this page" --advanced
cargo run -p craft-app -- run --target file --source Cargo.toml --prompt "Recreate workspace manifest" --dry-run
cargo run -p craft-app -- ui        # http://127.0.0.1:7420
```

### Desktop install (Omarchy app menu)

`packaging/install.sh` builds the release binary and installs `craft-anything`, the `craft-anything-app` launcher, a desktop entry and an icon for the current user; `--uninstall` removes them. The launcher starts the server on port 7420 from `~/.local/share/craft-anything` (runs are stored in `runs/` there) and opens the UI as an app window.

Useful environment variables: `CRAFT_BROWSER` (browser binary), `CRAFT_BROWSER_ARGS` (e.g. `--no-sandbox` in containers).
