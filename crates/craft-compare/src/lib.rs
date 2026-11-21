//! The Check stage: measures how close the recreation is to the original and turns the
//! differences into concrete gaps for the next Build pass.
//!
//! Three independent signals are combined: build/run verification, visual similarity against
//! the original screenshot, and the clean-room spec checklist. The comparator only ever reads
//! the clean-room spec and clean-room-safe evidence, so its gaps are safe to hand to the builder.

mod behavioral;
mod build_check;
mod visual;

use async_trait::async_trait;
use craft_browser::{Browser, Viewport, file_url};
use craft_core::pipeline::ParityReport;
use craft_core::{
    ComparatorPort, CraftError, EventSink, EvidenceKind, EvidenceStore, LogLevel, StudyReport,
};
use serde::Serialize;
use std::path::Path;
use uuid::Uuid;

const BUILD_WEIGHT: f32 = 0.3;
const VISUAL_WEIGHT: f32 = 0.4;
const CHECKLIST_WEIGHT: f32 = 0.3;
/// A recreation that does not build cannot be "close", whatever else matches.
const BROKEN_BUILD_CAP: f32 = 0.5;
const MAX_GAPS: usize = 20;

#[derive(Debug, Serialize)]
struct Component {
    score: f32,
    weight: f32,
    detail: String,
}

#[derive(Debug, Default, Serialize)]
struct Components {
    build: Option<Component>,
    visual: Option<Component>,
    checklist: Option<Component>,
}

#[derive(Debug, Serialize)]
struct ComparisonArtifact {
    iteration: u32,
    score: f32,
    generated_files: usize,
    components: Components,
    gaps: Vec<String>,
}

/// Parity comparator used by the pipeline's Check stage.
#[derive(Debug, Default, Clone)]
pub struct DefaultComparator;

struct Rendered {
    screenshot: Vec<u8>,
    text: String,
}

/// Renders the recreation's entry page at the given size.
async fn render(entry: &Path, width: u32, height: u32) -> Result<Rendered, String> {
    let mut browser = Browser::launch().await.map_err(|e| e.to_string())?;
    let viewport = Viewport {
        width,
        height,
        mobile: false,
    };
    browser
        .open(&file_url(entry), viewport)
        .await
        .map_err(|e| e.to_string())?;
    let screenshot = browser.screenshot().await.map_err(|e| e.to_string())?;
    let text = browser
        .eval("document.body ? document.body.innerText : ''")
        .await
        .map_err(|e| e.to_string())?;
    Ok(Rendered {
        screenshot,
        text: text.as_str().unwrap_or("").to_string(),
    })
}

