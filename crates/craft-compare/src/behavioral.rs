//! Spec checklist: does the recreation visibly contain what the clean-room spec says it must?

use craft_core::{CleanRoomSpec, REFERENCE_DIR};
use std::path::Path;

/// Directories that are never part of the recreation itself.
pub const SKIPPED_DIRS: &[&str] = &[REFERENCE_DIR, "node_modules", "target", ".git", "__pycache__"];
/// The dry-run marker embeds the whole spec, so it must not count as generated output.
pub const DRY_RUN_MARKER: &str = "CRAFT_DRY_RUN.md";

pub struct Checklist {
    /// Number of expected strings across all checkable requirements.
    pub total: usize,
    pub found: usize,
    /// One entry per requirement that is not fully satisfied.
    pub gaps: Vec<String>,
}

fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// Checks every requirement's `expect_text` against the recreation's text.
pub fn check(spec: &CleanRoomSpec, haystack: &str) -> Checklist {
    let haystack = normalize(haystack);
    let mut result = Checklist {
        total: 0,
        found: 0,
        gaps: Vec::new(),
    };
    for requirement in &spec.requirements {
        let missing: Vec<&str> = requirement
            .expect_text
            .iter()
            .filter(|text| !haystack.contains(&normalize(text)))
            .map(String::as_str)
            .collect();
        result.total += requirement.expect_text.len();
        result.found += requirement.expect_text.len() - missing.len();
        if !missing.is_empty() {
            result.gaps.push(format!(
                "{} ({}): missing visible text {}",
                requirement.id,
                requirement.description,
                missing
                    .iter()
                    .map(|m| format!("\"{m}\""))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    result
}

/// All generated files, relative to `dir`, skipping tooling folders and the dry-run marker.
pub fn output_files(dir: &Path) -> Vec<std::path::PathBuf> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if path.is_dir() {
                if !SKIPPED_DIRS.contains(&name.as_ref()) {
                    walk(root, &path, out);
                }
            } else if name != DRY_RUN_MARKER
                && let Ok(relative) = path.strip_prefix(root)
            {
                out.push(relative.to_path_buf());
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

/// Concatenated text of the generated files (bounded), used when nothing can be rendered.
pub fn collect_text(dir: &Path, files: &[std::path::PathBuf]) -> String {
    const PER_FILE: u64 = 512 * 1024;
    const TOTAL: usize = 8 * 1024 * 1024;
    let mut text = String::new();
    for file in files {
        let path = dir.join(file);
        if path.metadata().is_ok_and(|m| m.len() <= PER_FILE)
            && let Ok(contents) = std::fs::read_to_string(&path)
        {
            text.push_str(&contents);
            text.push('\n');
            if text.len() > TOTAL {
                break;
            }
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use craft_core::{BehavioralRequirement, TargetKind};

    #[test]
    fn reports_missing_text_per_requirement() {
        let mut spec = CleanRoomSpec::new(TargetKind::Website, "t".into(), "o".into());
        spec.requirements = vec![
            BehavioralRequirement::new("REQ-GOAL", "goal", "do it"),
            BehavioralRequirement::new("REQ-H", "content", "heading")
                .expecting(["Welcome  Home".to_string()]),
            BehavioralRequirement::new("REQ-NAV", "navigation", "links")
                .expecting(["Pricing".to_string(), "Docs".to_string()]),
        ];
        let result = check(&spec, "<h1>welcome\n home</h1> <a>Pricing</a>");
        assert_eq!((result.found, result.total), (2, 3));
        assert_eq!(result.gaps.len(), 1);
        assert!(result.gaps[0].starts_with("REQ-NAV") && result.gaps[0].contains("\"Docs\""));
    }
}
