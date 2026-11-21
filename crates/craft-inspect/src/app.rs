//! Desktop app, phone app and game study. Clean Room: user screenshots, passive live captures
//! and public metadata. Deep (authorized only): binary structure, APK/IPA packages, engine
//! detection and asset catalogs. Never launches, installs or executes the target.

use crate::Probe;
use crate::binary;
use crate::capture;
use crate::file::{summarize_entries, walk};
use crate::package;
use craft_core::{
    BehavioralRequirement, CleanRoomSpec, CraftError, CraftRequest, DeepFindings, EventSink,
    EvidenceKind, EvidenceStore, StudyReport, TargetKind, TargetLocation,
};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// What the target location turned out to be.
enum Target {
    Path(PathBuf),
    /// An Android package name such as `com.example.app`.
    Package(String),
}

fn is_package_name(s: &str) -> bool {
    s.contains('.')
        && !s.contains(['/', '\\'])
        && s.split('.')
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
}

pub async fn inspect(
    request: &CraftRequest,
    store: &EvidenceStore,
    events: &EventSink,
    run_id: Uuid,
) -> Result<StudyReport, CraftError> {
    let kind = request.target_kind;
    let raw = match &request.location {
        TargetLocation::Url(_) if kind == TargetKind::Game => {
            return crate::website::inspect(request, store, events, run_id).await;
        }
        TargetLocation::Url(u) => {
            return Err(CraftError::InvalidTarget(format!(
                "{} targets must be a local path{}, got URL: {u}",
                kind.as_str(),
                if kind == TargetKind::PhoneApp {
                    " or package name"
                } else {
                    ""
                }
            )));
        }
        TargetLocation::Path(p) => p.clone(),
    };
    let raw_str = raw.to_string_lossy().to_string();
    let target = if raw.exists() {
        Target::Path(raw)
    } else if kind == TargetKind::PhoneApp && is_package_name(&raw_str) {
        Target::Package(raw_str)
    } else if kind == TargetKind::DesktopApp
        && raw.components().count() == 1
        && let Ok(found) = which::which(&raw)
    {
        Target::Path(found)
    } else {
        return Err(CraftError::InvalidTarget(format!(
            "{} does not exist",
            raw.display()
        )));
    };

    let mut probe = Probe {
        store,
        events,
        run_id,
        source: match kind {
            TargetKind::PhoneApp => "inspect::phone_app",
            TargetKind::Game => "inspect::game",
            _ => "inspect::desktop_app",
        },
        evidence_ids: Vec::new(),
    };
    let label = match &target {
        Target::Path(p) => p.display().to_string(),
        Target::Package(p) => p.clone(),
    };
    let what = match kind {
        TargetKind::PhoneApp => "Phone app",
        TargetKind::Game => "Game",
        _ => "Desktop app",
    };
    let mut spec = CleanRoomSpec::new(
        kind,
        label.clone(),
        format!("{what} {label}. User goal: {}", request.prompt),
    );
    let goal_id = match kind {
        TargetKind::PhoneApp => "REQ-PHONE-GOAL",
        TargetKind::Game => "REQ-GAME-GOAL",
        _ => "REQ-APP-GOAL",
    };
    spec.requirements.push(
        BehavioralRequirement::new(goal_id, "goal", request.prompt.clone())
            .signal("The recreation fulfils the user's request."),
    );

    // Clean Room: screenshots the user captured win over live capture.
    let images = match &target {
        Target::Path(p) => capture::user_images(p),
        Target::Package(_) => Vec::new(),
    };
    let screenshots = if images.is_empty() {
        0
    } else {
        capture::save_user_screenshots(&mut probe, &mut spec, &images)?
    };

    match (kind, &target) {
        (TargetKind::PhoneApp, target) => {
            if screenshots == 0 {
                let expected = match target {
                    Target::Package(p) => Some(p.as_str()),
                    Target::Path(_) => None,
                };
                capture::capture_phone(&mut probe, &mut spec, expected).await?;
            }
        }
        (_, Target::Path(path)) => {
            if screenshots == 0 {
                let needles = public_metadata(&mut probe, &mut spec, path)?;
                let frames = if kind == TargetKind::Game { 3 } else { 1 };
                capture::capture_window(&mut probe, &mut spec, &needles, frames).await?;
            }
            if kind == TargetKind::Game {
                spec.open_questions.extend([
                    "Which controls (keys, mouse, touch) drive the game?".to_string(),
                    "What are the win and lose conditions?".to_string(),
                    "What does each HUD element mean, and how does scoring work?".to_string(),
                ]);
            }
        }
        (_, Target::Package(_)) => unreachable!("package names are only accepted for phone apps"),
    }
    if spec.observed_features.is_empty() && spec.ui_structure.is_empty() {
        spec.open_questions.push(format!(
            "Nothing visible was captured; describe the {} screens you want recreated.",
            what.to_lowercase()
        ));
    }

    let deep_findings = if request.depth.is_deep() {
        probe.info("Deep mode authorized: inspecting binaries, packages and assets");
        let deep = match (&target, kind) {
            (Target::Package(name), _) => {
                json!({ "note": format!("{name} is a package name; pass the .apk or .ipa file for package inspection") })
            }
            (Target::Path(path), TargetKind::PhoneApp) => {
                let path = path.clone();
                tokio::task::spawn_blocking(move || phone_deep(&path))
                    .await
                    .map_err(|e| {
                        CraftError::InspectionFailed(format!("package analysis panicked: {e}"))
                    })?
            }
            (Target::Path(path), _) => {
                let (path, game) = (path.clone(), kind == TargetKind::Game);
                tokio::task::spawn_blocking(move || app_deep(&path, game))
                    .await
                    .map_err(|e| {
                        CraftError::InspectionFailed(format!("binary analysis panicked: {e}"))
                    })?
            }
        };
        probe.save(
            "binary-inspection",
            EvidenceKind::BinaryInspection,
            "Binary, package and asset structure (deep)",
            "binary_inspection.json",
            &serde_json::to_vec_pretty(&deep)?,
            false,
        )?;
        Some(deep_findings(&deep))
    } else {
        None
    };

    Ok(StudyReport {
        clean_room_spec: spec,
        deep_findings,
        evidence_ids: probe.evidence_ids,
    })
}

