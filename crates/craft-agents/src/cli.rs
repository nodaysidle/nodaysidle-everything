use crate::runner::{StreamKind, run_streaming};
use crate::stream::{strip_ansi, summarize_line};
use crate::{CodingAgentKind, DetectedAgent, detect_installed_agents};
use async_trait::async_trait;
use craft_core::pipeline::{BuildContext, BuildOutcome};
use craft_core::{BuilderPort, CraftError, EventSink, LogLevel};
use std::time::Duration;
use uuid::Uuid;

/// Drives the user's installed coding agent CLI to build or refine the recreation.
#[derive(Debug, Clone)]
pub struct AgentBuilder {
    pub dry_run: bool,
    pub timeout: Duration,
}

impl Default for AgentBuilder {
    fn default() -> Self {
        Self {
            dry_run: false,
            timeout: Duration::from_secs(1800),
        }
    }
}

impl AgentBuilder {
    pub fn select_agent(&self, preferred: Option<&str>) -> Result<DetectedAgent, CraftError> {
        let installed = detect_installed_agents();
        if let Some(pref) = preferred {
            let kind = CodingAgentKind::from_name(pref);
            if let Some(found) = installed.iter().find(|a| a.kind == kind) {
                return Ok(found.clone());
            }
            // A custom command may carry its own arguments ("my-agent --yes"); resolve the program.
            let program = kind.binary_name().split_whitespace().next().unwrap_or("");
            if let Ok(path) = which::which(program) {
                return Ok(DetectedAgent {
                    binary: kind.binary_name().to_string(),
                    kind,
                    path,
                });
            }
            return Err(CraftError::AgentFailed(format!(
                "requested coding agent '{pref}' is not installed or not in PATH"
            )));
        }

        installed.into_iter().next().ok_or_else(|| {
            CraftError::AgentFailed(
                "no local coding agent found in PATH (checked: claude, codex, opencode, gemini, agy). \
                 Install one, or pass a custom agent command."
                    .into(),
            )
        })
    }

    /// Maps each coding agent to its verified non-interactive headless CLI arguments.
    pub fn build_args(kind: &CodingAgentKind, prompt: &str) -> Vec<String> {
        let fixed: &[&str] = match kind {
            // stream-json in print mode requires --verbose.
            CodingAgentKind::Claude => &[
                "--output-format",
                "stream-json",
                "--verbose",
                "--permission-mode",
                "acceptEdits",
                "-p",
            ],
            // The output workspace is usually not a git repository.
            CodingAgentKind::Codex => &[
                "-a",
                "never",
                "exec",
                "--json",
                "--skip-git-repo-check",
                "--sandbox",
                "workspace-write",
            ],
            CodingAgentKind::OpenCode => &["run"],
            CodingAgentKind::Gemini => &["--approval-mode", "auto_edit", "-p"],
            CodingAgentKind::Agy => &[
                "--output-format",
                "stream-json",
                "--mode",
                "accept-edits",
                "-p",
            ],
            CodingAgentKind::Custom(cmd) => {
                return cmd
                    .split_whitespace()
                    .skip(1)
                    .map(String::from)
                    .chain([prompt.to_string()])
                    .collect();
            }
        };
        fixed
            .iter()
            .map(|s| s.to_string())
            .chain([prompt.to_string()])
            .collect()
    }
}

/// Builds the prompt passed to the coding agent, strictly respecting the CleanRoom boundary:
/// only the spec, the user's request, prior parity gaps and (when authorized) deep findings.
pub fn build_prompt(ctx: &BuildContext<'_>) -> String {
    let spec_json = serde_json::to_string_pretty(ctx.spec).unwrap_or_default();
    let mut prompt = format!(
        "You are building a recreation in the current working directory for Craft Anything.\n\
         Pass: {}\n\
         User request: {}\n\n\
         Clean-room behavioral specification (build strictly from this specification):\n\
         ```json\n{}\n```\n\
         Work only inside the current directory. Prefer a result that runs without a build step \
         (for a web page: an index.html that opens directly) unless the request needs otherwise. Do not try to open, render or screenshot your result: \
         Craft Anything renders and checks it after you finish.\n",
        ctx.iteration, ctx.request.prompt, spec_json
    );

    if !ctx.spec.reference_images.is_empty() {
        prompt.push_str(
            "\nReference screenshots of the original are in this directory (do not ship them):\n",
        );
        for image in &ctx.spec.reference_images {
            prompt.push_str(&format!("- {image}\n"));
        }
    }

    if let Some(deep) = ctx.deep_findings {
        let deep_json = serde_json::to_string_pretty(deep).unwrap_or_default();
        prompt.push_str(&format!(
            "\nAuthorized deep inspection findings:\n```json\n{deep_json}\n```\n"
        ));
    }

    if !ctx.previous_gaps.is_empty() {
        prompt.push_str("\nFix these parity gaps from the previous check:\n");
        for gap in ctx.previous_gaps {
            prompt.push_str(&format!("- {gap}\n"));
        }
    }

    prompt
}

