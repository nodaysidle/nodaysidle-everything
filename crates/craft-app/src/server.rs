use anyhow::Result;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use craft_agents::{AgentBuilder, DetectedAgent, detect_installed_agents};
use craft_compare::DefaultComparator;
use craft_core::{
    CancellationToken, CraftRequest, DeepAuthorization, EventSink, EvidenceItem, EvidenceStore,
    InspectionDepth, Pipeline, RunConfig, RunState, RunStatus, TargetKind, TargetLocation,
};
use craft_inspect::CompositeInspector;
use futures_util::stream::Stream;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use uuid::Uuid;

const INDEX_HTML: &str = include_str!("../assets/index.html");

pub struct AppState {
    runs_dir: PathBuf,
    events: EventSink,
    /// Cancellation handles for runs executing in this process.
    active: Mutex<HashMap<Uuid, CancellationToken>>,
}

#[derive(Deserialize)]
struct CreateRunPayload {
    target_kind: TargetKind,
    source: String,
    prompt: String,
    #[serde(default)]
    deep: bool,
    #[serde(default)]
    confirm_authorized: bool,
    #[serde(default)]
    authorization_reason: Option<String>,
    #[serde(default)]
    preferred_agent: Option<String>,
    #[serde(default)]
    dry_run: bool,
    #[serde(default)]
    max_iterations: Option<u32>,
}

type ApiError = (StatusCode, Json<Value>);

fn api_error(code: StatusCode, message: impl Into<String>) -> ApiError {
    (
        code,
        Json(json!({ "status": "error", "message": message.into() })),
    )
}

pub fn router(runs_dir: PathBuf) -> Router {
    let state = Arc::new(AppState {
        events: EventSink::new(2048).persist_to(runs_dir.clone()),
        runs_dir,
        active: Mutex::new(HashMap::new()),
    });

    // No CORS layer on purpose: this API launches coding agents, so only the page it serves may call it.
    Router::new()
        .route("/", get(index))
        .route("/api/agents", get(list_agents))
        .route("/api/doctor", get(doctor))
        .route("/api/runs", get(list_runs).post(start_run))
        .route("/api/runs/{id}", get(get_run))
        .route("/api/runs/{id}/evidence", get(get_evidence))
        .route("/api/runs/{id}/events", get(get_run_events))
        .route("/api/runs/{id}/cancel", post(cancel_run))
        .route("/api/runs/{id}/open", post(open_output))
        .route("/api/runs/{id}/artifacts/{*path}", get(get_artifact))
        .route("/api/runs/{id}/output/{*path}", get(get_output))
        .route("/api/events", get(stream_events))
        .with_state(state)
}

pub async fn serve(host: &str, port: u16, runs_dir: PathBuf) -> Result<()> {
    std::fs::create_dir_all(&runs_dir)?;
    let mut app = router(runs_dir);
    let loopback = matches!(host, "127.0.0.1" | "localhost" | "::1");
    if loopback {
        app = app.layer(middleware::from_fn(require_local_host));
    } else {
        tracing::warn!(
            "Bound to {host}: anyone who can reach this address can start coding-agent runs."
        );
    }

    let listener = tokio::net::TcpListener::bind((host, port)).await?;
    tracing::info!(
        "Craft Anything UI running at http://{}",
        listener.local_addr()?
    );
    axum::serve(listener, app).await?;
    Ok(())
}

/// Rejects requests addressed to another hostname (DNS rebinding) when serving on loopback.
async fn require_local_host(req: Request, next: Next) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let name = match host.rsplit_once(':') {
        Some((name, port)) if port.parse::<u16>().is_ok() => name,
        _ => host,
    };
    if matches!(name, "127.0.0.1" | "localhost" | "[::1]") {
        next.run(req).await
    } else {
        api_error(StatusCode::FORBIDDEN, "this server only answers on localhost").into_response()
    }
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn list_agents() -> Json<Vec<DetectedAgent>> {
    Json(detect_installed_agents())
}

async fn doctor() -> Json<Value> {
    Json(json!(crate::doctor::collect().await))
}

