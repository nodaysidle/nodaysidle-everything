use crate::evidence::EvidenceItem;
use crate::model::{RunId, RunStatus, Stage};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;
use tokio::sync::broadcast;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Info,
    Warn,
    Error,
    Debug,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    StageChanged {
        stage: Stage,
        message: String,
    },
    Log {
        level: LogLevel,
        source: String,
        message: String,
    },
    EvidenceAdded {
        evidence_id: String,
        label: String,
    },
    ParityEvaluated {
        iteration: u32,
        score: f32,
        gaps: Vec<String>,
    },
    Finished {
        /// True only when the run completed all stages (it may still be below the parity target).
        ready: bool,
        final_score: f32,
        #[serde(default)]
        status: RunStatus,
        /// Plain-English outcome shown in Simple Mode.
        #[serde(default)]
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CraftEvent {
    pub run_id: RunId,
    pub timestamp: DateTime<Utc>,
    pub kind: EventKind,
}

impl CraftEvent {
    pub fn new(run_id: RunId, kind: EventKind) -> Self {
        Self {
            run_id,
            timestamp: Utc::now(),
            kind,
        }
    }
}

/// Broadcast sink shared between Simple Mode (stage progress) and Advanced Mode (full logs).
#[derive(Debug, Clone)]
pub struct EventSink {
    tx: broadcast::Sender<CraftEvent>,
    persist_dir: Option<PathBuf>,
}

impl EventSink {
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self {
            tx,
            persist_dir: None,
        }
    }

    /// Also append every event to `<runs_dir>/<run_id>/events.ndjson` so past runs can be replayed.
    pub fn persist_to(mut self, runs_dir: impl Into<PathBuf>) -> Self {
        self.persist_dir = Some(runs_dir.into());
        self
    }

    pub fn emit(&self, event: CraftEvent) {
        if let Some(dir) = &self.persist_dir {
            // Best effort: the run directory may not exist yet, and a lost log line must not fail a run.
            let path = dir.join(event.run_id.to_string()).join("events.ndjson");
            if let Ok(line) = serde_json::to_string(&event)
                && let Ok(mut file) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
            {
                let _ = writeln!(file, "{line}");
            }
        }
        let _ = self.tx.send(event);
    }

    pub fn log(&self, run_id: RunId, level: LogLevel, source: &str, message: impl Into<String>) {
        self.emit(CraftEvent::new(
            run_id,
            EventKind::Log {
                level,
                source: source.into(),
                message: message.into(),
            },
        ));
    }

    pub fn evidence(&self, run_id: RunId, item: &EvidenceItem) {
        self.emit(CraftEvent::new(
            run_id,
            EventKind::EvidenceAdded {
                evidence_id: item.id.clone(),
                label: item.label.clone(),
            },
        ));
    }

    pub fn subscribe(&self) -> broadcast::Receiver<CraftEvent> {
        self.tx.subscribe()
    }
}
