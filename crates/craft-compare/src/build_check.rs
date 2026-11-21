//! Build/run verification: detect what kind of project the agent produced and confirm it is
//! well-formed, using whichever toolchains are installed.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

pub struct BuildReport {
    /// Detected project kinds, e.g. `["static site", "node project"]`. Empty: nothing checkable.
    pub kinds: Vec<&'static str>,
    /// Compiler, syntax or missing-file errors: high-priority gaps.
    pub errors: Vec<String>,
    /// Checks that could not run (toolchain not installed).
    pub skipped: Vec<String>,
}

/// Runs a tool with a timeout; `None` when it is not installed. Returns (success, output tail).
async fn run(tool: &str, args: &[&str], dir: &Path, secs: u64) -> Option<(bool, String)> {
    let program = which::which(tool).ok()?;
    let child = Command::new(program)
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    Some(match tokio::time::timeout(Duration::from_secs(secs), child).await {
        Ok(Ok(out)) => {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stderr),
                String::from_utf8_lossy(&out.stdout)
            );
            let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
            let tail = lines[lines.len().saturating_sub(12)..].join("\n");
            (out.status.success(), tail)
        }
        Ok(Err(e)) => (false, format!("could not run {tool}: {e}")),
        Err(_) => (false, format!("{tool} did not finish within {secs}s")),
    })
}

fn with_ext<'a>(files: &'a [PathBuf], exts: &'a [&str]) -> impl Iterator<Item = &'a PathBuf> {
    files.iter().filter(move |f| {
        f.extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| exts.contains(&e.to_ascii_lowercase().as_str()))
    })
}

/// Local files an HTML document references through `src=` / `href=` that do not exist.
fn missing_references(dir: &Path, html_file: &Path, html: &str) -> Vec<String> {
    let mut missing = Vec::new();
    for attr in ["src=", "href="] {
        for part in html.split(attr).skip(1) {
            let Some(quote) = part.chars().next().filter(|c| *c == '"' || *c == '\'') else {
                continue;
            };
            let Some(value) = part[1..].split(quote).next() else { continue };
            let target = value.split(['?', '#']).next().unwrap_or("").trim();
            let external = target.is_empty()
                || target.starts_with("//")
                || target.contains(':')
                || target.contains("{{")
                || target.contains("${");
            if external {
                continue;
            }
            let base = if target.starts_with('/') {
                dir.to_path_buf()
            } else {
                dir.join(html_file).parent().unwrap_or(dir).to_path_buf()
            };
            if !base.join(target.trim_start_matches('/')).exists() {
                missing.push(format!(
                    "{} references missing file \"{target}\"",
                    html_file.display()
                ));
            }
        }
    }
    missing.sort();
    missing.dedup();
    missing
}

/// `files` are the generated files relative to `dir`.
pub async fn check(dir: &Path, files: &[PathBuf]) -> BuildReport {
    let mut report = BuildReport {
        kinds: Vec::new(),
        errors: Vec::new(),
        skipped: Vec::new(),
    };
    let has = |name: &str| dir.join(name).is_file();
    let read = |file: &Path| std::fs::read_to_string(dir.join(file)).unwrap_or_default();

    if has("Cargo.toml") {
        report.kinds.push("rust project");
        match run("cargo", &["check", "--quiet", "--message-format", "short"], dir, 600).await {
            Some((true, _)) => {}
            Some((false, tail)) => report.errors.push(format!("cargo check failed:\n{tail}")),
            None => report.skipped.push("cargo is not installed; Rust build not verified".into()),
        }
    }

    if has("package.json") {
        report.kinds.push("node project");
        match serde_json::from_str::<serde_json::Value>(&read(Path::new("package.json"))) {
            Err(e) => report.errors.push(format!("package.json is not valid JSON: {e}")),
            Ok(manifest) if manifest.pointer("/scripts/build").is_some() => {
                if !dir.join("node_modules").is_dir() {
                    report.skipped.push(
                        "dependencies are not installed (no node_modules); `npm run build` not verified".into(),
                    );
                } else {
                    match run("npm", &["run", "build", "--silent"], dir, 600).await {
                        Some((true, _)) => {}
                        Some((false, tail)) => {
                            report.errors.push(format!("npm run build failed:\n{tail}"));
                        }
                        None => report.skipped.push("npm is not installed; build not verified".into()),
                    }
                }
            }
            Ok(_) => {}
        }
    }

    let html: Vec<&PathBuf> = with_ext(files, &["html", "htm"]).collect();
    if !html.is_empty() {
        report.kinds.push("static site");
        for file in html.iter().take(40) {
            report.errors.extend(missing_references(dir, file, &read(file)));
        }
    }

    // Plain scripts only: JSX/TS need a compiler the project may not have installed.
    let scripts: Vec<&PathBuf> = with_ext(files, &["js", "mjs", "cjs"]).take(60).collect();
    if !scripts.is_empty() {
        if report.kinds.is_empty() {
            report.kinds.push("javascript");
        }
        let mut node_missing = false;
        for script in scripts {
            match run("node", &["--check", &script.to_string_lossy()], dir, 30).await {
                Some((true, _)) => {}
                Some((false, tail)) => {
                    report.errors.push(format!("{} has a syntax error:\n{tail}", script.display()));
                }
                None => node_missing = true,
            }
        }
        if node_missing {
            report.skipped.push("node is not installed; JavaScript syntax not verified".into());
        }
    }

    let python: Vec<String> = with_ext(files, &["py"])
        .take(100)
        .map(|f| f.to_string_lossy().into_owned())
        .collect();
    if !python.is_empty() {
        report.kinds.push("python project");
        // ast.parse checks syntax without writing __pycache__ into the output.
        let script = "import ast,sys\nfor f in sys.argv[1:]:\n    ast.parse(open(f, encoding='utf-8').read(), f)";
        let mut args = vec!["-c", script];
        args.extend(python.iter().map(String::as_str));
        match run("python3", &args, dir, 60).await {
            Some((true, _)) => {}
            Some((false, tail)) => report.errors.push(format!("Python syntax check failed:\n{tail}")),
            None => report.skipped.push("python3 is not installed; Python syntax not verified".into()),
        }
    }

    for file in with_ext(files, &["json"]).take(100) {
        if file != Path::new("package.json")
            && let Err(e) = serde_json::from_str::<serde_json::Value>(&read(file))
        {
            if report.kinds.is_empty() {
                report.kinds.push("data files");
            }
            report.errors.push(format!("{} is not valid JSON: {e}", file.display()));
        }
    }

    report
}

