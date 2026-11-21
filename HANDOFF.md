# HANDOFF.md — Craft Anything

Status handoff for **Craft Anything** (repo `nodaysidle-everything`), as of 2026-11-21.

## 0. Where things stand

- **Done:** milestones M1–M6 are implemented, tested and released. Nothing is in progress.
- **Repo:** `github.com/nodaysidle/nodaysidle-everything`, public, default branch `master`.
- **Release:** `v0.1.1` is the only tag and release. It carries an Arch package, a `.deb`, a tarball and `SHA256SUMS`; the README's install links use `releases/latest/download/…`.
- **Checks:** `cargo test --workspace` passes 58 tests and `cargo clippy --workspace --all-targets -- -D warnings` is clean.
- **Pushing:** a pre-push hook blocks `master` and tags unless the owner says yes for that push.

### What the 2026-11-21 session did, in order

1. Built M1–M6 on top of the scaffold: streaming agent runner, the `craft-browser` crate, all five inspectors, the parity comparator, the web UI, `doctor`, and end-to-end tests.
2. Installed the app per user with an Omarchy Apps menu entry and ran it live from its own window.
3. Ran every target type and four agents live (results below) and fixed what those runs exposed:
   - a hardened browser (Helium) refused plain-`http` pages: plain Chromium is now preferred and blocked addresses are retried over `https`;
   - a web-game folder got no Clean Room capture: it is now rendered;
   - a result checked only by the build read as a match: it is now labelled "build only";
   - agents wasted time screenshotting their own work: the prompt now tells them not to;
   - OpenCode wrote outside the run folder: `PWD` is now set for every agent;
   - Antigravity's log was unreadable: it now has its own parser.
4. Wrote the README, made the repo public, and fixed three defects seen on the rendered GitHub page.
5. Added packaging (`packaging/install.sh`, `PKGBUILD`, `build-deb.sh`), bumped the version to 0.1.1, added `--version`, and published `v0.1.1`.

### Live results (real agents, through the installed app)

| Target | Agent | Result |
|---|---|---|
| Website `info.cern.ch` | `claude` | 100% |
| Website `example.com` | `claude` | 90% |
| Website `news.ycombinator.com` | `codex` | 96% |
| Website `info.cern.ch` | `opencode` | 100% |
| Website `info.cern.ch` | `agy` | 93% |
| File (CSV) | `claude` | 100% |
| Desktop app (open terminal window) | `claude` | 90% |
| Phone app (tablet screen over `adb`) | `claude` | 90% |
| Game (web game folder) | `claude` | 100% |
| Forced second pass | `claude` | 31%, then 93% |

### Not done or not tested

- `gemini` has not been run live (the owner treats `agy` as covering it).
- Not tested: macOS capture, Windows and macOS executables, real `.apk`/`.ipa` files, native games, a native phone app's own screen, and the "build only" label on a real run.
- The Arch package was linted and unpacked but not installed with `pacman`. The `.deb` was installed only on Ubuntu 24.04; it needs glibc 2.39 or newer.
- `.optim-jev/` (the owner's router config) is untracked on purpose.

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
| `craft-agents` | Detects agents, builds headless arguments, streams stdout/stderr line by line, and turns Claude, Codex and Antigravity stream events into readable log events. |
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

### Packages and releases

- `(cd packaging && makepkg -f)` builds the Arch package from the checkout; `packaging/build-deb.sh [out-dir]` builds the `.deb` without `dpkg`.
- The tarball is the stripped release binary plus `packaging/`, `README.md` and `LICENSE` in a `craft-anything-<version>/` folder; its `install.sh` uses the binary beside it instead of building.
- To cut a release: bump `version` in `Cargo.toml` and `pkgver` in `packaging/PKGBUILD`, update the file names in the README's install commands, build the three files, write `SHA256SUMS`, tag, and upload with `gh release`.

Useful environment variables: `CRAFT_BROWSER` (browser binary), `CRAFT_BROWSER_ARGS` (e.g. `--no-sandbox` in containers).