#[async_trait]
impl ComparatorPort for DefaultComparator {
    async fn compare(
        &self,
        study: &StudyReport,
        output_dir: &Path,
        iteration: u32,
        store: &EvidenceStore,
        events: &EventSink,
        run_id: Uuid,
    ) -> Result<ParityReport, CraftError> {
        let log = |level, message: String| events.log(run_id, level, "compare", message);
        let files = behavioral::output_files(output_dir);
        let mut components = Components::default();
        let mut gaps: Vec<String> = Vec::new();

        if files.is_empty() {
            gaps.push("Nothing was generated in the output directory yet.".into());
        }

        // 1. Build / run verification.
        let build = build_check::check(output_dir, &files).await;
        for skipped in &build.skipped {
            log(LogLevel::Warn, format!("Build check: {skipped}"));
        }
        if !build.kinds.is_empty() {
            let detail = format!(
                "{}: {}",
                build.kinds.join(" + "),
                match build.errors.len() {
                    0 => "no problems found".to_string(),
                    n => format!("{n} problem(s)"),
                }
            );
            log(LogLevel::Info, format!("Build check — {detail}"));
            components.build = Some(Component {
                score: if build.errors.is_empty() { 1.0 } else { 0.0 },
                weight: BUILD_WEIGHT,
                detail,
            });
        }
        // Compiler and runtime errors come first: nothing else matters until they are fixed.
        gaps.extend(build.errors.iter().map(|e| format!("Build error: {e}")));

        // 2. Visual parity against the original screenshot (clean-room-safe evidence only).
        let original = store
            .list_evidence()?
            .into_iter()
            .filter(|e| {
                e.kind == EvidenceKind::Screenshot
                    && e.clean_room_safe
                    && study.evidence_ids.contains(&e.id)
            })
            .min_by_key(|e| e.id != "screenshot-desktop")
            .and_then(|e| std::fs::read(store.root_dir().join(&e.relative_path)).ok());
        let mut text = behavioral::collect_text(output_dir, &files);

        if let Some(entry) = build_check::entry_page(output_dir, &files) {
            let (width, height) = original
                .as_deref()
                .and_then(visual::dimensions)
                .map_or((1280, 800), |(w, h)| (w.clamp(320, 1920), h.clamp(320, 1600)));
            match render(&entry, width, height).await {
                Ok(rendered) => {
                    let item = store.write_artifact(
                        format!("recreation-{iteration}"),
                        EvidenceKind::Screenshot,
                        format!("Recreation screenshot (pass {iteration})"),
                        &format!("recreation_pass_{iteration}.png"),
                        &rendered.screenshot,
                        true,
                    )?;
                    events.evidence(run_id, &item);
                    text.push_str(&rendered.text);

                    if let Some(original) = &original {
                        match visual::diff(original, &rendered.screenshot) {
                            Ok(diff) => {
                                let item = store.write_artifact(
                                    format!("heatmap-{iteration}"),
                                    EvidenceKind::ComparisonDiff,
                                    format!("Visual difference heatmap (pass {iteration})"),
                                    &format!("heatmap_pass_{iteration}.png"),
                                    &diff.heatmap_png,
                                    true,
                                )?;
                                events.evidence(run_id, &item);
                                let detail = format!(
                                    "{:.0}% similar to the original screenshot; largest difference: {}",
                                    diff.score * 100.0,
                                    diff.worst_region
                                );
                                log(LogLevel::Info, format!("Visual check — {detail}"));
                                if diff.score < 0.95 {
                                    gaps.push(format!(
                                        "Visual: the rendered page is only {:.0}% similar to the original \
                                         screenshot. The largest differences are in the {} area; compare \
                                         layout, spacing, colours and sizes there against the reference.",
                                        diff.score * 100.0,
                                        diff.worst_region
                                    ));
                                }
                                components.visual = Some(Component {
                                    score: diff.score,
                                    weight: VISUAL_WEIGHT,
                                    detail,
                                });
                            }
                            Err(err) => log(LogLevel::Warn, format!("Visual check skipped: {err}")),
                        }
                    }
                }
                Err(err) => log(LogLevel::Warn, format!("Visual check skipped: {err}")),
            }
        } else if original.is_some() && !files.is_empty() {
            log(
                LogLevel::Warn,
                "Visual check skipped: the output has no index.html to render".into(),
            );
            gaps.push(
                "Visual: nothing could be rendered for comparison; provide an index.html entry page."
                    .into(),
            );
        }

        // 3. Spec checklist.
        let checklist = behavioral::check(&study.clean_room_spec, &text);
        if checklist.total > 0 && !files.is_empty() {
            let detail = format!(
                "{} of {} expected visible items found",
                checklist.found, checklist.total
            );
            log(LogLevel::Info, format!("Spec checklist — {detail}"));
            components.checklist = Some(Component {
                score: checklist.found as f32 / checklist.total as f32,
                weight: CHECKLIST_WEIGHT,
                detail,
            });
            gaps.extend(checklist.gaps);
        }

        // 4. Weighted score over the signals that could actually be measured.
        let measured: Vec<&Component> = [&components.build, &components.visual, &components.checklist]
            .into_iter()
            .flatten()
            .collect();
        let weight: f32 = measured.iter().map(|c| c.weight).sum();
        let mut score = if files.is_empty() || weight == 0.0 {
            0.0
        } else {
            measured.iter().map(|c| c.score * c.weight).sum::<f32>() / weight
        };
        if !build.errors.is_empty() {
            score = score.min(BROKEN_BUILD_CAP);
        }
        if !files.is_empty() && measured.is_empty() {
            log(
                LogLevel::Warn,
                "Nothing in the output could be verified automatically; parity is reported as 0%".into(),
            );
            gaps.push(
                "The output could not be verified automatically (no buildable project, renderable \
                 page or checkable requirement)."
                    .into(),
            );
        }
        gaps.truncate(MAX_GAPS);

        let part = |name: &str, c: &Option<Component>| {
            c.as_ref()
                .map_or(format!("{name} n/a"), |c| format!("{name} {:.0}%", c.score * 100.0))
        };
        let summary = format!(
            "Pass {iteration}: {:.0}% parity ({}, {}, {}) across {} file(s)",
            score * 100.0,
            part("build", &components.build),
            part("visual", &components.visual),
            part("checklist", &components.checklist),
            files.len()
        );
        log(LogLevel::Info, summary.clone());

        let artifact = ComparisonArtifact {
            iteration,
            score,
            generated_files: files.len(),
            components,
            gaps: gaps.clone(),
        };
        let item = store.write_artifact(
            format!("comparison-{iteration}"),
            EvidenceKind::ComparisonDiff,
            format!("Parity comparison report (pass {iteration})"),
            &format!("comparison_pass_{iteration}.json"),
            &serde_json::to_vec_pretty(&artifact)?,
            true,
        )?;
        events.evidence(run_id, &item);

        Ok(ParityReport {
            compared: artifact.components.visual.is_some()
                || artifact.components.checklist.is_some()
                || files.is_empty(),
            score,
            summary,
            gaps,
        })
    }
}