async fn start_run(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<CreateRunPayload>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    if payload.source.trim().is_empty() || payload.prompt.trim().is_empty() {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "both a target (URL or path) and a description are required",
        ));
    }

    // Deep mode only ever comes from the request's explicit flags; nothing here escalates to it.
    let depth = if payload.deep {
        InspectionDepth::Deep {
            authorization: DeepAuthorization {
                confirmed_by_user: payload.confirm_authorized,
                reason: payload
                    .authorization_reason
                    .unwrap_or_else(|| "UI deep inspection consent".into()),
                confirmed_at: Utc::now(),
            },
        }
    } else {
        InspectionDepth::CleanRoom
    };
    depth
        .validate()
        .map_err(|err| api_error(StatusCode::BAD_REQUEST, err.to_string()))?;

    let request = CraftRequest {
        target_kind: payload.target_kind,
        location: TargetLocation::parse(&payload.source),
        prompt: payload.prompt,
        depth,
        preferred_agent: payload.preferred_agent.filter(|a| !a.trim().is_empty()),
        output_dir: None,
    };
    let config = RunConfig {
        max_iterations: payload.max_iterations.unwrap_or(3).clamp(1, 10),
        ..Default::default()
    };

    let run_id = Uuid::new_v4();
    let cancel = CancellationToken::new();
    state
        .active
        .lock()
        .expect("active runs lock")
        .insert(run_id, cancel.clone());

    let dry_run = payload.dry_run;
    let task_state = state.clone();
    tokio::spawn(async move {
        let pipeline = Pipeline::new(
            CompositeInspector,
            AgentBuilder {
                dry_run,
                ..Default::default()
            },
            DefaultComparator,
            config,
        );
        if let Err(err) = pipeline
            .execute_with_id(
                run_id,
                request,
                &task_state.runs_dir,
                &task_state.events,
                cancel,
            )
            .await
        {
            tracing::warn!("run {run_id} ended: {err}");
        }
        task_state
            .active
            .lock()
            .expect("active runs lock")
            .remove(&run_id);
    });

    // The pipeline saves the initial state before its first await; wait for it so the returned
    // id is immediately usable with GET /api/runs/{id}.
    for _ in 0..100 {
        if EvidenceStore::open_existing(&state.runs_dir, run_id).is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "started",
            "run_id": run_id,
            "message": "Run started; subscribe to /api/events for live progress.",
        })),
    ))
}

/// Loads a run; a run still marked running that this process does not own was interrupted.
fn load_run(state: &AppState, id: Uuid) -> Option<(EvidenceStore, RunState)> {
    let store = EvidenceStore::open_existing(&state.runs_dir, id)?;
    let mut run = store.load_state().ok()?;
    if run.status == RunStatus::Running
        && !state.active.lock().expect("active runs lock").contains_key(&id)
    {
        run.status = RunStatus::Failed;
        run.error = Some("Interrupted: the app stopped while this run was in progress".into());
    }
    Some((store, run))
}

async fn list_runs(State(state): State<Arc<AppState>>) -> Json<Vec<RunState>> {
    let mut runs: Vec<RunState> = std::fs::read_dir(&state.runs_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| Uuid::parse_str(entry.file_name().to_str()?).ok())
        .filter_map(|id| load_run(&state, id).map(|(_, run)| run))
        .collect();
    runs.sort_by_key(|run| std::cmp::Reverse(run.created_at));
    Json(runs)
}

