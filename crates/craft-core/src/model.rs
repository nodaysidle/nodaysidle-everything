use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use thiserror::Error;
use uuid::Uuid;

pub type RunId = Uuid;

/// Supported recreation target categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetKind {
    Website,
    DesktopApp,
    PhoneApp,
    Game,
    File,
}

impl TargetKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Website => "website",
            Self::DesktopApp => "desktop_app",
            Self::PhoneApp => "phone_app",
            Self::Game => "game",
            Self::File => "file",
        }
    }
}

/// Where the target lives: a URL or a local file/directory path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum TargetLocation {
    Url(String),
    Path(PathBuf),
}

impl TargetLocation {
    pub fn parse(raw: &str) -> Self {
        let trimmed = raw.trim();
        if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
            Self::Url(trimmed.to_string())
        } else {
            Self::Path(PathBuf::from(trimmed))
        }
    }

    pub fn display(&self) -> String {
        match self {
            Self::Url(u) => u.clone(),
            Self::Path(p) => p.display().to_string(),
        }
    }
}

/// Explicit authorization record required before Deep inspection may run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeepAuthorization {
    pub confirmed_by_user: bool,
    pub reason: String,
    pub confirmed_at: DateTime<Utc>,
}

/// Clean Room (default) vs. Deep (opt-in when permitted).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum InspectionDepth {
    #[default]
    CleanRoom,
    Deep {
        authorization: DeepAuthorization,
    },
}

impl InspectionDepth {
    pub fn is_deep(&self) -> bool {
        matches!(self, Self::Deep { .. })
    }

    pub fn validate(&self) -> Result<(), CraftError> {
        if let Self::Deep { authorization } = self
            && !authorization.confirmed_by_user
        {
            return Err(CraftError::DeepModeUnauthorized);
        }
        Ok(())
    }
}

/// Simple Mode (guided progress) vs. Advanced Mode (logs, evidence, inspection tools).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum UiMode {
    #[default]
    Simple,
    Advanced,
}

/// The four-stage user-facing workflow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Study,
    Build,
    Check,
    Ready,
}

impl Stage {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Study => "study",
            Self::Build => "build",
            Self::Check => "check",
            Self::Ready => "ready",
        }
    }
}

/// User request to study and recreate a target.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CraftRequest {
    pub target_kind: TargetKind,
    pub location: TargetLocation,
    pub prompt: String,
    #[serde(default)]
    pub depth: InspectionDepth,
    #[serde(default)]
    pub preferred_agent: Option<String>,
    #[serde(default)]
    pub output_dir: Option<PathBuf>,
}

/// Tuning parameters for the Study -> Build -> Check -> Ready loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunConfig {
    /// Maximum number of Build -> Check refinement passes.
    pub max_iterations: u32,
    /// Minimum parity score (0.0..=1.0) required to transition to Ready.
    pub target_parity_score: f32,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            max_iterations: 3,
            target_parity_score: 0.85,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IterationRecord {
    pub iteration: u32,
    pub parity_score: f32,
    pub summary: String,
    pub gaps: Vec<String>,
    pub completed_at: DateTime<Utc>,
}

/// Lifecycle of a run, independent of which stage it reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    #[default]
    Running,
    Ready,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunState {
    pub id: RunId,
    pub request: CraftRequest,
    pub stage: Stage,
    #[serde(default)]
    pub status: RunStatus,
    pub current_iteration: u32,
    pub latest_parity_score: Option<f32>,
    pub workspace_dir: PathBuf,
    pub iterations: Vec<IterationRecord>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Error)]
pub enum CraftError {
    #[error("deep inspection requires explicit user authorization")]
    DeepModeUnauthorized,

    #[error("run cancelled")]
    Cancelled,

    #[error("invalid target location: {0}")]
    InvalidTarget(String),

    #[error("inspection failed: {0}")]
    InspectionFailed(String),

    #[error("coding agent failed: {0}")]
    AgentFailed(String),

    #[error("comparison failed: {0}")]
    ComparisonFailed(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}
