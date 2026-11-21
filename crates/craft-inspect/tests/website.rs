//! Website study against a local fixture site: Clean Room must capture only observable
//! evidence, Deep (authorized) must add network and bundle findings.

use axum::Router;
use axum::http::header::CONTENT_TYPE;
use axum::response::Html;
use axum::routing::get;
use craft_core::{
    CraftRequest, DeepAuthorization, EventSink, EvidenceKind, EvidenceStore, InspectionDepth,
    InspectorPort, StudyReport, TargetKind, TargetLocation,
};
use craft_inspect::CompositeInspector;
use uuid::Uuid;

const PAGE: &str = r#"<!doctype html><html lang="en"><head><title>Fixture Shop</title>
<meta name="description" content="A tiny shop."><meta name="generator" content="FixtureGen 1.0">
<style>body{background:rgb(10,20,30);color:rgb(240,240,240);font-family:Georgia,serif}
header{background:rgb(200,50,50)} h1{font-size:40px}</style></head>
<body><header><nav aria-label="Main"><a href="/">Home</a> <a href="/pricing">Pricing</a></nav></header>
<main><h1>Welcome to Fixture Shop</h1><h2>Featured items</h2>
<form><label>Email <input type="email" name="email"></label><button type="submit">Subscribe</button></form></main>
<footer>Fixture footer</footer>
<script src="/app.js"></script></body></html>"#;

const APP_JS: &str = "fetch('/api/items').then(r => r.json()).then(d => window.items = d);\n//# sourceMappingURL=app.js.map\n";

async fn serve_fixture() -> String {
    let app = Router::new()
        .route("/", get(|| async { Html(PAGE) }))
        .route("/app.js", get(|| async { ([(CONTENT_TYPE, "text/javascript")], APP_JS) }))
        .route("/api/items", get(|| async { ([(CONTENT_TYPE, "application/json")], "[1,2,3]") }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}/")
}

async fn study(url: &str, depth: InspectionDepth) -> (StudyReport, EvidenceStore, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("craft-web-test-{}", Uuid::new_v4()));
    let run_id = Uuid::new_v4();
    let store = EvidenceStore::open(&dir, run_id).unwrap();
    let request = CraftRequest {
        target_kind: TargetKind::Website,
        location: TargetLocation::parse(url),
        prompt: "Recreate the shop landing page".into(),
        depth,
        preferred_agent: None,
        output_dir: None,
    };
    let report = CompositeInspector
        .study(&request, &store, &EventSink::new(64), run_id)
        .await
        .expect("study should succeed");
    (report, store, dir)
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

#[tokio::test]
async fn clean_room_captures_only_observable_evidence() {
    if craft_browser::find_browser().is_none() {
        eprintln!("skipped: no Chromium-family browser on this machine");
        return;
    }
    let url = serve_fixture().await;
    let (report, store, dir) = study(&url, InspectionDepth::CleanRoom).await;

    assert!(report.deep_findings.is_none());
    let evidence = store.list_evidence().unwrap();
    let ids: Vec<&str> = evidence.iter().map(|e| e.id.as_str()).collect();
    for expected in ["screenshot-desktop", "screenshot-mobile", "visible-structure-desktop"] {
        assert!(ids.contains(&expected), "{ids:?}");
    }
    assert!(evidence.iter().all(|e| e.clean_room_safe), "clean room stored deep evidence");
    assert!(!evidence.iter().any(|e| e.kind == EvidenceKind::NetworkTrace));
    let png = std::fs::read(store.root_dir().join("artifacts/original_desktop.png")).unwrap();
    assert_eq!(&png[1..4], b"PNG");

    let spec = &report.clean_room_spec;
    assert!(spec.overview.contains("Fixture Shop"), "{}", spec.overview);
    assert_eq!(spec.visual_tokens["color.page_background"], "rgb(10, 20, 30)");
    assert_eq!(spec.visual_tokens["font.family.1"], "Georgia");
    assert!(spec.ui_structure.iter().any(|l| l.contains("h1: Welcome to Fixture Shop")));
    let expected: Vec<&String> = spec.requirements.iter().flat_map(|r| &r.expect_text).collect();
    for text in ["Welcome to Fixture Shop", "Pricing", "Subscribe"] {
        assert!(expected.iter().any(|e| *e == text), "missing {text}: {expected:?}");
    }
    // Nothing from the page's source may leak into the clean-room spec.
    let spec_json = serde_json::to_string(spec).unwrap();
    for internal in ["app.js", "/api/items", "sourceMappingURL", "FixtureGen", "<script"] {
        assert!(!spec_json.contains(internal), "spec leaked {internal}");
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn authorized_deep_mode_adds_network_and_bundle_findings() {
    if craft_browser::find_browser().is_none() {
        eprintln!("skipped: no Chromium-family browser on this machine");
        return;
    }
    let url = serve_fixture().await;
    let (report, store, dir) = study(&url, deep_authorized()).await;

    let evidence = store.list_evidence().unwrap();
    let trace = evidence.iter().find(|e| e.id == "network-trace").expect("network trace stored");
    assert!(!trace.clean_room_safe);
    assert!(evidence.iter().any(|e| e.id == "screenshot-desktop" && e.clean_room_safe));

    let deep = report.deep_findings.expect("deep findings present");
    assert!(deep.extracted_symbols.iter().any(|u| u.ends_with("/api/items")), "{deep:?}");
    assert!(deep.assets_discovered.iter().any(|a| a.ends_with("/app.js")), "{deep:?}");
    assert!(deep.assets_discovered.iter().any(|a| a.starts_with("sourcemap:") && a.ends_with("app.js.map")), "{deep:?}");
    assert_eq!(deep.format_details["generator"], "FixtureGen 1.0");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn unconfirmed_deep_mode_is_refused_by_the_inspector() {
    let dir = std::env::temp_dir().join(format!("craft-web-test-{}", Uuid::new_v4()));
    let run_id = Uuid::new_v4();
    let store = EvidenceStore::open(&dir, run_id).unwrap();
    let request = CraftRequest {
        target_kind: TargetKind::Website,
        location: TargetLocation::parse("http://127.0.0.1:9/"),
        prompt: "p".into(),
        depth: InspectionDepth::Deep {
            authorization: DeepAuthorization {
                confirmed_by_user: false,
                reason: String::new(),
                confirmed_at: Default::default(),
            },
        },
        preferred_agent: None,
        output_dir: None,
    };
    let err = CompositeInspector
        .study(&request, &store, &EventSink::new(8), run_id)
        .await
        .unwrap_err();
    assert!(matches!(err, craft_core::CraftError::DeepModeUnauthorized));
    assert!(store.list_evidence().unwrap().is_empty());
    let _ = std::fs::remove_dir_all(dir);
}
