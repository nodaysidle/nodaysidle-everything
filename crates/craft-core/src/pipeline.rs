use crate::events::{CraftEvent, EventKind, EventSink, LogLevel};
use crate::evidence::{EvidenceKind, EvidenceStore};
use crate::model::{
    CraftError, CraftRequest, InspectionDepth, IterationRecord, RunConfig, RunState, RunStatus,
    Stage,
};
use crate::spec::{CleanRoomSpec, DeepFindings, StudyReport};
use async_trait::async_trait;
use chrono::Utc;
use std::path::Path;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Folder inside the build workspace holding clean-room-safe reference screenshots.
pub const REFERENCE_DIR: &str = ".craft-reference";

#[derive(Debug, Clone)]
pub struct BuildContext<'a> {
    pub request: &'a CraftRequest,
    pub spec: &'a CleanRoomSpec,
    /// Only `Some` when `InspectionDepth::Deep` was explicitly authorized.
    pub deep_findings: Option<&'a DeepFindings>,
    pub iteration: u32,
    pub previous_gaps: &'a [String],
    pub output_dir: &'a Path,
}

#[derive(Debug, Clone)]
pub struct BuildOutcome {
    pub agent_name: String,
    pub summary: String,
    pub raw_log: String,
}

#[derive(Debug, Clone)]
pub struct ParityReport {
    pub score: f32,
    pub summary: String,
    pub gaps: Vec<String>,
}

#[async_trait]
pub trait InspectorPort: Send + Sync {
    async fn study(
        &self,
        request: &CraftRequest,
        store: &EvidenceStore,
        events: &EventSink,
        run_id: Uuid,
    ) -> Result<StudyReport, CraftError>;
}

#[async_trait]
pub trait BuilderPort: Send + Sync {
    async fn build(
        &self,
        ctx: BuildContext<'_>,
        events: &EventSink,
        run_id: Uuid,
    ) -> Result<BuildOutcome, CraftError>;
}

#[async_trait]
pub trait ComparatorPort: Send + Sync {
    async fn compare(
        &self,
        study: &StudyReport,
        output_dir: &Path,
        iteration: u32,
        store: &EvidenceStore,
        events: &EventSink,
        run_id: Uuid,
    ) -> Result<ParityReport, CraftError>;
}

#[derive(Debug, Clone)]
pub struct PipelineResult {
    pub state: RunState,
    pub study: StudyReport,
}

/// Orchestrates the `study -> build -> check -> ready` loop with clean-room enforcement.
pub struct Pipeline<I, B, C> {
    inspector: I,
    builder: B,
    comparator: C,
    config: RunConfig,
}