async fn get_run(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Json<RunState>, StatusCode> {
    load_run(&state, id)
        .map(|(_, run)| Json(run))
        .ok_or(StatusCode::NOT_FOUND)
}

async fn get_evidence(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<EvidenceItem>>, StatusCode> {
    let (store, _) = load_run(&state, id).ok_or(StatusCode::NOT_FOUND)?;
    store
        .list_evidence()
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

/// The persisted event log, so Advanced Mode can replay a past or in-flight run.
async fn get_run_events(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<Value>>, StatusCode> {
    let (store, _) = load_run(&state, id).ok_or(StatusCode::NOT_FOUND)?;
    let raw = tokio::fs::read_to_string(store.root_dir().join("events.ndjson"))
        .await
        .unwrap_or_default();
    Ok(Json(
        raw.lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect(),
    ))
}

async fn cancel_run(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let token = state.active.lock().expect("active runs lock").get(&id).cloned();
    match token {
        Some(token) => {
            token.cancel();
            Ok(Json(json!({ "status": "cancelling", "run_id": id })))
        }
        None if load_run(&state, id).is_some() => Err(api_error(
            StatusCode::CONFLICT,
            "this run is not in progress",
        )),
        None => Err(api_error(StatusCode::NOT_FOUND, "no such run")),
    }
}

async fn open_output(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let (_, run) =
        load_run(&state, id).ok_or_else(|| api_error(StatusCode::NOT_FOUND, "no such run"))?;
    let opener = ["xdg-open", "open", "explorer"]
        .into_iter()
        .find_map(|name| which::which(name).ok())
        .ok_or_else(|| {
            api_error(
                StatusCode::NOT_IMPLEMENTED,
                format!(
                    "no file manager launcher found; the output is at {}",
                    run.workspace_dir.display()
                ),
            )
        })?;
    std::process::Command::new(opener)
        .arg(&run.workspace_dir)
        .spawn()
        .map_err(|e| api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "status": "opened", "path": run.workspace_dir })))
}

fn content_type(path: &std::path::Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" | "map" => "application/json",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "wasm" => "application/wasm",
        "txt" | "log" | "md" | "xml" | "csv" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// Serves one file from under `base`, refusing anything that resolves outside it.
async fn serve_file(base: PathBuf, rel: &str) -> Result<Response, StatusCode> {
    let base = base.canonicalize().map_err(|_| StatusCode::NOT_FOUND)?;
    let full = base
        .join(rel.trim_start_matches('/'))
        .canonicalize()
        .map_err(|_| StatusCode::NOT_FOUND)?;
    if !full.starts_with(&base) || !full.is_file() {
        return Err(StatusCode::NOT_FOUND);
    }
    let bytes = tokio::fs::read(&full)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    let mut response = bytes.into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(content_type(&full)),
    );
    // Agent-built pages and captured evidence are untrusted: run them in an opaque origin so
    // their scripts cannot call this API.
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("sandbox allow-scripts"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    Ok(response)
}

async fn get_artifact(
    State(state): State<Arc<AppState>>,
    Path((id, path)): Path<(Uuid, String)>,
) -> Result<Response, StatusCode> {
    let (store, _) = load_run(&state, id).ok_or(StatusCode::NOT_FOUND)?;
    serve_file(store.root_dir().join("artifacts"), &path).await
}

async fn get_output(
    State(state): State<Arc<AppState>>,
    Path((id, path)): Path<(Uuid, String)>,
) -> Result<Response, StatusCode> {
    let (_, run) = load_run(&state, id).ok_or(StatusCode::NOT_FOUND)?;
    serve_file(run.workspace_dir, &path).await
}

async fn stream_events(
    State(state): State<Arc<AppState>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.events.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|msg| match msg {
        Ok(event) => {
            let data = serde_json::to_string(&event).unwrap_or_default();
            Some(Ok(Event::default().data(data)))
        }
        Err(_) => None,
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use tower::ServiceExt;

    struct TestApp {
        app: Router,
        dir: PathBuf,
    }

    impl TestApp {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("craft-server-test-{}", Uuid::new_v4()));
            std::fs::create_dir_all(dir.join("runs")).unwrap();
            Self {
                app: router(dir.join("runs")),
                dir,
            }
        }

        async fn call(&self, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Response) {
            let mut request = Request::builder().method(method).uri(uri);
            let body = match body {
                Some(json) => {
                    request = request.header(header::CONTENT_TYPE, "application/json");
                    Body::from(json.to_string())
                }
                None => Body::empty(),
            };
            let response = self
                .app
                .clone()
                .oneshot(request.body(body).unwrap())
                .await
                .unwrap();
            (response.status(), response)
        }

        async fn json(&self, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
            let (status, response) = self.call(method, uri, body).await;
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
        }

        /// Polls the run until `done` holds, failing the test after 30 seconds.
        async fn wait_for(&self, run_id: &str, done: impl Fn(&Value) -> bool) -> Value {
            for _ in 0..300 {
                let (_, run) = self.json("GET", &format!("/api/runs/{run_id}"), None).await;
                if done(&run) {
                    return run;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            panic!("run {run_id} did not reach the expected state in time");
        }

        fn fixture(&self, name: &str, contents: &str) -> String {
            let path = self.dir.join(name);
            std::fs::write(&path, contents).unwrap();
            path.display().to_string()
        }
    }

    impl Drop for TestApp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// Full `study -> build (dry run) -> check -> ready` loop through the HTTP API.
    #[tokio::test]
    async fn dry_run_completes_and_exposes_its_evidence() {
        let t = TestApp::new();
        let source = t.fixture("data.json", r#"{"name":"demo","items":[1,2]}"#);

        let (status, started) = t
            .json(
                "POST",
                "/api/runs",
                Some(json!({
                    "target_kind": "file", "source": source,
                    "prompt": "Recreate this config", "dry_run": true, "max_iterations": 1,
                })),
            )
            .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let run_id = started["run_id"].as_str().expect("run_id returned synchronously");

        let run = t.wait_for(run_id, |r| r["status"] != "running").await;
        assert_eq!(run["status"], "ready", "{run}");
        assert_eq!(run["stage"], "ready");
        assert_eq!(run["request"]["depth"]["mode"], "clean_room");
        assert_eq!(run["iterations"].as_array().unwrap().len(), 1);

        let (_, runs) = t.json("GET", "/api/runs", None).await;
        assert_eq!(runs.as_array().unwrap().len(), 1);
        assert_eq!(runs[0]["id"], run_id);

        let (_, evidence) = t.json("GET", &format!("/api/runs/{run_id}/evidence"), None).await;
        let ids: Vec<&str> = evidence
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e["id"].as_str())
            .collect();
        assert!(ids.contains(&"clean-room-spec"), "{ids:?}");
        assert!(!ids.contains(&"deep-findings"), "clean room must not store deep findings");

        let (status, spec) = t
            .json("GET", &format!("/api/runs/{run_id}/artifacts/clean_room_spec.json"), None)
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(spec["target_kind"], "file");

        let (_, events) = t.json("GET", &format!("/api/runs/{run_id}/events"), None).await;
        let stages: Vec<&str> = events
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["kind"]["type"] == "stage_changed")
            .filter_map(|e| e["kind"]["stage"].as_str())
            .collect();
        assert_eq!(stages, ["study", "build", "check", "ready"]);

        // The dry-run prompt is served from the output folder in a sandboxed origin.
        let (status, response) = t
            .call("GET", &format!("/api/runs/{run_id}/output/CRAFT_DRY_RUN.md"), None)
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_SECURITY_POLICY],
            "sandbox allow-scripts"
        );
    }

    #[tokio::test]
    async fn artifact_paths_cannot_escape_the_run() {
        let t = TestApp::new();
        let source = t.fixture("a.txt", "hello");
        let (_, started) = t
            .json(
                "POST",
                "/api/runs",
                Some(json!({ "target_kind": "file", "source": source, "prompt": "p", "dry_run": true, "max_iterations": 1 })),
            )
            .await;
        let run_id = started["run_id"].as_str().unwrap();
        t.wait_for(run_id, |r| r["status"] != "running").await;

        for path in ["../state.json", "..%2Fstate.json", "%2e%2e/%2e%2e/a.txt", "missing.png"] {
            let (status, _) = t
                .call("GET", &format!("/api/runs/{run_id}/artifacts/{path}"), None)
                .await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        }
    }

    #[tokio::test]
    async fn deep_mode_without_confirmation_is_rejected_before_anything_runs() {
        let t = TestApp::new();
        let (status, body) = t
            .json(
                "POST",
                "/api/runs",
                Some(json!({
                    "target_kind": "file", "source": "/etc/hostname", "prompt": "p",
                    "deep": true, "confirm_authorized": false, "dry_run": true,
                })),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["message"].as_str().unwrap().contains("authorization"));
        let (_, runs) = t.json("GET", "/api/runs", None).await;
        assert!(runs.as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn unknown_runs_are_not_found_and_not_created() {
        let t = TestApp::new();
        let id = Uuid::new_v4();
        for uri in [
            format!("/api/runs/{id}"),
            format!("/api/runs/{id}/evidence"),
            format!("/api/runs/{id}/events"),
        ] {
            assert_eq!(t.call("GET", &uri, None).await.0, StatusCode::NOT_FOUND);
        }
        let (status, _) = t.call("POST", &format!("/api/runs/{id}/cancel"), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(!t.dir.join("runs").join(id.to_string()).exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_running_agent_can_be_cancelled() {
        use std::os::unix::fs::PermissionsExt;
        let t = TestApp::new();
        let source = t.fixture("a.txt", "hello");
        let agent = t.fixture("slow-agent.sh", "#!/bin/sh\necho started\nsleep 60\n");
        std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();

        let (_, started) = t
            .json(
                "POST",
                "/api/runs",
                Some(json!({ "target_kind": "file", "source": source, "prompt": "p", "preferred_agent": agent })),
            )
            .await;
        let run_id = started["run_id"].as_str().unwrap();
        t.wait_for(run_id, |r| r["stage"] == "build").await;

        let (status, _) = t.json("POST", &format!("/api/runs/{run_id}/cancel"), None).await;
        assert_eq!(status, StatusCode::OK);
        let run = t.wait_for(run_id, |r| r["status"] != "running").await;
        assert_eq!(run["status"], "cancelled");

        // Once finished, the run is no longer cancellable.
        for _ in 0..50 {
            let (status, _) = t.json("POST", &format!("/api/runs/{run_id}/cancel"), None).await;
            if status == StatusCode::CONFLICT {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("finished run stayed cancellable");
    }

    #[tokio::test]
    async fn loopback_server_refuses_foreign_host_headers() {
        let app = router(std::env::temp_dir().join("craft-host-test"))
            .layer(middleware::from_fn(require_local_host));
        let get = |host: &'static str| {
            let app = app.clone();
            async move {
                let request = Request::builder()
                    .uri("/api/agents")
                    .header(header::HOST, host)
                    .body(Body::empty())
                    .unwrap();
                app.oneshot(request).await.unwrap().status()
            }
        };
        assert_eq!(get("127.0.0.1:7420").await, StatusCode::OK);
        assert_eq!(get("localhost:7420").await, StatusCode::OK);
        assert_eq!(get("evil.example:7420").await, StatusCode::FORBIDDEN);
    }
}
