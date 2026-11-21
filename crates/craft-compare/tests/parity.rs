//! End-to-end Check stage: a faithful recreation scores high, a poor one scores low and
//! yields actionable gaps plus a heatmap.

use craft_browser::{Browser, Viewport, file_url};
use craft_compare::DefaultComparator;
use craft_core::{
    BehavioralRequirement, CleanRoomSpec, ComparatorPort, EventSink, EvidenceKind, EvidenceStore,
    StudyReport, TargetKind,
};
use std::path::{Path, PathBuf};
use uuid::Uuid;

const ORIGINAL: &str = r#"<!doctype html><title>Shop</title>
<style>body{margin:0;font-family:sans-serif;background:#fff}header{background:#c33;color:#fff;padding:24px}
.card{margin:40px;padding:40px;background:#235;color:#fff;width:300px}</style>
<header><nav><a>Home</a> <a>Pricing</a></nav></header>
<h1>Welcome to Fixture Shop</h1><div class="card">Featured item</div><button>Subscribe</button>"#;

const POOR: &str = r#"<!doctype html><title>Shop</title>
<body style="background:#111;color:#eee"><h1>Welcome to Fixture Shop</h1><p>Coming soon</p>
<script src="app.js"></script></body>"#;

struct Fixture {
    dir: PathBuf,
    store: EvidenceStore,
    study: StudyReport,
}

/// A study whose original screenshot is `ORIGINAL` rendered by the real browser.
async fn fixture() -> Fixture {
    let dir = std::env::temp_dir().join(format!("craft-parity-test-{}", Uuid::new_v4()));
    let store = EvidenceStore::open(&dir, Uuid::new_v4()).unwrap();
    let original_page = dir.join("original.html");
    std::fs::write(&original_page, ORIGINAL).unwrap();
    let mut browser = Browser::launch().await.unwrap();
    browser.open(&file_url(&original_page), Viewport::DESKTOP).await.unwrap();
    let png = browser.screenshot().await.unwrap();
    store
        .write_artifact("screenshot-desktop", EvidenceKind::Screenshot, "Original", "original_desktop.png", &png, true)
        .unwrap();

    let mut spec = CleanRoomSpec::new(TargetKind::Website, "fixture".into(), "A shop".into());
    spec.requirements = vec![
        BehavioralRequirement::new("REQ-WEB-GOAL", "goal", "Recreate the shop"),
        BehavioralRequirement::new("REQ-WEB-HEADING-1", "content", "Heading")
            .expecting(["Welcome to Fixture Shop".to_string()]),
        BehavioralRequirement::new("REQ-WEB-NAV", "navigation", "Nav links")
            .expecting(["Home".to_string(), "Pricing".to_string()]),
        BehavioralRequirement::new("REQ-WEB-BUTTONS", "interaction", "Buttons")
            .expecting(["Subscribe".to_string()]),
    ];
    Fixture {
        dir,
        store,
        study: StudyReport {
            clean_room_spec: spec,
            deep_findings: None,
            evidence_ids: vec!["screenshot-desktop".into()],
        },
    }
}

async fn compare(f: &Fixture, output: &Path, iteration: u32) -> craft_core::pipeline::ParityReport {
    DefaultComparator
        .compare(&f.study, output, iteration, &f.store, &EventSink::new(64), Uuid::new_v4())
        .await
        .unwrap()
}

#[tokio::test]
async fn faithful_recreation_passes_and_poor_one_gets_actionable_gaps() {
    if craft_browser::find_browser().is_none() {
        eprintln!("skipped: no Chromium-family browser on this machine");
        return;
    }
    let f = fixture().await;

    let poor = f.dir.join("poor");
    std::fs::create_dir_all(&poor).unwrap();
    std::fs::write(poor.join("index.html"), POOR).unwrap();
    let report = compare(&f, &poor, 1).await;
    assert!(report.score < 0.6, "{}", report.summary);
    assert!(report.gaps[0].starts_with("Build error") && report.gaps[0].contains("app.js"), "{:?}", report.gaps);
    assert!(report.gaps.iter().any(|g| g.starts_with("Visual:")), "{:?}", report.gaps);
    assert!(report.gaps.iter().any(|g| g.contains("REQ-WEB-NAV") && g.contains("\"Pricing\"")), "{:?}", report.gaps);
    assert!(!report.gaps.iter().any(|g| g.contains("REQ-WEB-HEADING-1")), "{:?}", report.gaps);

    let good = f.dir.join("good");
    std::fs::create_dir_all(&good).unwrap();
    std::fs::write(good.join("index.html"), ORIGINAL).unwrap();
    let report = compare(&f, &good, 2).await;
    assert!(report.score > 0.95, "{}", report.summary);
    assert!(report.gaps.is_empty(), "{:?}", report.gaps);

    let evidence = f.store.list_evidence().unwrap();
    for id in ["recreation-1", "heatmap-1", "comparison-1", "recreation-2", "heatmap-2", "comparison-2"] {
        assert!(evidence.iter().any(|e| e.id == id && e.clean_room_safe), "missing {id}");
    }
    let heatmap = std::fs::read(f.store.root_dir().join("artifacts/heatmap_pass_1.png")).unwrap();
    assert_eq!(&heatmap[1..4], b"PNG");
    let report_json: serde_json::Value = serde_json::from_slice(
        &std::fs::read(f.store.root_dir().join("artifacts/comparison_pass_1.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(report_json["components"]["build"]["score"], 0.0);
    assert!(report_json["components"]["visual"]["score"].as_f64().unwrap() < 0.8);
    let _ = std::fs::remove_dir_all(&f.dir);
}

#[tokio::test]
async fn empty_and_dry_run_outputs_score_zero() {
    let dir = std::env::temp_dir().join(format!("craft-parity-test-{}", Uuid::new_v4()));
    let store = EvidenceStore::open(&dir, Uuid::new_v4()).unwrap();
    let mut spec = CleanRoomSpec::new(TargetKind::File, "f".into(), "o".into());
    spec.requirements = vec![
        BehavioralRequirement::new("REQ-FILE-KEYS", "schema", "keys").expecting(["name".to_string()]),
    ];
    let study = StudyReport {
        clean_room_spec: spec,
        deep_findings: None,
        evidence_ids: vec![],
    };
    let output = store.output_dir();
    // The dry-run marker embeds the spec text; it must not satisfy the checklist.
    std::fs::write(output.join("CRAFT_DRY_RUN.md"), "prompt mentioning name").unwrap();
    let report = DefaultComparator
        .compare(&study, &output, 1, &store, &EventSink::new(16), Uuid::new_v4())
        .await
        .unwrap();
    assert_eq!(report.score, 0.0);
    assert!(report.gaps[0].contains("Nothing was generated"));

    std::fs::write(output.join("config.json"), r#"{"name": "demo"}"#).unwrap();
    let report = DefaultComparator
        .compare(&study, &output, 2, &store, &EventSink::new(16), Uuid::new_v4())
        .await
        .unwrap();
    assert_eq!(report.score, 1.0, "{}", report.summary);
    let _ = std::fs::remove_dir_all(dir);
}
