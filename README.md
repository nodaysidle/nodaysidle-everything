<p align="center">
  <img src="packaging/craft-anything.svg" width="128" height="128" alt="Craft Anything icon">
</p>

<h1 align="center">Craft Anything</h1>

![Craft Anything running on Omarchy after recreating a website at a 100% match](docs/assets/hero.png)

<p align="center">
  <strong>Point at a website, desktop app, phone app, game or file, say what you want in plain English,<br>and let your own coding agent rebuild it until it matches.</strong><br>
  Study → Build → Check → Ready. Clean-room by default, local-first, written in Rust.
</p>

<p align="center">
  <img alt="Rust 2024" src="https://img.shields.io/badge/Rust-2024-000000?style=flat-square&logo=rust&logoColor=white">
  <img alt="Arch, Wayland, Hyprland" src="https://img.shields.io/badge/Arch%20%2F%20Wayland%20%2F%20Hyprland-native-1f2328?style=flat-square&logo=archlinux&logoColor=1793d1">
  <img alt="Clean Room by default" src="https://img.shields.io/badge/Clean%20Room-default-2ecc71?style=flat-square">
  <img alt="No API key" src="https://img.shields.io/badge/API%20key-not%20needed-4c8c6b?style=flat-square">
  <img alt="Zero telemetry" src="https://img.shields.io/badge/telemetry-none-4c8c6b?style=flat-square">
  <img alt="License MIT" src="https://img.shields.io/badge/license-MIT-blue?style=flat-square">
</p>

<p align="center">
  <a href="#what-it-is">What it is</a> ·
  <a href="#verified-proof">Verified proof</a> ·
  <a href="#how-it-works">How it works</a> ·
  <a href="#clean-room-and-deep-mode">Clean Room and Deep</a> ·
  <a href="#install">Install</a> ·
  <a href="#use-it">Use it</a> ·
  <a href="#how-parity-is-scored">Scoring</a> ·
  <a href="#limits">Limits</a>
</p>

---

## What it is

Craft Anything recreates things you can see.

1. **Study.** It looks at the original the way a user would: screenshots at desktop and phone size, the visible structure, colours and fonts for a website; the public shape of a file; a window capture of an app or game; the current screen of a connected phone.
2. **Build.** It writes what it observed into a clean-room specification and hands that, with your request, to a coding agent you already have installed: `claude`, `codex`, `opencode`, `gemini`, `agy`, or any command of your own. The agent runs headlessly in an output folder. No extra API key is involved.
3. **Check.** It verifies the result builds, renders it, compares it with the original screenshot, and ticks off what the specification says must be visible.
4. **Ready.** If the match is below the target, the differences go back to the agent as concrete gaps and it tries again, up to the pass limit.

Two views sit on the same run:

- **Simple Mode** shows only the four steps, a plain status line and a match meter.
- **Advanced Mode** adds the live log, every captured artifact, a side-by-side comparison with a difference heatmap, the specification, the raw agent log and the tools check.

## Verified proof

A real run on 2026-11-21, started from the installed app's own window on Omarchy, with the `claude` agent, in Clean Room mode:

| | |
|---|---|
| Target | `http://info.cern.ch` (the first website) |
| Request | "Recreate this page faithfully as a single static index.html" |
| Result | Ready in one pass: build 100%, visual 100%, checklist 100% |

The original (blue border) above the page the agent built from the specification (green border):

![Original screenshot above the recreated page; they are identical](docs/assets/proof-original-vs-recreation.png)

Every target type went through a full live run that day:

