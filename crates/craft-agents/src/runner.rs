use craft_core::CraftError;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub struct CommandOutput {
    pub success: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    Stdout,
    Stderr,
}

/// Terminates the agent's whole process group when the run is dropped (cancel or timeout),
/// so helper processes spawned by wrapper scripts do not outlive it.
struct GroupGuard(Option<u32>);

impl Drop for GroupGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.0 {
            // shortcut: shells out to `kill` instead of taking a libc dependency for killpg.
            let _ = std::process::Command::new("kill")
                .args(["-TERM", "--", &format!("-{pid}")])
                .stderr(Stdio::null())
                .status();
        }
    }
}

async fn pump(
    reader: impl AsyncRead + Unpin,
    kind: StreamKind,
    tx: mpsc::UnboundedSender<(StreamKind, String)>,
) {
    let mut reader = BufReader::new(reader);
    let mut buf = Vec::new();
    // read_until + lossy decode: agent output is not guaranteed to be valid UTF-8.
    while let Ok(n) = reader.read_until(b'\n', &mut buf).await {
        if n == 0 {
            break;
        }
        let line = String::from_utf8_lossy(&buf).trim_end().to_string();
        buf.clear();
        if tx.send((kind, line)).is_err() {
            break;
        }
    }
}

/// Spawns the command and calls `on_line` for every stdout/stderr line as it arrives.
pub async fn run_streaming(
    program: &Path,
    args: &[String],
    cwd: &Path,
    timeout: Duration,
    mut on_line: impl FnMut(StreamKind, &str) + Send,
) -> Result<CommandOutput, CraftError> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(cwd)
        // Some agents (OpenCode) take their project folder from $PWD, not the real cwd.
        .env("PWD", cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = cmd.spawn().map_err(|e| {
        CraftError::AgentFailed(format!("failed to spawn {}: {e}", program.display()))
    })?;
    let mut guard = GroupGuard(child.id());

    let (tx, mut rx) = mpsc::unbounded_channel();
    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(pump(stdout, StreamKind::Stdout, tx.clone()));
    }
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(pump(stderr, StreamKind::Stderr, tx.clone()));
    }
    drop(tx);

    let mut stdout = String::new();
    let mut stderr = String::new();
    let run = async {
        while let Some((kind, line)) = rx.recv().await {
            on_line(kind, &line);
            let buf = match kind {
                StreamKind::Stdout => &mut stdout,
                StreamKind::Stderr => &mut stderr,
            };
            buf.push_str(&line);
            buf.push('\n');
        }
        child.wait().await
    };

    let status = tokio::time::timeout(timeout, run)
        .await
        .map_err(|_| {
            CraftError::AgentFailed(format!(
                "coding agent timed out after {}s",
                timeout.as_secs()
            ))
        })?
        .map_err(|e| CraftError::AgentFailed(format!("failed waiting for agent: {e}")))?;
    guard.0 = None;

    Ok(CommandOutput {
        success: status.success(),
        exit_code: status.code(),
        stdout,
        stderr,
    })
}