/// Fields of a freedesktop `.desktop` entry's `[Desktop Entry]` group.
pub(crate) fn desktop_entry(text: &str) -> Vec<(String, String)> {
    let mut in_entry = false;
    let mut out = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
        } else if in_entry && let Some((k, v)) = line.split_once('=') {
            out.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    out
}

/// Records public metadata (name, size, type, `.desktop` fields) and returns window-name needles.
fn public_metadata(
    probe: &mut Probe<'_>,
    spec: &mut CleanRoomSpec,
    path: &Path,
) -> Result<Vec<String>, CraftError> {
    let name = path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    let stem = path
        .file_stem()
        .map_or_else(|| name.clone(), |s| s.to_string_lossy().into_owned());
    let mut needles = vec![stem.clone()];
    let short: String = stem.chars().take_while(|c| c.is_alphanumeric()).collect();
    if short.len() >= 3 && short != stem {
        needles.push(short);
    }
    let mut meta = json!({ "name": name, "kind": if path.is_dir() { "folder" } else { "file" } });
    if path.is_file() {
        meta["size_bytes"] = json!(std::fs::metadata(path).map(|m| m.len()).unwrap_or(0));
        if let Ok(Some(t)) = infer::get_from_path(path) {
            meta["type"] = json!(t.mime_type());
        }
    }
    if name.ends_with(".desktop")
        && let Ok(text) = std::fs::read_to_string(path)
    {
        let fields = desktop_entry(&text);
        let get = |k: &str| {
            fields
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
        };
        for key in ["Name", "Comment", "Categories"] {
            if let Some(v) = get(key) {
                meta[key.to_lowercase()] = json!(v);
            }
        }
        if let Some(app_name) = get("Name") {
            spec.overview = format!(
                "Desktop app \"{app_name}\"{}. {}",
                get("Comment").map(|c| format!(": {c}")).unwrap_or_default(),
                spec.overview
            );
            spec.requirements.push(
                BehavioralRequirement::new(
                    "REQ-APP-NAME",
                    "content",
                    format!("The app is titled \"{app_name}\""),
                )
                .expecting([app_name.clone()]),
            );
        }
        if let Some(categories) = get("Categories") {
            spec.observed_features
                .push(format!("Desktop categories: {categories}"));
        }
        needles.splice(0..0, get("StartupWMClass").into_iter().chain(get("Name")));
    }
    for (key, value) in meta.as_object().into_iter().flatten() {
        spec.visual_tokens.insert(
            format!("app.{key}"),
            value
                .as_str()
                .map_or_else(|| value.to_string(), str::to_string),
        );
    }
    probe.save(
        "app-metadata",
        EvidenceKind::FileMetadata,
        "Public app metadata",
        "app_metadata.json",
        &serde_json::to_vec_pretty(&meta)?,
        true,
    )?;
    Ok(needles)
}