/// The page to render for visual comparison, if the output has one.
pub fn entry_page(dir: &Path, files: &[PathBuf]) -> Option<PathBuf> {
    ["index.html", "dist/index.html", "build/index.html", "public/index.html", "out/index.html"]
        .into_iter()
        .map(|candidate| dir.join(candidate))
        .find(|p| p.is_file())
        .or_else(|| {
            // A single stray page is unambiguous; several are not.
            let mut pages = with_ext(files, &["html", "htm"]);
            match (pages.next(), pages.next()) {
                (Some(only), None) => Some(dir.join(only)),
                _ => None,
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::behavioral::output_files;

    fn project(files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("craft-build-test-{}", uuid::Uuid::new_v4()));
        for (name, contents) in files {
            let path = dir.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
        dir
    }

    async fn check_dir(dir: &Path) -> BuildReport {
        check(dir, &output_files(dir)).await
    }

    #[tokio::test]
    async fn static_site_with_all_assets_passes_and_missing_assets_are_reported() {
        let good = project(&[
            ("index.html", r##"<link href="css/site.css?v=2" rel="stylesheet"><script src="app.js"></script><a href="https://example.com">x</a><a href="#top">t</a><img src="data:image/png;base64,AA">"##),
            ("css/site.css", "body{}"),
            ("app.js", "const a = 1;"),
        ]);
        let report = check_dir(&good).await;
        assert!(report.kinds.contains(&"static site"));
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(entry_page(&good, &output_files(&good)), Some(good.join("index.html")));

        let bad = project(&[("index.html", r#"<script src="missing.js"></script><img src='/img/logo.png'>"#)]);
        let report = check_dir(&bad).await;
        assert_eq!(report.errors.len(), 2, "{:?}", report.errors);
        assert!(report.errors.iter().any(|e| e.contains("missing.js")));
        for dir in [good, bad] {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[tokio::test]
    async fn broken_sources_become_errors_when_the_toolchain_exists() {
        let dir = project(&[
            ("package.json", "{ not json"),
            ("main.js", "function ( {"),
            ("tool.py", "def broken(:\n  pass"),
            ("data/config.json", "{\"a\": }"),
        ]);
        let report = check_dir(&dir).await;
        let all = report.errors.join("\n");
        assert!(all.contains("package.json is not valid JSON"), "{all}");
        assert!(all.contains("config.json is not valid JSON"), "{all}");
        if which::which("node").is_ok() {
            assert!(all.contains("main.js has a syntax error"), "{all}");
        }
        if which::which("python3").is_ok() {
            assert!(all.contains("Python syntax check failed"), "{all}");
            assert!(!dir.join("__pycache__").exists());
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn output_without_a_recognizable_project_is_not_penalized() {
        let dir = project(&[("notes.txt", "hello"), ("CRAFT_DRY_RUN.md", "prompt")]);
        let files = output_files(&dir);
        assert_eq!(files, vec![PathBuf::from("notes.txt")]);
        let report = check(&dir, &files).await;
        assert!(report.kinds.is_empty() && report.errors.is_empty());
        assert_eq!(entry_page(&dir, &files), None);
        let _ = std::fs::remove_dir_all(dir);
    }
}