impl<I, B, C> Pipeline<I, B, C>
where
    I: InspectorPort,
    B: BuilderPort,
    C: ComparatorPort,
{
    pub fn new(inspector: I, builder: B, comparator: C, config: RunConfig) -> Self {
        Self {
            inspector,
            builder,
            comparator,
            config,
        }
    }

    pub async fn execute(
        &self,
        request: CraftRequest,
        runs_base_dir: &Path,
        events: &EventSink,
    ) -> Result<PipelineResult, CraftError> {
        self.execute_with_id(
            Uuid::new_v4(),
            request,
            runs_base_dir,
            events,
            CancellationToken::new(),
        )
        .await
    }

    /// Runs the loop under a caller-chosen id. Cancelling `cancel` drops the stage in flight
    /// (subprocesses are killed on drop) and finishes the run as `Cancelled`.
    pub async fn execute_with_id(
        &self,
        run_id: Uuid,
        request: CraftRequest,
        runs_base_dir: &Path,
        events: &EventSink,
        cancel: CancellationToken,
    ) -> Result<PipelineResult, CraftError> {
        request.depth.validate()?;

        let store = EvidenceStore::open(runs_base_dir, run_id)?;
        let output_dir = request
            .output_dir
            .clone()
            .unwrap_or_else(|| store.output_dir());
        std::fs::create_dir_all(&output_dir)?;

        let now = Utc::now();
        let mut state = RunState {
            id: run_id,
            request: request.clone(),
            stage: Stage::Study,
            status: RunStatus::Running,
            current_iteration: 0,
            latest_parity_score: None,
            workspace_dir: output_dir.clone(),
            iterations: Vec::new(),
            error: None,
            created_at: now,
            updated_at: now,
        };
        store.save_state(&state)?;

        let outcome = tokio::select! {
            result = self.run_stages(&request, &store, &output_dir, &mut state, events) => result,
            () = cancel.cancelled() => Err(CraftError::Cancelled),
        };

        let final_score = state.latest_parity_score.unwrap_or(0.0);
        let target = self.config.target_parity_score;
        let passes = state.iterations.len();
        let (status, message) = match &outcome {
            Ok(_) if final_score >= target => (
                RunStatus::Ready,
                format!("Ready: {:.0}% match with the original", final_score * 100.0),
            ),
            Ok(_) => (
                RunStatus::Ready,
                format!(
                    "Ready: best result is a {:.0}% match after {passes} pass(es) (target {:.0}%)",
                    final_score * 100.0,
                    target * 100.0
                ),
            ),
            Err(CraftError::Cancelled) => (RunStatus::Cancelled, "Cancelled".to_string()),
            Err(err) => (RunStatus::Failed, format!("Failed: {err}")),
        };

        state.status = status;
        state.updated_at = Utc::now();
        match status {
            RunStatus::Ready => {
                state.stage = Stage::Ready;
                events.emit(CraftEvent::new(
                    run_id,
                    EventKind::StageChanged {
                        stage: Stage::Ready,
                        message: message.clone(),
                    },
                ));
            }
            RunStatus::Failed => {
                state.error = Some(message.clone());
                events.log(run_id, LogLevel::Error, "pipeline", message.clone());
            }
            _ => events.log(run_id, LogLevel::Warn, "pipeline", message.clone()),
        }
        store.save_state(&state)?;
        events.emit(CraftEvent::new(
            run_id,
            EventKind::Finished {
                ready: status == RunStatus::Ready,
                final_score,
                status,
                message,
            },
        ));

        outcome.map(|study| PipelineResult { state, study })
    }

    async fn run_stages(
        &self,
        request: &CraftRequest,
        store: &EvidenceStore,
        output_dir: &Path,
        state: &mut RunState,
        events: &EventSink,
    ) -> Result<StudyReport, CraftError> {
        let run_id = state.id;

        // 1. STUDY
        events.emit(CraftEvent::new(
            run_id,
            EventKind::StageChanged {
                stage: Stage::Study,
                message: format!("Studying {} target", request.target_kind.as_str()),
            },
        ));
        let mut study = self.inspector.study(request, store, events, run_id).await?;

        // Clean-room boundary: only screenshots flagged clean-room-safe reach the build workspace.
        for item in store.list_evidence()? {
            if item.kind == EvidenceKind::Screenshot
                && item.clean_room_safe
                && study.evidence_ids.contains(&item.id)
                && let Some(name) = item.relative_path.file_name()
            {
                let reference_dir = output_dir.join(REFERENCE_DIR);
                std::fs::create_dir_all(&reference_dir)?;
                std::fs::copy(
                    store.root_dir().join(&item.relative_path),
                    reference_dir.join(name),
                )?;
                study
                    .clean_room_spec
                    .reference_images
                    .push(format!("{REFERENCE_DIR}/{}", name.to_string_lossy()));
            }
        }

        let spec_bytes = serde_json::to_vec_pretty(&study.clean_room_spec)?;
        let spec_item = store.write_artifact(
            "clean-room-spec",
            EvidenceKind::CleanRoomSpec,
            "Clean-room behavioral specification",
            "clean_room_spec.json",
            &spec_bytes,
            true,
        )?;
        events.evidence(run_id, &spec_item);

        // Enforce clean-room boundary: never pass deep_findings unless Deep mode was authorized.
        let deep_for_builder = match request.depth {
            InspectionDepth::CleanRoom => None,
            InspectionDepth::Deep { .. } => study.deep_findings.as_ref(),
        };
        if let Some(deep) = deep_for_builder {
            let item = store.write_artifact(
                "deep-findings",
                EvidenceKind::BinaryInspection,
                "Deep inspection findings (authorized)",
                "deep_findings.json",
                &serde_json::to_vec_pretty(deep)?,
                false,
            )?;
            events.evidence(run_id, &item);
        }

        let mut previous_gaps: Vec<String> = Vec::new();

        // 2 & 3. BUILD -> CHECK refinement loop
        for iteration in 1..=self.config.max_iterations.max(1) {
            state.stage = Stage::Build;
            state.current_iteration = iteration;
            state.updated_at = Utc::now();
            store.save_state(state)?;

            events.emit(CraftEvent::new(
                run_id,
                EventKind::StageChanged {
                    stage: Stage::Build,
                    message: format!("Building recreation (pass {iteration})"),
                },
            ));

            let build_outcome = self
                .builder
                .build(
                    BuildContext {
                        request,
                        spec: &study.clean_room_spec,
                        deep_findings: deep_for_builder,
                        iteration,
                        previous_gaps: &previous_gaps,
                        output_dir,
                    },
                    events,
                    run_id,
                )
                .await?;

            let log_item = store.write_artifact(
                format!("agent-log-{iteration}"),
                EvidenceKind::AgentLog,
                format!("{} build log (pass {iteration})", build_outcome.agent_name),
                &format!("build_pass_{iteration}.log"),
                build_outcome.raw_log.as_bytes(),
                true,
            )?;
            events.evidence(run_id, &log_item);

            state.stage = Stage::Check;
            state.updated_at = Utc::now();
            store.save_state(state)?;

            events.emit(CraftEvent::new(
                run_id,
                EventKind::StageChanged {
                    stage: Stage::Check,
                    message: format!("Checking parity against original (pass {iteration})"),
                },
            ));

            let parity = self
                .comparator
                .compare(&study, output_dir, iteration, store, events, run_id)
                .await?;
            state.latest_parity_score = Some(parity.score);
            state.iterations.push(IterationRecord {
                iteration,
                parity_score: parity.score,
                summary: parity.summary.clone(),
                gaps: parity.gaps.clone(),
                completed_at: Utc::now(),
            });
            state.updated_at = Utc::now();
            store.save_state(state)?;

            events.emit(CraftEvent::new(
                run_id,
                EventKind::ParityEvaluated {
                    iteration,
                    score: parity.score,
                    gaps: parity.gaps.clone(),
                },
            ));

            if parity.score >= self.config.target_parity_score {
                events.log(
                    run_id,
                    LogLevel::Info,
                    "pipeline",
                    format!(
                        "Reached parity threshold ({:.0}% >= {:.0}%) on pass {iteration}",
                        parity.score * 100.0,
                        self.config.target_parity_score * 100.0
                    ),
                );
                break;
            }

            previous_gaps = parity.gaps;
        }

        Ok(study)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DeepAuthorization, TargetKind, TargetLocation};
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct MockInspector;
    #[async_trait]
    impl InspectorPort for MockInspector {
        async fn study(
            &self,
            request: &CraftRequest,
            _store: &EvidenceStore,
            _events: &EventSink,
            _run_id: Uuid,
        ) -> Result<StudyReport, CraftError> {
            Ok(StudyReport {
                clean_room_spec: CleanRoomSpec {
                    target_kind: request.target_kind,
                    target_label: request.location.display(),
                    overview: "Mock target".into(),
                    observed_features: vec!["Feature A".into()],
                    ui_structure: vec!["Main view".into()],
                    visual_tokens: BTreeMap::new(),
                    requirements: vec![],
                    open_questions: vec![],
                    reference_images: vec![],
                },
                deep_findings: Some(DeepFindings {
                    notes: vec!["Sensitive deep note".into()],
                    ..Default::default()
                }),
                evidence_ids: vec![],
            })
        }
    }

    struct MockBuilder {
        saw_deep_findings: Arc<AtomicBool>,
    }
    #[async_trait]
    impl BuilderPort for MockBuilder {
        async fn build(
            &self,
            ctx: BuildContext<'_>,
            _events: &EventSink,
            _run_id: Uuid,
        ) -> Result<BuildOutcome, CraftError> {
            if ctx.deep_findings.is_some() {
                self.saw_deep_findings.store(true, Ordering::SeqCst);
            }
            Ok(BuildOutcome {
                agent_name: "mock-agent".into(),
                summary: format!("built pass {}", ctx.iteration),
                raw_log: "ok".into(),
            })
        }
    }

    struct MockComparator;
    #[async_trait]
    impl ComparatorPort for MockComparator {
        async fn compare(
            &self,
            _study: &StudyReport,
            _output_dir: &Path,
            iteration: u32,
            _store: &EvidenceStore,
            _events: &EventSink,
            _run_id: Uuid,
        ) -> Result<ParityReport, CraftError> {
            if iteration == 1 {
                Ok(ParityReport {
                    score: 0.60,
                    summary: "Needs refinement".into(),
                    gaps: vec!["Header spacing off".into()],
                })
            } else {
                Ok(ParityReport {
                    score: 0.92,
                    summary: "Close match".into(),
                    gaps: vec![],
                })
            }
        }
    }

    #[tokio::test]
    async fn clean_room_hides_deep_findings_and_refines_until_ready() {
        let saw_deep = Arc::new(AtomicBool::new(false));
        let pipeline = Pipeline::new(
            MockInspector,
            MockBuilder {
                saw_deep_findings: saw_deep.clone(),
            },
            MockComparator,
            RunConfig {
                max_iterations: 3,
                target_parity_score: 0.85,
            },
        );

        let temp_dir = std::env::temp_dir().join(format!("craft-test-{}", Uuid::new_v4()));
        let events = EventSink::new(32);

        let result = pipeline
            .execute(
                CraftRequest {
                    target_kind: TargetKind::Website,
                    location: TargetLocation::parse("https://example.com"),
                    prompt: "Recreate the landing page".into(),
                    depth: InspectionDepth::CleanRoom,
                    preferred_agent: None,
                    output_dir: None,
                },
                &temp_dir,
                &events,
            )
            .await
            .expect("pipeline should succeed");

        assert_eq!(result.state.stage, Stage::Ready);
        assert_eq!(result.state.iterations.len(), 2);
        assert!(!saw_deep.load(Ordering::SeqCst));
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn deep_mode_requires_explicit_user_confirmation() {
        let pipeline = Pipeline::new(
            MockInspector,
            MockBuilder {
                saw_deep_findings: Arc::new(AtomicBool::new(false)),
            },
            MockComparator,
            RunConfig::default(),
        );
        let temp_dir = std::env::temp_dir().join(format!("craft-test-{}", Uuid::new_v4()));
        let events = EventSink::new(8);

        let err = pipeline
            .execute(
                CraftRequest {
                    target_kind: TargetKind::File,
                    location: TargetLocation::parse("/tmp/sample.bin"),
                    prompt: "Rebuild parser".into(),
                    depth: InspectionDepth::Deep {
                        authorization: DeepAuthorization {
                            confirmed_by_user: false,
                            reason: "unconfirmed".into(),
                            confirmed_at: Utc::now(),
                        },
                    },
                    preferred_agent: None,
                    output_dir: None,
                },
                &temp_dir,
                &events,
            )
            .await
            .unwrap_err();

        assert!(matches!(err, CraftError::DeepModeUnauthorized));
    }

    struct SlowBuilder;
    #[async_trait]
    impl BuilderPort for SlowBuilder {
        async fn build(
            &self,
            _ctx: BuildContext<'_>,
            _events: &EventSink,
            _run_id: Uuid,
        ) -> Result<BuildOutcome, CraftError> {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            unreachable!("the build must be dropped on cancel")
        }
    }

    fn request(depth: InspectionDepth) -> CraftRequest {
        CraftRequest {
            target_kind: TargetKind::Website,
            location: TargetLocation::parse("https://example.com"),
            prompt: "Recreate the landing page".into(),
            depth,
            preferred_agent: None,
            output_dir: None,
        }
    }

    #[tokio::test]
    async fn cancelling_stops_the_stage_in_flight_and_records_it() {
        let pipeline = Pipeline::new(MockInspector, SlowBuilder, MockComparator, RunConfig::default());
        let temp_dir = std::env::temp_dir().join(format!("craft-test-{}", Uuid::new_v4()));
        let events = EventSink::new(32).persist_to(&temp_dir);
        let mut rx = events.subscribe();
        let cancel = CancellationToken::new();
        let run_id = Uuid::new_v4();

        let canceller = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            canceller.cancel();
        });
        let err = pipeline
            .execute_with_id(run_id, request(InspectionDepth::CleanRoom), &temp_dir, &events, cancel)
            .await
            .unwrap_err();
        assert!(matches!(err, CraftError::Cancelled));

        let state = EvidenceStore::open_existing(&temp_dir, run_id)
            .expect("run exists on disk")
            .load_state()
            .unwrap();
        assert_eq!(state.status, RunStatus::Cancelled);
        assert_eq!(state.stage, Stage::Build);

        let mut last = None;
        while let Ok(ev) = rx.try_recv() {
            last = Some(ev.kind);
        }
        assert!(matches!(
            last,
            Some(EventKind::Finished { ready: false, status: RunStatus::Cancelled, .. })
        ));
        // Every event was also persisted for later replay.
        let log = std::fs::read_to_string(temp_dir.join(run_id.to_string()).join("events.ndjson")).unwrap();
        assert!(log.lines().count() >= 3 && log.contains("\"cancelled\""));
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn authorized_deep_mode_passes_findings_to_the_builder_and_stores_them() {
        let saw_deep = Arc::new(AtomicBool::new(false));
        let pipeline = Pipeline::new(
            MockInspector,
            MockBuilder { saw_deep_findings: saw_deep.clone() },
            MockComparator,
            RunConfig::default(),
        );
        let temp_dir = std::env::temp_dir().join(format!("craft-test-{}", Uuid::new_v4()));
        let depth = InspectionDepth::Deep {
            authorization: DeepAuthorization {
                confirmed_by_user: true,
                reason: "my own site".into(),
                confirmed_at: Utc::now(),
            },
        };
        let result = pipeline
            .execute(request(depth), &temp_dir, &EventSink::new(32))
            .await
            .unwrap();
        assert!(saw_deep.load(Ordering::SeqCst));
        assert_eq!(result.state.status, RunStatus::Ready);

        let store = EvidenceStore::open_existing(&temp_dir, result.state.id).unwrap();
        let deep = store
            .list_evidence()
            .unwrap()
            .into_iter()
            .find(|e| e.id == "deep-findings")
            .expect("deep findings stored as evidence");
        assert!(!deep.clean_room_safe);
        let _ = std::fs::remove_dir_all(temp_dir);
    }
}