| Target | Agent | Result |
|---|---|---|
| Website `info.cern.ch` | `claude` | 100% |
| Website `example.com` | `claude` | 90% |
| Website `news.ycombinator.com` | `codex` | 96% |
| File (a CSV) | `claude` | 100%; the script it wrote runs and totals correctly |
| Desktop app (an open terminal window) | `claude` | 90% |
| Phone app (a connected tablet's screen) | `claude` | 90% |
| Game (a small web game folder) | `claude` | 100% |
| Forced second pass (rough draft first) | `claude` | 31% on pass 1, 93% after the gaps were fixed on pass 2 |

Advanced Mode shows where an original and its recreation differ, here for `example.com`:

![Advanced Mode comparison: original, recreation and difference heatmap with component scores](docs/assets/advanced-compare.png)

The repository's checks at that commit: `cargo test --workspace` passes 56 tests and `cargo clippy --workspace --all-targets -- -D warnings` is clean. The tests include an end-to-end run proving that bytes internal to a binary never reach a Clean Room prompt, and that Deep mode is refused without confirmation.

## How it works

```mermaid
flowchart LR
    T[Target<br>website · app · phone · game · file] --> S
    subgraph Study
      S[Inspectors] --> E[(Evidence store)]
      S --> SPEC[Clean-room spec]
    end
    SPEC --> B
    P[Your plain-English request] --> B
    subgraph Build
      B[Your coding agent<br>headless, in the output folder]
    end
    B --> O[Recreation]
    O --> C
    E --> C
    subgraph Check
      C[Build check · visual diff · spec checklist]
    end
    C -- "below target: gaps" --> B
    C -- "at target or out of passes" --> R[Ready]
```

| Crate | Role |
|---|---|
| `craft-core` | Domain model, clean-room spec, evidence store, event stream, and the pipeline with run ids and cancellation |
| `craft-browser` | Finds a Chromium-family browser and drives it headlessly over the DevTools protocol |
| `craft-inspect` | Study stage for websites, files, desktop apps, phone apps and games |
| `craft-agents` | Detects coding agents, runs them headlessly, and streams their output line by line |
| `craft-compare` | Check stage: build verification, visual similarity with a heatmap, spec checklist |
| `craft-app` | The `craft-anything` binary and the local web UI |

Every stage emits structured events. Simple Mode, Advanced Mode and the terminal output are three readers of that one stream, and each run's events and artifacts are kept on disk so past runs can be reopened.

## Clean Room and Deep mode

**Clean Room is the default.** The Study stage records only what can be observed from outside. The coding agent receives the specification, your request, the gaps from the previous check, and screenshots of the original. It never receives source code, bundles, decompiled code or the contents of a binary.

**Deep mode is opt-in, per run.** It adds network requests, script bundles, source maps and framework fingerprints for websites; headers, sections, imports and strings for binaries; manifests and resource listings for `.apk` and `.ipa` files; engine detection and an asset catalogue for games. It runs only when you switch it on *and* confirm you are authorized to inspect the target: a dialog in the UI, `--deep --confirm-authorized` on the command line. Nothing escalates to it automatically, and its findings are stored separately and marked as not clean-room safe.

Use Deep mode only on things you own or have permission to inspect.

## Install

Requirements: Rust and Cargo, a Chromium-family browser (Chromium, Chrome, or `npx playwright install chromium`), and at least one coding agent for real runs.

```bash
git clone https://github.com/nodaysidle/nodaysidle-everything.git
cd nodaysidle-everything
packaging/install.sh
```

That builds the release binary and installs, for your user only:

- `~/.local/bin/craft-anything`, the program
- `~/.local/bin/craft-anything-app`, a launcher that starts the local server and opens the UI as an app window
- an application entry and icon, so **Craft Anything** appears in the Omarchy Apps menu

Runs are stored in `~/.local/share/craft-anything/runs`. Remove everything except that data with `packaging/install.sh --uninstall`.

Check what is available on your machine:

```bash
craft-anything doctor
```

Everything beyond the browser and one agent is optional and discovered at run time. A missing tool skips its probe with a message; it never fails a run.

| Tool | Used for |
|---|---|
| `adb` | Phone apps: device screenshot and visible UI tree |
| `grim` + `hyprctl` | Desktop apps and games: capturing the app's window on Hyprland |
| `screencapture` | The same on macOS (whole screen) |
| `sqlite3` | Reading the schema of SQLite files |
| `node`, `npm`, `python3`, `cargo` | Checking that what the agent built is well-formed |

## Use it

**From the app menu:** open *Craft Anything*, pick a target type, enter the URL or path, describe what you want, press **Start**.

**From the terminal:**

```bash
# Recreate a website with the first agent found
craft-anything run --target website --source https://example.com \
  --prompt "Recreate this page as a single static index.html"

# Pick the agent and see the full log
craft-anything run --target website --source https://example.com \
  --prompt "Recreate this page" --agent codex --advanced

# A desktop app from screenshots you took
craft-anything run --target desktop-app --source ~/Pictures/my-app-shots \
  --prompt "Rebuild this settings window as a web page"

# See the prompt the agent would get, without running it
craft-anything run --target file --source data.csv --prompt "Write a parser for this" --dry-run

# Deep mode, on something you are authorized to inspect
craft-anything run --target file --source ./my-tool --prompt "Document this binary" \
  --deep --confirm-authorized

# Start the web UI without the launcher
craft-anything ui            # http://127.0.0.1:7420
```

What each target accepts:

| Target | Source |
|---|---|
| Website | A URL, or a local `.html` file |
| Desktop app | An executable, `.desktop` file or app bundle (its open window is captured), or a folder of screenshots |
| Phone app | A package name or `.apk`/`.ipa` (the connected phone's current screen is captured; open the app first), or a folder of screenshots |
| Game | A game folder or executable (three frames of its open window), a web game by URL or as a folder with an `index.html` (rendered), or screenshots |
| File | Any file or folder: JSON, CSV, SQLite, zip, tar, images, text |

## How parity is scored

| Signal | Weight | What it measures |
|---|---|---|
| Build | 0.3 | `cargo check`, `npm run build` when dependencies are installed, JavaScript and Python syntax, JSON validity, and missing files referenced by HTML |
| Visual | 0.4 | The output's `index.html` rendered at the original screenshot's size and compared block by block; a heatmap shows where they differ |
| Checklist | 0.3 | Every piece of text the specification says must be visible is looked for in the result |

The score is the weighted average of the signals that could be measured. When only the build could be checked, the result is labelled "build only" rather than shown as a match. A build error caps it at 50% and is listed first in the gaps. The default target is 85%, with up to three passes.

## Safety of the local server

The web UI's server can start coding agents, so it is deliberately closed: it listens on `127.0.0.1`, has no cross-origin access, refuses requests addressed to any other host name, and serves agent-built previews in a sandboxed origin so their scripts cannot call it.

## Limits

- The visual check renders a static entry page. Output that needs a dev server or a build step is not rendered.
- The checklist confirms that expected text is present. It does not click through behaviour.
- Agent output is parsed into readable lines for Claude Code and Codex; other agents' output is shown as plain lines.
- Window capture works on Hyprland; on macOS it captures the whole screen; elsewhere, pass screenshots.
- Phone capture records whatever is on the screen and never launches or changes anything on the device.
- By default the output folder lives under the runs folder. If that is inside another project, your agent will pick up that project's instructions; use `--output-dir` to choose somewhere else.
- Not yet tested: macOS, Windows and macOS executables, and real `.apk`/`.ipa` files beyond test fixtures.

## Development

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

`AGENTS.md` holds the engineering rules; `HANDOFF.md` holds the current state and the HTTP API.

## License

MIT. See [LICENSE](LICENSE).
