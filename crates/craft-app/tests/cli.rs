//! End-to-end tests of the `craft-anything` binary: the real inspector, agent builder and
//! comparator wired together, driven the way a user drives them.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Marker that exists only inside the binary fixture's bytes, never in what a user can observe.
const INTERNAL_MARKER: &str = "CRAFT_INTERNAL_SYMBOL_9731";

struct Sandbox(PathBuf);

impl Sandbox {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("craft-cli-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.path(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    /// Runs `craft-anything --runs-dir <sandbox>/runs <args>`.
    fn craft(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_craft-anything"))
            .arg("--runs-dir")
            .arg(self.path("runs"))
            .args(args)
            .output()
            .expect("binary runs")
    }

    fn binary_fixture(&self) -> PathBuf {
        let mut bytes = vec![0u8, 1, 2, 3, 0xff, 0xfe];
        bytes.extend_from_slice(INTERNAL_MARKER.as_bytes());
        bytes.extend_from_slice(&[0, 0, 7, 0xff]);
        self.write("blob.bin", &bytes)
    }

    /// The single run's directory under `runs/`.
    fn run_dir(&self) -> PathBuf {
        let mut runs: Vec<PathBuf> = std::fs::read_dir(self.path("runs"))
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .collect();
        assert_eq!(runs.len(), 1, "expected exactly one run");
        runs.remove(0)
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn read_json(path: &Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

/// Clean Room is the default: the builder's prompt carries the spec and nothing deeper.
#[test]
fn clean_room_prompt_never_contains_deep_findings() {
    let s = Sandbox::new();
    let source = s.binary_fixture();
    let out = s.craft(&[
        "run", "--target", "file", "--source", source.to_str().unwrap(),
        "--prompt", "Recreate a reader for this format", "--dry-run", "--max-iterations", "1",
    ]);
    assert!(out.status.success(), "{}", text(&out.stderr));

    let run = s.run_dir();
    let state = read_json(&run.join("state.json"));
    assert_eq!(state["request"]["depth"]["mode"], "clean_room");
    assert_eq!(state["status"], "ready");

    let prompt = std::fs::read_to_string(run.join("output/CRAFT_DRY_RUN.md")).unwrap();
    assert!(prompt.contains("Clean-room behavioral specification"));
    assert!(prompt.contains("Recreate a reader for this format"));
    assert!(!prompt.contains("Authorized deep inspection findings"));
    assert!(!prompt.contains(INTERNAL_MARKER), "internal bytes leaked into the builder prompt");

    let spec = std::fs::read_to_string(run.join("artifacts/clean_room_spec.json")).unwrap();
    assert!(!spec.contains(INTERNAL_MARKER), "internal bytes leaked into the clean-room spec");
    let evidence = read_json(&run.join("evidence.json"));
    assert!(evidence.as_array().unwrap().iter().all(|e| e["clean_room_safe"] == true));
}

/// Deep mode needs both flags; `--deep` alone is refused before anything is inspected.
#[test]
fn deep_mode_requires_explicit_confirmation() {
    let s = Sandbox::new();
    let source = s.binary_fixture();
    let base = [
        "run", "--target", "file", "--source", source.to_str().unwrap(),
        "--prompt", "Recreate a reader", "--dry-run", "--max-iterations", "1", "--deep",
    ];

    let refused = s.craft(&base);
    assert!(!refused.status.success());
    assert!(text(&refused.stderr).contains("explicit user authorization"), "{}", text(&refused.stderr));
    assert!(!s.path("runs").exists() || std::fs::read_dir(s.path("runs")).unwrap().next().is_none());

    let mut confirmed = base.to_vec();
    confirmed.push("--confirm-authorized");
    let out = s.craft(&confirmed);
    assert!(out.status.success(), "{}", text(&out.stderr));

    let run = s.run_dir();
    let prompt = std::fs::read_to_string(run.join("output/CRAFT_DRY_RUN.md")).unwrap();
    assert!(prompt.contains("Authorized deep inspection findings"));
    assert!(prompt.contains(INTERNAL_MARKER), "authorized deep findings should reach the builder");
    // Even in Deep mode the clean-room spec itself stays clean.
    let spec = std::fs::read_to_string(run.join("artifacts/clean_room_spec.json")).unwrap();
    assert!(!spec.contains(INTERNAL_MARKER));
    let evidence = read_json(&run.join("evidence.json"));
    let deep = evidence
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == "deep-findings")
        .expect("deep findings stored");
    assert_eq!(deep["clean_room_safe"], false);
}

/// The whole loop with a real (fake) agent process: study -> build -> check -> ready.
#[cfg(unix)]
#[test]
fn full_loop_with_a_custom_agent_reaches_ready() {
    use std::os::unix::fs::PermissionsExt;
    let s = Sandbox::new();
    let page = "<!doctype html><title>Tiny</title><body style=\"margin:0;font-family:sans-serif\">\
                <header style=\"background:#246;color:#fff;padding:20px\"><nav><a href=\"#a\">Home</a> <a href=\"#b\">About</a></nav></header>\
                <h1>Tiny Fixture Site</h1><button>Join now</button></body>";
    let original = s.write("original.html", page.as_bytes());
    s.write("rebuilt.html", page.as_bytes());
    // A stand-in coding agent: reports progress as NDJSON and writes the page into its cwd.
    let agent = s.write(
        "agent.sh",
        format!(
            "#!/bin/sh\necho '{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"building the page\"}}]}}}}'\n\
             test -n \"$1\" || exit 3\ncp '{}' index.html\n",
            s.path("rebuilt.html").display()
        )
        .as_bytes(),
    );
    std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();

    let out = s.craft(&[
        "run", "--target", "website", "--source", original.to_str().unwrap(),
        "--prompt", "Recreate this page", "--agent", agent.to_str().unwrap(), "--advanced",
    ]);
    let stdout = text(&out.stdout);
    assert!(out.status.success(), "{stdout}\n{}", text(&out.stderr));
    let order: Vec<usize> = ["[STUDY]", "[BUILD]", "[CHECK]", "[READY]"]
        .iter()
        .map(|stage| stdout.find(stage).unwrap_or_else(|| panic!("{stage} missing:\n{stdout}")))
        .collect();
    assert!(order.windows(2).all(|w| w[0] < w[1]), "{stdout}");
    assert!(stdout.contains("building the page"), "agent output was not streamed:\n{stdout}");

    let run = s.run_dir();
    assert!(run.join("output/index.html").is_file());
    let state = read_json(&run.join("state.json"));
    assert_eq!(state["status"], "ready");
    // With a browser the page is rendered and compared; an identical page converges in one pass.
    if craft_browser::find_browser().is_some() {
        assert_eq!(state["iterations"].as_array().unwrap().len(), 1, "{stdout}");
        assert!(state["latest_parity_score"].as_f64().unwrap() > 0.9, "{stdout}");
        assert!(run.join("artifacts/heatmap_pass_1.png").is_file());
        assert!(run.join("output/.craft-reference/original_desktop.png").is_file());
    }
}

#[test]
fn doctor_reports_agents_browser_and_tools() {
    let s = Sandbox::new();
    let out = s.craft(&["doctor"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    for section in ["Coding agents", "Headless browser", "Optional tools", "claude", "adb"] {
        assert!(stdout.contains(section), "{section} missing:\n{stdout}");
    }
}