/// The executable to analyze: `.app` bundles resolve `Contents/MacOS/*`, `.desktop` files their
/// `Exec` program, folders their largest executable-looking file.
fn resolve_executable(path: &Path) -> Option<PathBuf> {
    if path.extension().is_some_and(|e| e == "app") && path.join("Contents/MacOS").is_dir() {
        return binary::largest_executable(&path.join("Contents/MacOS"));
    }
    if path.extension().is_some_and(|e| e == "desktop") {
        let text = std::fs::read_to_string(path).ok()?;
        let exec = desktop_entry(&text)
            .into_iter()
            .find(|(k, _)| k == "Exec")?
            .1;
        return which::which(exec.split_whitespace().next()?).ok();
    }
    if path.is_dir() {
        return binary::largest_executable(path);
    }
    Some(path.to_path_buf())
}

fn analyze_executable(path: &Path) -> Value {
    match binary::read_capped(path, binary::BINARY_CAP) {
        Ok((_, true)) => {
            json!({ "path": path.display().to_string(), "error": "larger than the 256 MiB analysis cap" })
        }
        Ok((bytes, false)) => match binary::analyze(&bytes) {
            Some(mut analysis) => {
                analysis["path"] = json!(path.display().to_string());
                analysis["strings_sample"] =
                    json!(binary::extract_printable_strings(&bytes, 6, 64));
                analysis
            }
            None => {
                json!({ "path": path.display().to_string(), "error": "not a recognized executable format" })
            }
        },
        Err(err) => json!({ "path": path.display().to_string(), "error": err.to_string() }),
    }
}

fn app_deep(path: &Path, game: bool) -> Value {
    let mut deep = json!({});
    if game {
        let root = if path.is_dir() {
            path.to_path_buf()
        } else {
            path.parent().unwrap_or(path).to_path_buf()
        };
        let (entries, truncated) = walk(&root, 20_000);
        deep["engines"] = json!(detect_engines(&root, &entries));
        let mut catalog =
            summarize_entries(entries.iter().map(|e| (e.path.clone(), e.size, e.is_dir)));
        catalog["walk_truncated"] = json!(truncated);
        deep["asset_catalog"] = catalog;
    }
    match resolve_executable(path) {
        Some(exe) => deep["binary"] = analyze_executable(&exe),
        None => deep["note"] = json!("no executable found to analyze"),
    }
    deep
}

fn phone_deep(path: &Path) -> Value {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if path.is_file() && (ext == "apk" || ext == "ipa") {
        package::inspect_package(path).unwrap_or_else(|err| json!({ "error": err }))
    } else {
        json!({ "note": "deep phone inspection needs an .apk or .ipa file as the target" })
    }
}

