//! `craft-anything doctor`: reports which coding agents and optional tools are available.

use craft_agents::CodingAgentKind;
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct Check {
    pub category: &'static str,
    pub name: String,
    pub ok: bool,
    /// Where it was found, or what to do about it.
    pub detail: String,
    pub purpose: &'static str,
}

/// Optional external tools: (binary, what Craft Anything uses it for).
const TOOLS: &[(&str, &str)] = &[
    ("adb", "Phone app study: device screenshots and UI hierarchy"),
    ("grim", "Desktop app / game study: window capture on Wayland"),
    ("hyprctl", "Desktop app / game study: locating the window on Hyprland"),
    ("screencapture", "Desktop app / game study: screen capture on macOS"),
    ("sqlite3", "File study: reading the schema of SQLite databases"),
    ("node", "Check stage: syntax and build checks for JavaScript output"),
    ("npm", "Check stage: running the build script of Node projects"),
    ("python3", "Check stage: compile checks for Python output"),
    ("cargo", "Check stage: build checks for Rust output"),
    ("objdump", "Optional: binaries are parsed in-process, objdump is only a manual aid"),
];

pub async fn collect() -> Vec<Check> {
    let mut checks = Vec::new();

    for kind in [
        CodingAgentKind::Claude,
        CodingAgentKind::Codex,
        CodingAgentKind::OpenCode,
        CodingAgentKind::Gemini,
        CodingAgentKind::Agy,
    ] {
        let found = which::which(kind.binary_name());
        checks.push(Check {
            category: "agent",
            name: kind.binary_name().to_string(),
            ok: found.is_ok(),
            detail: match found {
                Ok(path) => path.display().to_string(),
                Err(_) => format!("not in PATH ({} is optional)", kind.display_name()),
            },
            purpose: "Build stage: one local coding agent is enough",
        });
    }

    // Launching proves the browser actually works headlessly, not just that a file exists.
    let browser = match craft_browser::Browser::launch().await {
        Ok(mut browser) => {
            let version = browser.version().await.unwrap_or_else(|e| e.to_string());
            Ok(format!("{} ({version})", browser.binary.display()))
        }
        Err(err) => Err(err.to_string()),
    };
    checks.push(Check {
        category: "browser",
        name: "headless browser".into(),
        ok: browser.is_ok(),
        detail: browser.unwrap_or_else(|e| e),
        purpose: "Website study and visual parity checks",
    });

    for (tool, purpose) in TOOLS {
        let found = which::which(tool);
        checks.push(Check {
            category: "tool",
            name: tool.to_string(),
            ok: found.is_ok(),
            detail: match found {
                Ok(path) => path.display().to_string(),
                Err(_) => "not found (optional; the related probe is skipped)".into(),
            },
            purpose,
        });
    }
    checks
}

pub fn print(checks: &[Check]) {
    for (category, title) in [
        ("agent", "Coding agents"),
        ("browser", "Headless browser"),
        ("tool", "Optional tools"),
    ] {
        println!("{title}");
        for check in checks.iter().filter(|c| c.category == category) {
            println!(
                "  [{}] {:<17} {}",
                if check.ok { "ok" } else { "--" },
                check.name,
                check.detail
            );
            if !check.ok {
                println!("       needed for: {}", check.purpose);
            }
        }
        println!();
    }

    let agents = checks.iter().filter(|c| c.category == "agent" && c.ok).count();
    if agents == 0 {
        println!("No coding agent found: install claude, codex, opencode, gemini or agy,");
        println!("or pass your own command with --agent. Dry runs work without one.");
    } else {
        println!("{agents} coding agent(s) ready. Clean Room mode is the default;");
        println!("Deep mode needs --deep --confirm-authorized on each run.");
    }
}
