mod app;
mod binary;
mod capture;
mod file;
mod package;
mod website;

use async_trait::async_trait;
use craft_core::{
    CraftError, CraftEvent, CraftRequest, EventKind, EventSink, EvidenceKind, EvidenceStore,
    InspectorPort, LogLevel, StudyReport, TargetKind,
};
use uuid::Uuid;

/// Where one study writes its evidence and logs. Shared by every target inspector.
pub(crate) struct Probe<'a> {
    pub store: &'a EvidenceStore,
    pub events: &'a EventSink,
    pub run_id: Uuid,
    /// Log source, e.g. `inspect::website`.
    pub source: &'static str,
    /// Ids of evidence recorded so far; becomes `StudyReport::evidence_ids`.
    pub evidence_ids: Vec<String>,
}

impl Probe<'_> {
    pub fn info(&self, message: impl Into<String>) {
        self.events.log(self.run_id, LogLevel::Info, self.source, message);
    }

    /// For skipped probes: say what is missing and what the user can do about it.
    pub fn warn(&self, message: impl Into<String>) {
        self.events.log(self.run_id, LogLevel::Warn, self.source, message);
    }

    /// Stores an artifact and announces it. `clean_room_safe` must be `false` for anything
    /// that exposes internals (bundles, binaries, package contents, network traces).
    pub fn save(
        &mut self,
        id: &str,
        kind: EvidenceKind,
        label: &str,
        file_name: &str,
        bytes: &[u8],
        clean_room_safe: bool,
    ) -> Result<(), CraftError> {
        let item = self
            .store
            .write_artifact(id, kind, label, file_name, bytes, clean_room_safe)?;
        self.events.evidence(self.run_id, &item);
        self.evidence_ids.push(item.id);
        Ok(())
    }
}

/// Dispatches Study requests to the target-specific inspector while enforcing CleanRoom vs Deep.
#[derive(Debug, Default, Clone)]
pub struct CompositeInspector;

#[async_trait]
impl InspectorPort for CompositeInspector {
    async fn study(
        &self,
        request: &CraftRequest,
        store: &EvidenceStore,
        events: &EventSink,
        run_id: Uuid,
    ) -> Result<StudyReport, CraftError> {
        request.depth.validate()?;
        events.emit(CraftEvent::new(
            run_id,
            EventKind::Log {
                level: LogLevel::Info,
                source: "inspect".into(),
                message: format!(
                    "Starting {} inspection ({}) for {}",
                    if request.depth.is_deep() {
                        "deep"
                    } else {
                        "clean-room"
                    },
                    request.target_kind.as_str(),
                    request.location.display()
                ),
            },
        ));

        match request.target_kind {
            TargetKind::Website => website::inspect(request, store, events, run_id).await,
            TargetKind::File => file::inspect(request, store, events, run_id).await,
            TargetKind::DesktopApp | TargetKind::PhoneApp | TargetKind::Game => {
                app::inspect(request, store, events, run_id).await
            }
        }
    }
}