/// Game engines whose file signatures appear in the folder listing.
pub(crate) fn detect_engines(root: &Path, entries: &[crate::file::WalkEntry]) -> Vec<String> {
    let paths: Vec<String> = entries.iter().map(|e| e.path.to_lowercase()).collect();
    let base = |p: &str| p.rsplit('/').next().unwrap_or(p).to_string();
    let any = |f: &dyn Fn(&str) -> bool| paths.iter().any(|p| f(p));
    let has_file = |name: &str| any(&|p| base(p) == name);
    let dir_names: Vec<&String> = entries
        .iter()
        .filter(|e| e.is_dir)
        .map(|e| &e.path)
        .collect();

    let mut engines = Vec::new();
    let mut add = |hit: bool, name: &str| {
        if hit {
            engines.push(name.to_string());
        }
    };
    add(
        dir_names.iter().any(|d| d.ends_with("_Data"))
            || any(&|p| base(p).starts_with("unityplayer."))
            || has_file("globalgamemanagers")
            || has_file("data.unity3d"),
        "Unity",
    );
    add(
        any(&|p| p.ends_with(".pck")) || has_file("project.godot"),
        "Godot",
    );
    add(
        any(&|p| p.ends_with(".pak") && p.contains("content/paks"))
            || any(&|p| p.contains("engine/binaries"))
            || any(&|p| p.ends_with(".uproject")),
        "Unreal Engine",
    );
    add(
        any(&|p| p.ends_with(".love")) || (has_file("main.lua") && has_file("conf.lua")),
        "LÖVE",
    );
    let pygame = entries
        .iter()
        .filter(|e| !e.is_dir && e.path.ends_with(".py"))
        .take(50)
        .any(|e| {
            binary::read_capped(&root.join(&e.path), 256 * 1024)
                .map(|(b, _)| {
                    let text = String::from_utf8_lossy(&b);
                    text.contains("import pygame") || text.contains("from pygame")
                })
                .unwrap_or(false)
        });
    add(pygame, "Pygame");
    add(has_file("data.win"), "GameMaker");
    add(
        any(&|p| p.ends_with("www/js/rpg_core.js") || p.ends_with("js/rmmz_core.js")),
        "RPG Maker",
    );
    add(
        dir_names
            .iter()
            .any(|d| d.to_lowercase() == "renpy" || d.to_lowercase().ends_with("/renpy"))
            && any(&|p| p.contains("game/") && (p.ends_with(".rpy") || p.ends_with(".rpyc"))),
        "Ren'Py",
    );
    add(
        has_file("index.html") && any(&|p| p.ends_with(".wasm") || p.ends_with(".js")),
        "Web (HTML5)",
    );
    engines
}

