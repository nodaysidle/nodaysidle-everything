mod cli;
mod runner;
mod stream;

pub use cli::{AgentBuilder, build_prompt};
pub use runner::{CommandOutput, StreamKind, run_streaming};
pub use stream::summarize_line;

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Local coding agents Craft Anything can auto-detect and drive headlessly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodingAgentKind {
    Claude,
    Codex,
    OpenCode,
    Gemini,
    Agy,
    Custom(String),
}

impl CodingAgentKind {
    pub fn binary_name(&self) -> &str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::OpenCode => "opencode",
            Self::Gemini => "gemini",
            Self::Agy => "agy",
            Self::Custom(cmd) => cmd.as_str(),
        }
    }

    pub fn display_name(&self) -> String {
        match self {
            Self::Claude => "Claude Code".into(),
            Self::Codex => "OpenAI Codex CLI".into(),
            Self::OpenCode => "OpenCode".into(),
            Self::Gemini => "Gemini CLI".into(),
            Self::Agy => "Antigravity CLI (agy)".into(),
            Self::Custom(cmd) => format!("Custom ({cmd})"),
        }
    }

    pub fn from_name(name: &str) -> Self {
        match name.trim().to_ascii_lowercase().as_str() {
            "claude" | "claude-code" => Self::Claude,
            "codex" => Self::Codex,
            "opencode" => Self::OpenCode,
            "gemini" => Self::Gemini,
            "agy" | "antigravity" => Self::Agy,
            // Custom commands keep their original case (paths are case-sensitive).
            _ => Self::Custom(name.trim().to_string()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectedAgent {
    pub kind: CodingAgentKind,
    pub binary: String,
    pub path: PathBuf,
}

/// Scans `PATH` for installed coding agents in default preference order.
pub fn detect_installed_agents() -> Vec<DetectedAgent> {
    let candidates = [
        CodingAgentKind::Claude,
        CodingAgentKind::Codex,
        CodingAgentKind::OpenCode,
        CodingAgentKind::Gemini,
        CodingAgentKind::Agy,
    ];

    candidates
        .into_iter()
        .filter_map(|kind| {
            let binary = kind.binary_name().to_string();
            which::which(&binary).ok().map(|path| DetectedAgent {
                binary,
                kind,
                path,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_agent_names() {
        assert_eq!(CodingAgentKind::from_name("claude"), CodingAgentKind::Claude);
        assert_eq!(CodingAgentKind::from_name("codex"), CodingAgentKind::Codex);
        assert_eq!(
            CodingAgentKind::from_name("opencode"),
            CodingAgentKind::OpenCode
        );
        assert_eq!(CodingAgentKind::from_name("gemini"), CodingAgentKind::Gemini);
        assert_eq!(CodingAgentKind::from_name("agy"), CodingAgentKind::Agy);
        assert_eq!(
            CodingAgentKind::from_name("my-agent"),
            CodingAgentKind::Custom("my-agent".into())
        );
    }
}
