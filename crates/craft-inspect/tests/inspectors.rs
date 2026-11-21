//! File, desktop app, phone app and game studies on fixtures built in-test. None of these
//! depend on optional tools: every app/game fixture carries a screenshot, so no live capture runs.

use craft_core::{
    CraftRequest, EventSink, EvidenceStore, InspectionDepth, InspectorPort, StudyReport,
    TargetKind, TargetLocation,
};
use craft_inspect::CompositeInspector;
use std::io::Write;
use std::path::{Path, PathBuf};
use uuid::Uuid;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("craft-{tag}-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn deep_authorized() -> InspectionDepth {
    // Built through serde so the test does not need chrono as a dependency.
    serde_json::from_value(serde_json::json!({
        "mode": "deep",
        "authorization": { "confirmed_by_user": true, "reason": "test fixture I own",
                           "confirmed_at": "2026-01-01T00:00:00Z" }
    }))
    .unwrap()
}

async fn study(
    kind: TargetKind,
    location: &Path,
    depth: InspectionDepth,
) -> (StudyReport, EvidenceStore, PathBuf) {
    let runs = temp_dir("runs");
    let run_id = Uuid::new_v4();
    let store = EvidenceStore::open(&runs, run_id).unwrap();
    let request = CraftRequest {
        target_kind: kind,
        location: TargetLocation::Path(location.to_path_buf()),
        prompt: "Recreate this target".into(),
        depth,
        preferred_agent: None,
        output_dir: None,
    };
    let report = CompositeInspector
        .study(&request, &store, &EventSink::new(256), run_id)
        .await
        .expect("study should succeed");
    (report, store, runs)
}

/// Clean Room invariants: no deep findings, only clean-room-safe evidence, goal requirement kept.
fn assert_clean(report: &StudyReport, store: &EvidenceStore, goal: &str) {
    assert!(report.deep_findings.is_none());
    let evidence = store.list_evidence().unwrap();
    assert!(!evidence.is_empty());
    assert!(evidence.iter().all(|e| e.clean_room_safe), "{evidence:?}");
    assert!(
        report
            .clean_room_spec
            .requirements
            .iter()
            .any(|r| r.id == goal)
    );
}

/// Deep invariants: findings present, deep evidence stored as not clean-room safe.
fn assert_deep(report: &StudyReport, store: &EvidenceStore) {
    assert!(report.deep_findings.is_some());
    let evidence = store.list_evidence().unwrap();
    let deep = evidence
        .iter()
        .find(|e| e.id == "binary-inspection")
        .expect("deep evidence stored");
    assert!(!deep.clean_room_safe);
    assert!(report.evidence_ids.contains(&deep.id));
}

fn expected_texts(report: &StudyReport) -> Vec<String> {
    report
        .clean_room_spec
        .requirements
        .iter()
        .flat_map(|r| r.expect_text.clone())
        .collect()
}

fn write_png(path: &Path, w: u32, h: u32) {
    image::RgbImage::from_pixel(w, h, image::Rgb([200, 30, 30]))
        .save(path)
        .unwrap();
}

/// A symbol the test binary imports, read independently of the inspector.
fn imported_symbol() -> String {
    let exe = std::env::current_exe().unwrap();
    let bytes = std::fs::read(&exe).unwrap();
    match goblin::Object::parse(&bytes).unwrap() {
        goblin::Object::Elf(elf) => elf
            .dynsyms
            .iter()
            .filter(|s| s.is_import())
            .filter_map(|s| elf.dynstrtab.get_at(s.st_name))
            .find(|n| n.len() > 6)
            .unwrap()
            .to_string(),
        goblin::Object::PE(pe) => pe
            .imports
            .iter()
            .find(|i| i.name.len() > 6)
            .unwrap()
            .name
            .to_string(),
        goblin::Object::Mach(goblin::mach::Mach::Binary(m)) => m
            .imports()
            .unwrap()
            .into_iter()
            .find(|i| i.name.len() > 6)
            .unwrap()
            .name
            .to_string(),
        _ => panic!("unexpected test binary format"),
    }
}

