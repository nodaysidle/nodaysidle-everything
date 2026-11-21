use crate::model::{CraftError, RunId, RunState};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    Screenshot,
    DomSnapshot,
    NetworkTrace,
    FileMetadata,
    BinaryInspection,
    CleanRoomSpec,
    AgentLog,
    ComparisonDiff,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceItem {
    pub id: String,
    pub kind: EvidenceKind,
    pub label: String,
    pub relative_path: PathBuf,
    pub clean_room_safe: bool,
    pub created_at: DateTime<Utc>,
}

/// Filesystem-backed store for a run's state, clean-room spec, and Advanced Mode evidence.
#[derive(Debug, Clone)]
pub struct EvidenceStore {
    root_dir: PathBuf,
}

impl EvidenceStore {
    pub fn open(base_dir: impl AsRef<Path>, run_id: RunId) -> Result<Self, CraftError> {
        let root_dir = base_dir.as_ref().join(run_id.to_string());
        fs::create_dir_all(root_dir.join("artifacts"))?;
        fs::create_dir_all(root_dir.join("output"))?;
        Ok(Self { root_dir })
    }

    /// Opens a run that already exists on disk, without creating anything.
    pub fn open_existing(base_dir: impl AsRef<Path>, run_id: RunId) -> Option<Self> {
        let root_dir = base_dir.as_ref().join(run_id.to_string());
        root_dir
            .join("state.json")
            .is_file()
            .then_some(Self { root_dir })
    }

    pub fn root_dir(&self) -> &Path {
        &self.root_dir
    }

    pub fn output_dir(&self) -> PathBuf {
        self.root_dir.join("output")
    }

    pub fn save_state(&self, state: &RunState) -> Result<(), CraftError> {
        let path = self.root_dir.join("state.json");
        let json = serde_json::to_string_pretty(state)?;
        fs::write(path, json)?;
        Ok(())
    }

    pub fn load_state(&self) -> Result<RunState, CraftError> {
        let path = self.root_dir.join("state.json");
        let raw = fs::read_to_string(path)?;
        Ok(serde_json::from_str(&raw)?)
    }

    pub fn write_artifact(
        &self,
        id: impl Into<String>,
        kind: EvidenceKind,
        label: impl Into<String>,
        file_name: &str,
        bytes: &[u8],
        clean_room_safe: bool,
    ) -> Result<EvidenceItem, CraftError> {
        let relative_path = PathBuf::from("artifacts").join(file_name);
        let full_path = self.root_dir.join(&relative_path);
        fs::write(full_path, bytes)?;

        let item = EvidenceItem {
            id: id.into(),
            kind,
            label: label.into(),
            relative_path,
            clean_room_safe,
            created_at: Utc::now(),
        };

        let mut manifest = self.list_evidence()?;
        manifest.push(item.clone());
        let manifest_path = self.root_dir.join("evidence.json");
        fs::write(manifest_path, serde_json::to_string_pretty(&manifest)?)?;
        Ok(item)
    }

    pub fn list_evidence(&self) -> Result<Vec<EvidenceItem>, CraftError> {
        let manifest_path = self.root_dir.join("evidence.json");
        if !manifest_path.exists() {
            return Ok(Vec::new());
        }
        let raw = fs::read_to_string(manifest_path)?;
        Ok(serde_json::from_str(&raw)?)
    }
}