#[async_trait]
impl BuilderPort for AgentBuilder {
    async fn build(
        &self,
        ctx: BuildContext<'_>,
        events: &EventSink,
        run_id: Uuid,
    ) -> Result<BuildOutcome, CraftError> {
        let prompt = build_prompt(&ctx);

        if self.dry_run {
            // A dry run never launches an agent, so it must work with none installed.
            let agent_name = self
                .select_agent(ctx.request.preferred_agent.as_deref())
                .map(|a| a.binary)
                .unwrap_or_else(|_| "dry-run".into());
            events.log(
                run_id,
                LogLevel::Info,
                "agents",
                format!("Dry run: writing the prompt for {agent_name} instead of running it"),
            );
            tokio::fs::write(ctx.output_dir.join("CRAFT_DRY_RUN.md"), &prompt).await?;
            return Ok(BuildOutcome {
                agent_name,
                summary: "Dry-run prompt written to output directory".into(),
                raw_log: prompt,
            });
        }

        let agent = self.select_agent(ctx.request.preferred_agent.as_deref())?;
        events.log(
            run_id,
            LogLevel::Info,
            "agents",
            format!(
                "Running {} headlessly ({})",
                agent.kind.display_name(),
                agent.path.display()
            ),
        );

        let args = Self::build_args(&agent.kind, &prompt);
        let output = run_streaming(
            &agent.path,
            &args,
            ctx.output_dir,
            self.timeout,
            |stream, line| match stream {
                StreamKind::Stdout => {
                    for (level, message) in summarize_line(&agent.kind, line) {
                        events.log(run_id, level, "agents", message);
                    }
                }
                StreamKind::Stderr => {
                    let line = strip_ansi(line);
                    if !line.trim().is_empty() {
                        events.log(run_id, LogLevel::Debug, "agents::stderr", line);
                    }
                }
            },
        )
        .await?;

        if !output.success {
            let tail: Vec<&str> = output.stderr.lines().rev().take(5).collect();
            return Err(CraftError::AgentFailed(format!(
                "{} exited with code {:?}: {}",
                agent.binary,
                output.exit_code,
                tail.into_iter().rev().collect::<Vec<_>>().join(" | ")
            )));
        }

        Ok(BuildOutcome {
            agent_name: agent.binary,
            summary: format!(
                "Completed build pass {} with {}",
                ctx.iteration,
                agent.kind.display_name()
            ),
            raw_log: format!(
                "=== STDOUT ===\n{}\n=== STDERR ===\n{}",
                output.stdout, output.stderr
            ),
        })
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use craft_core::{
        CleanRoomSpec, CraftRequest, EventKind, InspectionDepth, TargetKind, TargetLocation,
    };
    use std::os::unix::fs::PermissionsExt;

    /// A custom agent command streams NDJSON while running and its lines reach the event sink live.
    #[tokio::test]
    async fn custom_agent_output_is_streamed_as_log_events() {
        let dir = std::env::temp_dir().join(format!("craft-agent-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("fake-agent.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\necho '{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"writing page\"}]}}'\n\
             echo '<h1>hi</h1>' > index.html\necho 'plain progress'\necho 'warn' >&2\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let request = CraftRequest {
            target_kind: TargetKind::Website,
            location: TargetLocation::parse("https://example.com"),
            prompt: "Recreate it".into(),
            depth: InspectionDepth::CleanRoom,
            preferred_agent: Some(script.display().to_string()),
            output_dir: None,
        };
        let spec = CleanRoomSpec::new(TargetKind::Website, "x".into(), "y".into());
        let events = EventSink::new(64);
        let mut rx = events.subscribe();

        let outcome = AgentBuilder::default()
            .build(
                BuildContext {
                    request: &request,
                    spec: &spec,
                    deep_findings: None,
                    iteration: 1,
                    previous_gaps: &[],
                    output_dir: &dir,
                },
                &events,
                Uuid::new_v4(),
            )
            .await
            .expect("fake agent should succeed");

        assert!(dir.join("index.html").exists());
        assert!(outcome.raw_log.contains("plain progress"));
        let mut logged = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let EventKind::Log { message, .. } = ev.kind {
                logged.push(message);
            }
        }
        assert!(logged.contains(&"writing page".to_string()), "{logged:?}");
        assert!(logged.contains(&"plain progress".to_string()), "{logged:?}");
        assert!(logged.contains(&"warn".to_string()), "{logged:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn headless_args_end_with_the_prompt() {
        for kind in [
            CodingAgentKind::Claude,
            CodingAgentKind::Codex,
            CodingAgentKind::OpenCode,
            CodingAgentKind::Gemini,
            CodingAgentKind::Agy,
            CodingAgentKind::Custom("my-agent --yes".into()),
        ] {
            let args = AgentBuilder::build_args(&kind, "PROMPT");
            assert_eq!(args.last().map(String::as_str), Some("PROMPT"), "{kind:?}");
        }
        assert_eq!(
            AgentBuilder::build_args(&CodingAgentKind::Custom("my-agent --yes".into()), "P"),
            vec!["--yes", "P"]
        );
    }
}