fn deep_findings(deep: &Value) -> DeepFindings {
    let mut findings = DeepFindings::default();
    let strings = |v: &Value| -> Vec<String> {
        v.as_array()
            .into_iter()
            .flatten()
            .filter_map(|s| s.as_str().map(str::to_string))
            .collect()
    };
    if deep["binary"]["format"].is_string() {
        binary::add_findings(&mut findings, "binary", &deep["binary"]);
    }
    for (key, value) in [
        ("binary.path", &deep["binary"]["path"]),
        ("binary.error", &deep["binary"]["error"]),
    ] {
        if let Some(v) = value.as_str() {
            findings.format_details.insert(key.into(), v.into());
        }
    }
    if !deep["engines"].is_null() {
        let engines = strings(&deep["engines"]);
        findings.format_details.insert(
            "engine".into(),
            if engines.is_empty() {
                "unknown".into()
            } else {
                engines.join(", ")
            },
        );
    }
    let catalog = &deep["asset_catalog"];
    if let Some(by_ext) = catalog["by_extension"].as_object() {
        findings.notes.push(format!(
            "Asset catalog: {} files, {} bytes",
            catalog["file_count"], catalog["total_bytes"]
        ));
        findings.assets_discovered.extend(
            by_ext
                .iter()
                .map(|(ext, v)| format!(".{ext}: {} files, {} bytes", v["count"], v["bytes"])),
        );
    }
    let manifest = &deep["manifest"];
    if let Some(package) = manifest["package"].as_str() {
        findings
            .format_details
            .insert("android.package".into(), package.into());
    }
    let permissions = strings(&manifest["permissions"]);
    if !permissions.is_empty() {
        findings
            .format_details
            .insert("android.permissions".into(), permissions.join(", "));
    }
    findings
        .extracted_symbols
        .extend(strings(&manifest["components"]));
    if let Some(plist) = deep["info_plist"].as_object() {
        for key in [
            "CFBundleIdentifier",
            "CFBundleName",
            "CFBundleDisplayName",
            "CFBundleShortVersionString",
            "CFBundleVersion",
            "MinimumOSVersion",
        ] {
            if let Some(v) = plist.get(key).and_then(Value::as_str) {
                findings
                    .format_details
                    .insert(format!("ios.{key}"), v.into());
            }
        }
    }
    if let Some(folders) = deep["by_folder"].as_object() {
        findings
            .notes
            .push(format!("Package has {} entries", deep["entry_count"]));
        findings.assets_discovered.extend(
            folders
                .iter()
                .map(|(f, v)| format!("{f}: {} files, {} bytes", v["count"], v["bytes"])),
        );
    }
    for key in ["note", "error"] {
        if let Some(v) = deep[key].as_str() {
            findings.notes.push(v.into());
        }
    }
    findings
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_names_are_recognized() {
        assert!(is_package_name("com.example.app"));
        assert!(!is_package_name("./game"));
        assert!(!is_package_name("notes"));
        assert!(!is_package_name("my file.apk"));
    }

    #[test]
    fn desktop_entry_fields_are_read_from_the_main_group() {
        let text = "[Desktop Entry]\nName=Calculator\nComment=Do sums\nExec=gnome-calculator %U\n[Desktop Action new]\nName=New";
        let fields = desktop_entry(text);
        assert!(fields.contains(&("Name".into(), "Calculator".into())));
        assert!(!fields.contains(&("Name".into(), "New".into())));
    }

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("craft-app-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn engines_are_detected_from_file_signatures() {
        let dir = temp_dir();
        std::fs::create_dir_all(dir.join("MyGame_Data")).unwrap();
        std::fs::write(dir.join("UnityPlayer.so"), b"x").unwrap();
        std::fs::create_dir_all(dir.join("tools/renpy")).unwrap();
        std::fs::create_dir_all(dir.join("tools/game")).unwrap();
        std::fs::write(dir.join("tools/game/script.rpy"), b"label start:").unwrap();
        std::fs::write(dir.join("tools/run.py"), b"import os\nimport pygame\n").unwrap();
        let (entries, _) = walk(&dir, 1000);
        assert_eq!(
            detect_engines(&dir, &entries),
            vec!["Unity", "Pygame", "Ren'Py"]
        );

        let other = temp_dir();
        std::fs::write(other.join("index.html"), b"<canvas>").unwrap();
        std::fs::write(other.join("game.wasm"), b"\0asm").unwrap();
        let (entries, _) = walk(&other, 1000);
        assert_eq!(detect_engines(&other, &entries), vec!["Web (HTML5)"]);
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(other);
    }

    #[test]
    fn apk_deep_inspection_decodes_manifest_and_folders() {
        use std::io::Write;
        let dir = temp_dir();
        let apk = dir.join("notes.apk");
        let manifest = crate::package::tests::build_axml(&[
            ("manifest", &[("package", "com.example.notes")]),
            (
                "uses-permission",
                &[("name", "android.permission.RECORD_AUDIO")],
            ),
            ("activity", &[("name", ".MainActivity")]),
        ]);
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&apk).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        for (name, data) in [
            ("AndroidManifest.xml", manifest.as_slice()),
            ("classes.dex", b"dex\n035".as_slice()),
            ("classes2.dex", b"dex\n035".as_slice()),
            ("lib/arm64-v8a/libnative.so", b"\x7fELF".as_slice()),
            ("res/layout/main.xml", b"x".as_slice()),
        ] {
            zip.start_file(name, opts).unwrap();
            zip.write_all(data).unwrap();
        }
        zip.finish().unwrap();

        let findings = deep_findings(&phone_deep(&apk));
        assert_eq!(
            findings.format_details["android.package"],
            "com.example.notes"
        );
        assert_eq!(
            findings.format_details["android.permissions"],
            "android.permission.RECORD_AUDIO"
        );
        assert!(
            findings
                .extracted_symbols
                .contains(&"activity: .MainActivity".to_string())
        );
        assert!(
            findings
                .assets_discovered
                .iter()
                .any(|a| a.starts_with("classes*.dex: 2 files"))
        );
        assert!(
            findings
                .assets_discovered
                .iter()
                .any(|a| a.starts_with("lib/arm64-v8a/: 1 files"))
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
