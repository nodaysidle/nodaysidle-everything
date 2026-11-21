use crate::model::TargetKind;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A single observable requirement extracted during Study.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BehavioralRequirement {
    pub id: String,
    pub area: String,
    pub description: String,
    pub acceptance_signal: String,
    /// Visible strings the recreation must show; the Check stage verifies these mechanically.
    #[serde(default)]
    pub expect_text: Vec<String>,
}

impl BehavioralRequirement {
    pub fn new(id: impl Into<String>, area: &str, description: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            area: area.into(),
            description: description.into(),
            acceptance_signal: String::new(),
            expect_text: Vec::new(),
        }
    }

    /// Makes the requirement mechanically checkable: these strings must be visible in the result.
    pub fn expecting(mut self, texts: impl IntoIterator<Item = String>) -> Self {
        self.expect_text = texts.into_iter().collect();
        self.acceptance_signal = format!("Recreation visibly shows: {}", self.expect_text.join(", "));
        self
    }

    pub fn signal(mut self, acceptance_signal: &str) -> Self {
        self.acceptance_signal = acceptance_signal.into();
        self
    }
}

/// Clean-room behavioral specification derived strictly from visible/observable behavior.
///
/// In `InspectionDepth::CleanRoom`, ONLY this structure (plus the user's plain-English prompt)
/// is passed to the Builder coding agent—never raw proprietary source or decompiled code.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CleanRoomSpec {
    pub target_kind: TargetKind,
    pub target_label: String,
    pub overview: String,
    pub observed_features: Vec<String>,
    pub ui_structure: Vec<String>,
    pub visual_tokens: BTreeMap<String, String>,
    pub requirements: Vec<BehavioralRequirement>,
    pub open_questions: Vec<String>,
    /// Clean-room-safe screenshots copied into the build workspace (paths relative to it).
    #[serde(default)]
    pub reference_images: Vec<String>,
}

impl CleanRoomSpec {
    pub fn new(target_kind: TargetKind, target_label: String, overview: String) -> Self {
        Self {
            target_kind,
            target_label,
            overview,
            observed_features: Vec::new(),
            ui_structure: Vec::new(),
            visual_tokens: BTreeMap::new(),
            requirements: Vec::new(),
            open_questions: Vec::new(),
            reference_images: Vec::new(),
        }
    }
}

/// Additional findings collected only in `InspectionDepth::Deep` when explicitly authorized.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeepFindings {
    pub format_details: BTreeMap<String, String>,
    pub extracted_symbols: Vec<String>,
    pub assets_discovered: Vec<String>,
    pub notes: Vec<String>,
}

/// Combined output of the Study stage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StudyReport {
    pub clean_room_spec: CleanRoomSpec,
    /// Populated only when `InspectionDepth::Deep` was explicitly authorized.
    pub deep_findings: Option<DeepFindings>,
    pub evidence_ids: Vec<String>,
}
