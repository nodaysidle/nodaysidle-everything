# AGENTS.md — Craft Anything (`nodaysidle-everything`)

Authoritative engineering rules for **Craft Anything**, a Rust application that studies a target (website, desktop app, phone app, game, or file), builds a recreation via the user's installed coding agent, compares the result, and refines it until close.

## Orientation

- Read [HANDOFF.md](./HANDOFF.md) before starting implementation work for architecture, milestones, and acceptance criteria.
- Workspace layout: `crates/craft-core` (domain, state machine, evidence store), `crates/craft-browser` (headless Chromium discovery + minimal DevTools driver, shared by inspect and compare), `crates/craft-inspect` (target probes), `crates/craft-agents` (coding agent discovery & headless runners), `crates/craft-compare` (parity scoring & diffs), `crates/craft-app` (CLI + local `axum` Web UI).

## Hard Invariants

1. **Clean-room boundary (default mode):** In `InspectionDepth::CleanRoom`, the Study phase records only observable behavior (UI layout, screenshots, user flows, public outputs, file schema/behavior) into `CleanRoomSpec`. The Build phase receives `CleanRoomSpec` + the user's plain-English prompt—never raw proprietary source, decompiled code, or private binaries.
2. **Explicit opt-in for Deep mode:** `InspectionDepth::Deep` runs deeper static/dynamic inspection only when the user explicitly opts in and confirms authorization for the target (`DeepAuthorization`). Never auto-escalate from Clean Room to Deep.
3. **Simple vs. Advanced UX parity:** Simple Mode always presents the four-stage progress flow (`study` → `build` → `check` → `ready`) without exposing raw logs or low-level artifacts. Advanced Mode exposes structured logs, evidence artifacts, comparison diffs, and inspection tools from the same underlying event stream.
4. **User's own coding agent first:** Detect and execute installed local coding agents (`claude`, `codex`, `opencode`, `gemini`, `agy`, or user-configured command) in headless/non-interactive mode. Never require a separate API key when a local coding agent is available.
5. **Pure Rust workspace:** Keep runtime execution self-contained in Rust (`cargo build` / `cargo run`). Optional external tools (browser, `adb`, `objdump`, etc.) must be discovered dynamically and degrade gracefully with actionable messages when absent.

## Coding & Verification Standards

- Edition: Rust 2024. Use typed errors (`thiserror` in libraries, `anyhow` in `craft-app`). Handle every error explicitly; propagate or surface it in run logs.
- Keep `craft-core` free of UI, HTTP, and subprocess concerns so the engine stays testable and UI-agnostic.
- Emit structured `CraftEvent`s at every stage transition, probe execution, agent invocation, and parity check.
- Before claiming work complete, run:
  ```bash
  cargo test --workspace
  cargo clippy --workspace --all-targets -- -D warnings
  ```