#[tokio::test]
async fn file_json_clean_room_records_shape_and_checkable_keys() {
    let dir = temp_dir("file-json");
    let path = dir.join("catalog.json");
    std::fs::write(
        &path,
        r#"{"products":[{"sku":"A1","price":9.5}],"currency":"EUR","updated":"2026-01-01"}"#,
    )
    .unwrap();
    let (report, store, runs) = study(TargetKind::File, &path, InspectionDepth::CleanRoom).await;

    assert_clean(&report, &store, "REQ-FILE-GOAL");
    let spec = &report.clean_room_spec;
    assert_eq!(spec.visual_tokens["file.format"], "json");
    let texts = expected_texts(&report);
    for key in ["products", "currency", "updated"] {
        assert!(texts.contains(&key.to_string()), "{texts:?}");
    }
    assert!(
        spec.ui_structure
            .iter()
            .any(|l| l.contains("\"sku\": \"string\"")),
        "{:?}",
        spec.ui_structure
    );
    let meta: serde_json::Value = serde_json::from_slice(
        &std::fs::read(store.root_dir().join("artifacts/file_metadata.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        meta["json"]["shape"]["products"]["array_of"]["price"],
        "number"
    );

    let (deep_report, deep_store, deep_runs) =
        study(TargetKind::File, &path, deep_authorized()).await;
    assert_deep(&deep_report, &deep_store);
    assert!(
        deep_report
            .deep_findings
            .unwrap()
            .format_details
            .contains_key("magic_hex_16")
    );
    for d in [dir, runs, deep_runs] {
        let _ = std::fs::remove_dir_all(d);
    }
}

#[tokio::test]
async fn file_csv_zip_tar_and_directory_structures() {
    let dir = temp_dir("file-structs");
    let csv = dir.join("people.csv");
    std::fs::write(&csv, "name,email,age\nAda,a@x,36\nBob,b@x,41\n").unwrap();
    let (report, store, runs) = study(TargetKind::File, &csv, InspectionDepth::CleanRoom).await;
    assert_clean(&report, &store, "REQ-FILE-GOAL");
    let cols = report
        .clean_room_spec
        .requirements
        .iter()
        .find(|r| r.id == "REQ-FILE-CSV-COLUMNS")
        .unwrap();
    assert_eq!(cols.expect_text, vec!["name", "email", "age"]);
    assert_eq!(report.clean_room_spec.visual_tokens["csv.rows"], "2");
    let _ = std::fs::remove_dir_all(runs);

    let zip_path = dir.join("bundle.zip");
    {
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&zip_path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        zip.start_file("docs/readme.txt", opts).unwrap();
        zip.write_all(b"hello").unwrap();
        zip.start_file("img/logo.png", opts).unwrap();
        zip.write_all(&[0u8; 10]).unwrap();
        zip.finish().unwrap();
    }
    let (report, store, runs) =
        study(TargetKind::File, &zip_path, InspectionDepth::CleanRoom).await;
    assert_clean(&report, &store, "REQ-FILE-GOAL");
    let outline = report.clean_room_spec.ui_structure.join("\n");
    assert!(
        outline.contains("docs/readme.txt (5 bytes)")
            && outline.contains("img/logo.png (10 bytes)"),
        "{outline}"
    );
    let _ = std::fs::remove_dir_all(runs);

    let tgz = dir.join("site.tar.gz");
    {
        let gz = flate2::write::GzEncoder::new(
            std::fs::File::create(&tgz).unwrap(),
            flate2::Compression::fast(),
        );
        let mut tar = tar::Builder::new(gz);
        let mut header = tar::Header::new_gnu();
        header.set_size(3);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, "site/index.html", &b"<p>"[..])
            .unwrap();
        tar.into_inner().unwrap().finish().unwrap();
    }
    let (report, store, runs) = study(TargetKind::File, &tgz, InspectionDepth::CleanRoom).await;
    assert_clean(&report, &store, "REQ-FILE-GOAL");
    assert_eq!(
        report.clean_room_spec.visual_tokens["file.format"],
        "tar.gz"
    );
    assert!(
        report
            .clean_room_spec
            .ui_structure
            .iter()
            .any(|l| l == "site/index.html (3 bytes)")
    );
    let _ = std::fs::remove_dir_all(runs);

    let (report, store, runs) = study(TargetKind::File, &dir, InspectionDepth::CleanRoom).await;
    assert_clean(&report, &store, "REQ-FILE-GOAL");
    assert_eq!(
        report.clean_room_spec.visual_tokens["file.format"],
        "directory"
    );
    assert!(
        report
            .clean_room_spec
            .ui_structure
            .iter()
            .any(|l| l.starts_with("people.csv"))
    );
    for d in [dir, runs] {
        let _ = std::fs::remove_dir_all(d);
    }
}

#[tokio::test]
async fn file_sqlite_schema_when_cli_is_installed() {
    let Ok(sqlite) = which::which("sqlite3") else {
        eprintln!("skipped: sqlite3 CLI not installed");
        return;
    };
    let dir = temp_dir("file-sqlite");
    let db = dir.join("shop.db");
    let status = std::process::Command::new(sqlite)
        .arg(&db)
        .arg("CREATE TABLE customers(id INTEGER PRIMARY KEY, name TEXT); CREATE TABLE orders(id INTEGER, total REAL);")
        .status()
        .unwrap();
    assert!(status.success());
    let (report, store, runs) = study(TargetKind::File, &db, InspectionDepth::CleanRoom).await;
    assert_clean(&report, &store, "REQ-FILE-GOAL");
    let tables = report
        .clean_room_spec
        .requirements
        .iter()
        .find(|r| r.id == "REQ-FILE-SQLITE-TABLES")
        .unwrap();
    assert_eq!(tables.expect_text, vec!["customers", "orders"]);
    assert!(
        report
            .clean_room_spec
            .ui_structure
            .iter()
            .any(|l| l == "table orders(id INTEGER, total REAL)")
    );
    for d in [dir, runs] {
        let _ = std::fs::remove_dir_all(d);
    }
}

#[tokio::test]
async fn file_executable_deep_findings_never_leak_into_clean_room() {
    let exe = std::env::current_exe().unwrap();
    let symbol = imported_symbol();
    let (clean, store, runs) = study(TargetKind::File, &exe, InspectionDepth::CleanRoom).await;
    assert_clean(&clean, &store, "REQ-FILE-GOAL");
    assert_eq!(
        clean.clean_room_spec.visual_tokens["file.format"],
        "executable"
    );
    assert!(
        !serde_json::to_string(&clean.clean_room_spec)
            .unwrap()
            .contains(&symbol)
    );

    let (deep, store, deep_runs) = study(TargetKind::File, &exe, deep_authorized()).await;
    assert_deep(&deep, &store);
    let findings = deep.deep_findings.unwrap();
    assert!(
        findings
            .extracted_symbols
            .contains(&format!("import: {symbol}")),
        "{symbol}"
    );
    for d in [runs, deep_runs] {
        let _ = std::fs::remove_dir_all(d);
    }
}

#[tokio::test]
async fn file_inspector_rejects_urls_and_missing_paths() {
    let runs = temp_dir("runs");
    let run_id = Uuid::new_v4();
    let store = EvidenceStore::open(&runs, run_id).unwrap();
    for location in ["https://example.com/x.json", "/definitely/not/here.json"] {
        let request = CraftRequest {
            target_kind: TargetKind::File,
            location: TargetLocation::parse(location),
            prompt: "p".into(),
            depth: InspectionDepth::CleanRoom,
            preferred_agent: None,
            output_dir: None,
        };
        let err = CompositeInspector
            .study(&request, &store, &EventSink::new(8), run_id)
            .await
            .unwrap_err();
        assert!(
            matches!(err, craft_core::CraftError::InvalidTarget(_)),
            "{err}"
        );
    }
    let _ = std::fs::remove_dir_all(runs);
}

/// A folder holding two screenshots and a copy of the test binary.
fn app_folder(tag: &str) -> PathBuf {
    let dir = temp_dir(tag);
    write_png(&dir.join("01-home.png"), 64, 48);
    write_png(&dir.join("02-settings.png"), 32, 32);
    std::fs::copy(std::env::current_exe().unwrap(), dir.join("app-binary")).unwrap();
    dir
}

#[tokio::test]
async fn desktop_app_screenshots_clean_room_and_binary_deep() {
    let dir = app_folder("desktop");
    let symbol = imported_symbol();
    let (clean, store, runs) =
        study(TargetKind::DesktopApp, &dir, InspectionDepth::CleanRoom).await;
    assert_clean(&clean, &store, "REQ-APP-GOAL");
    let evidence = store.list_evidence().unwrap();
    let shots: Vec<_> = evidence
        .iter()
        .filter(|e| e.id.starts_with("screenshot-"))
        .collect();
    assert_eq!(
        shots.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
        ["screenshot-1", "screenshot-2"]
    );
    assert!(clean.evidence_ids.contains(&"screenshot-1".to_string()));
    assert!(
        clean
            .clean_room_spec
            .observed_features
            .iter()
            .any(|f| f.contains("64x48 px"))
    );
    assert!(
        !serde_json::to_string(&clean.clean_room_spec)
            .unwrap()
            .contains(&symbol)
    );

    let (deep, store, deep_runs) = study(TargetKind::DesktopApp, &dir, deep_authorized()).await;
    assert_deep(&deep, &store);
    let findings = deep.deep_findings.unwrap();
    assert!(findings.format_details["binary.path"].ends_with("app-binary"));
    assert!(
        findings
            .extracted_symbols
            .contains(&format!("import: {symbol}"))
    );
    for d in [dir, runs, deep_runs] {
        let _ = std::fs::remove_dir_all(d);
    }
}

#[tokio::test]
async fn game_folder_engine_and_assets_only_in_deep() {
    let dir = app_folder("game");
    std::fs::write(dir.join("project.godot"), "config_version=5").unwrap();
    std::fs::write(dir.join("game.pck"), [0u8; 128]).unwrap();
    let (clean, store, runs) = study(TargetKind::Game, &dir, InspectionDepth::CleanRoom).await;
    assert_clean(&clean, &store, "REQ-GAME-GOAL");
    let spec_json = serde_json::to_string(&clean.clean_room_spec).unwrap();
    assert!(
        !spec_json.contains("Godot") && !spec_json.contains(".pck"),
        "{spec_json}"
    );
    assert!(
        clean
            .clean_room_spec
            .open_questions
            .iter()
            .any(|q| q.contains("controls"))
    );

    let (deep, store, deep_runs) = study(TargetKind::Game, &dir, deep_authorized()).await;
    assert_deep(&deep, &store);
    let findings = deep.deep_findings.unwrap();
    assert_eq!(findings.format_details["engine"], "Godot");
    assert!(
        findings
            .assets_discovered
            .iter()
            .any(|a| a.starts_with(".pck: 1 files")),
        "{:?}",
        findings.assets_discovered
    );
    assert!(findings.format_details.contains_key("binary.format"));
    for d in [dir, runs, deep_runs] {
        let _ = std::fs::remove_dir_all(d);
    }
}

#[tokio::test]
async fn phone_app_screenshots_clean_room_and_deep_without_package() {
    let dir = temp_dir("phone");
    write_png(&dir.join("screen.png"), 36, 80);
    let (clean, store, runs) = study(TargetKind::PhoneApp, &dir, InspectionDepth::CleanRoom).await;
    assert_clean(&clean, &store, "REQ-PHONE-GOAL");
    assert!(
        store
            .list_evidence()
            .unwrap()
            .iter()
            .any(|e| e.id == "screenshot-1")
    );

    let (deep, store, deep_runs) = study(TargetKind::PhoneApp, &dir, deep_authorized()).await;
    assert_deep(&deep, &store);
    assert!(
        deep.deep_findings
            .unwrap()
            .notes
            .iter()
            .any(|n| n.contains(".apk or .ipa"))
    );
    for d in [dir, runs, deep_runs] {
        let _ = std::fs::remove_dir_all(d);
    }
}

/// A web game on disk is rendered for its Clean Room capture instead of waiting for a window.
#[tokio::test]
async fn web_game_folder_is_rendered_for_clean_room_capture() {
    if craft_browser::find_browser().is_none() {
        eprintln!("skipped: no Chromium-family browser on this machine");
        return;
    }
    let dir = std::env::temp_dir().join(format!("craft-webgame-{}", Uuid::new_v4()));
    std::fs::create_dir_all(dir.join("game")).unwrap();
    std::fs::write(
        dir.join("game/index.html"),
        "<title>Dot Catch</title><h1>Dot Catch</h1><p>Score: 0</p><canvas width=200 height=100></canvas>",
    )
    .unwrap();
    let run_id = Uuid::new_v4();
    let store = EvidenceStore::open(dir.join("runs"), run_id).unwrap();
    let request = CraftRequest {
        target_kind: TargetKind::Game,
        location: TargetLocation::Path(dir.join("game")),
        prompt: "Rebuild this game".into(),
        depth: InspectionDepth::CleanRoom,
        preferred_agent: None,
        output_dir: None,
    };
    let report = CompositeInspector
        .study(&request, &store, &EventSink::new(64), run_id)
        .await
        .unwrap();
    assert!(report.evidence_ids.iter().any(|id| id == "screenshot-desktop"));
    assert!(report.deep_findings.is_none());
    let expected: Vec<&String> = report
        .clean_room_spec
        .requirements
        .iter()
        .flat_map(|r| &r.expect_text)
        .collect();
    assert!(expected.iter().any(|t| *t == "Dot Catch"), "{expected:?}");
    let _ = std::fs::remove_dir_all(dir);
}
