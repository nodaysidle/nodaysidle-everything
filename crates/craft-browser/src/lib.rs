//! Minimal headless Chromium driver over the DevTools protocol.
//!
//! Discovers a Chromium-family browser the user already has (or one installed by Playwright),
//! and offers just what Craft Anything needs: open a URL at a viewport, evaluate JavaScript,
//! and capture a PNG screenshot.

use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

const CALL_TIMEOUT: Duration = Duration::from_secs(60);
const LOAD_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum BrowserError {
    #[error(
        "no Chromium-family browser found. Install chromium or google-chrome, run \
         `npx playwright install chromium`, or set CRAFT_BROWSER to a browser binary"
    )]
    NotFound,
    #[error("could not start headless browser: {0}")]
    Launch(String),
    #[error("browser protocol error: {0}")]
    Protocol(String),
    #[error("page failed to load: {0}")]
    Navigation(String),
}

#[derive(Debug, Clone, Copy)]
pub struct Viewport {
    pub width: u32,
    pub height: u32,
    pub mobile: bool,
}

impl Viewport {
    pub const DESKTOP: Self = Self {
        width: 1280,
        height: 800,
        mobile: false,
    };
    pub const MOBILE: Self = Self {
        width: 390,
        height: 844,
        mobile: true,
    };
}

/// Locates a usable browser binary: `CRAFT_BROWSER`, then `PATH`, then Playwright's cache,
/// then the standard macOS application paths.
pub fn find_browser() -> Option<PathBuf> {
    if let Some(custom) = std::env::var_os("CRAFT_BROWSER").filter(|v| !v.is_empty()) {
        return which::which(custom).ok();
    }
    const NAMES: &[&str] = &[
        "chromium",
        "chromium-browser",
        "google-chrome",
        "google-chrome-stable",
        "chrome",
        "microsoft-edge",
    ];
    // Privacy-hardened browsers block or rewrite parts of pages, so they are the last resort.
    const HARDENED: &[&str] = &["brave", "brave-browser", "helium"];
    let on_path = |names: &[&str]| names.iter().find_map(|name| which::which(name).ok());
    if let Some(found) = on_path(NAMES) {
        return Some(found);
    }

    let home = std::env::var_os("HOME").map(PathBuf::from);
    let caches = [
        std::env::var_os("PLAYWRIGHT_BROWSERS_PATH").map(PathBuf::from),
        home.as_ref().map(|h| h.join(".cache/ms-playwright")),
        home.as_ref().map(|h| h.join("Library/Caches/ms-playwright")),
    ];
    for cache in caches.into_iter().flatten() {
        let mut installs: Vec<PathBuf> = std::fs::read_dir(&cache)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("chromium"))
            })
            .collect();
        installs.sort();
        // Newest revision first.
        for install in installs.into_iter().rev() {
            for platform_dir in std::fs::read_dir(&install).into_iter().flatten().flatten() {
                for binary in [
                    "chrome",
                    "chrome-headless-shell",
                    "headless_shell",
                    "Chromium.app/Contents/MacOS/Chromium",
                ] {
                    let candidate = platform_dir.path().join(binary);
                    if candidate.is_file() {
                        return Some(candidate);
                    }
                }
            }
        }
    }

    [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
    ]
    .into_iter()
    .map(PathBuf::from)
    .find(|p| p.is_file())
    .or_else(|| on_path(HARDENED))
    .or_else(|| {
        Some(PathBuf::from(
            "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
        ))
        .filter(|p| p.is_file())
    })
}

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// One headless browser process with a single page attached.
pub struct Browser {
    child: Child,
    socket: Socket,
    session_id: String,
    next_id: u64,
    profile_dir: PathBuf,
    pub binary: PathBuf,
}

impl Browser {
    pub async fn launch() -> Result<Self, BrowserError> {
        let binary = find_browser().ok_or(BrowserError::NotFound)?;
        let profile_dir =
            std::env::temp_dir().join(format!("craft-browser-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&profile_dir).map_err(|e| BrowserError::Launch(e.to_string()))?;

        let mut cmd = Command::new(&binary);
        cmd.arg("--headless")
            .arg("--remote-debugging-port=0")
            .arg(format!("--user-data-dir={}", profile_dir.display()))
            .args([
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-gpu",
                "--hide-scrollbars",
                "--mute-audio",
                "--disable-extensions",
            ]);
        // e.g. CRAFT_BROWSER_ARGS="--no-sandbox" inside containers.
        if let Ok(extra) = std::env::var("CRAFT_BROWSER_ARGS") {
            cmd.args(extra.split_whitespace());
        }
        cmd.arg("about:blank")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);

        let mut child = cmd
            .spawn()
            .map_err(|e| BrowserError::Launch(format!("{}: {e}", binary.display())))?;
        let stderr = child.stderr.take().expect("stderr was piped");

        // Chromium announces its DevTools endpoint on stderr.
        let mut lines = BufReader::new(stderr).lines();
        let mut seen = Vec::new();
        let endpoint = tokio::time::timeout(Duration::from_secs(30), async {
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(url) = line.strip_prefix("DevTools listening on ") {
                    return Some(url.trim().to_string());
                }
                seen.push(line);
            }
            None
        })
        .await
        .ok()
        .flatten();
        let Some(endpoint) = endpoint else {
            let _ = std::fs::remove_dir_all(&profile_dir);
            seen.truncate(5);
            return Err(BrowserError::Launch(format!(
                "{} did not expose a DevTools endpoint. {}",
                binary.display(),
                seen.join(" | ")
            )));
        };
        // Keep draining stderr so the browser never blocks on a full pipe.
        tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });

        let (socket, _) = tokio_tungstenite::connect_async(endpoint.as_str())
            .await
            .map_err(|e| BrowserError::Launch(format!("DevTools connection failed: {e}")))?;

        let mut browser = Self {
            child,
            socket,
            session_id: String::new(),
            next_id: 0,
            profile_dir,
            binary,
        };
        let target = browser
            .call("Target.createTarget", json!({ "url": "about:blank" }))
            .await?;
        let attached = browser
            .call(
                "Target.attachToTarget",
                json!({ "targetId": target["targetId"], "flatten": true }),
            )
            .await?;
        browser.session_id = attached["sessionId"]
            .as_str()
            .ok_or_else(|| BrowserError::Protocol("no sessionId returned".into()))?
            .to_string();
        browser.call("Page.enable", json!({})).await?;
        Ok(browser)
    }

    async fn call(&mut self, method: &str, params: Value) -> Result<Value, BrowserError> {
        self.next_id += 1;
        let id = self.next_id;
        let mut request = json!({ "id": id, "method": method, "params": params });
        if !self.session_id.is_empty() {
            request["sessionId"] = json!(self.session_id);
        }
        let protocol = |e: &dyn std::fmt::Display| BrowserError::Protocol(format!("{method}: {e}"));
        self.socket
            .send(Message::text(request.to_string()))
            .await
            .map_err(|e| protocol(&e))?;

        let reply = tokio::time::timeout(CALL_TIMEOUT, async {
            // Events and replies to other ids are skipped; only our reply matters.
            while let Some(frame) = self.socket.next().await {
                let frame = frame.map_err(|e| protocol(&e))?;
                let Message::Text(text) = frame else { continue };
                let Ok(value) = serde_json::from_str::<Value>(text.as_str()) else {
                    continue;
                };
                if value["id"].as_u64() == Some(id) {
                    return Ok(value);
                }
            }
            Err(protocol(&"connection closed"))
        })
        .await
        .map_err(|_| protocol(&"timed out"))??;

        if let Some(error) = reply.get("error") {
            return Err(protocol(&error["message"].as_str().unwrap_or("unknown error")));
        }
        Ok(reply["result"].clone())
    }

    /// Navigates to `url` at the given viewport and waits for the page to finish loading.
    pub async fn open(&mut self, url: &str, viewport: Viewport) -> Result<(), BrowserError> {
        self.call(
            "Emulation.setDeviceMetricsOverride",
            json!({
                "width": viewport.width,
                "height": viewport.height,
                "deviceScaleFactor": 1,
                "mobile": viewport.mobile,
            }),
        )
        .await?;
        let mut nav = self.call("Page.navigate", json!({ "url": url })).await?;
        // Switching device emulation can reload the current page, which aborts a navigation
        // started right after it. One retry gets past that.
        if nav["errorText"] == "net::ERR_ABORTED" {
            tokio::time::sleep(Duration::from_millis(400)).await;
            nav = self.call("Page.navigate", json!({ "url": url })).await?;
        }
        // Browsers in HTTPS-only mode refuse plain http; the https address is the same site.
        if nav["errorText"] == "net::ERR_BLOCKED_BY_CLIENT"
            && let Some(rest) = url.strip_prefix("http://")
        {
            nav = self
                .call("Page.navigate", json!({ "url": format!("https://{rest}") }))
                .await?;
        }
        if let Some(error) = nav["errorText"].as_str().filter(|e| !e.is_empty()) {
            let hint = if error == "net::ERR_BLOCKED_BY_CLIENT" {
                " (the browser refused to open it; set CRAFT_BROWSER to a plain Chromium or Chrome)"
            } else {
                ""
            };
            return Err(BrowserError::Navigation(format!("{url}: {error}{hint}")));
        }

        let deadline = tokio::time::Instant::now() + LOAD_TIMEOUT;
        loop {
            let ready = self.eval("document.readyState").await;
            if matches!(&ready, Ok(v) if v == "complete") {
                break;
            }
            // A page that never settles is still worth capturing as it looks at the deadline.
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        // Let late layout, fonts and first-paint animations settle.
        tokio::time::sleep(Duration::from_millis(700)).await;
        Ok(())
    }

    /// Evaluates a JavaScript expression (promises are awaited) and returns its JSON value.
    pub async fn eval(&mut self, expression: &str) -> Result<Value, BrowserError> {
        let result = self
            .call(
                "Runtime.evaluate",
                json!({
                    "expression": expression,
                    "awaitPromise": true,
                    "returnByValue": true,
                }),
            )
            .await?;
        if let Some(details) = result.get("exceptionDetails") {
            return Err(BrowserError::Protocol(format!(
                "script failed: {}",
                details["exception"]["description"]
                    .as_str()
                    .or(details["text"].as_str())
                    .unwrap_or("unknown exception")
            )));
        }
        Ok(result["result"]["value"].clone())
    }

    /// Captures the current viewport as PNG bytes.
    pub async fn screenshot(&mut self) -> Result<Vec<u8>, BrowserError> {
        let shot = self
            .call("Page.captureScreenshot", json!({ "format": "png" }))
            .await?;
        base64::engine::general_purpose::STANDARD
            .decode(shot["data"].as_str().unwrap_or(""))
            .map_err(|e| BrowserError::Protocol(format!("bad screenshot data: {e}")))
    }

    /// Browser product string, e.g. `Chrome/140.0.0.0`.
    pub async fn version(&mut self) -> Result<String, BrowserError> {
        let saved = std::mem::take(&mut self.session_id);
        let result = self.call("Browser.getVersion", json!({})).await;
        self.session_id = saved;
        Ok(result?["product"].as_str().unwrap_or("unknown").to_string())
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        // Launcher scripts may not exec the real binary, so stop the whole process group.
        #[cfg(unix)]
        if let Some(pid) = self.child.id() {
            let _ = std::process::Command::new("kill")
                .args(["-TERM", "--", &format!("-{pid}")])
                .stderr(Stdio::null())
                .status();
        }
        let _ = self.child.start_kill();
        let _ = std::fs::remove_dir_all(&self.profile_dir);
    }
}

/// Converts a local file path into a `file://` URL the browser can open.
pub fn file_url(path: &std::path::Path) -> String {
    let absolute = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let encoded: String = absolute
        .to_string_lossy()
        .chars()
        .map(|c| match c {
            ' ' => "%20".to_string(),
            '#' => "%23".to_string(),
            '?' => "%3F".to_string(),
            '%' => "%25".to_string(),
            c => c.to_string(),
        })
        .collect();
    format!("file://{encoded}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn renders_a_local_page_when_a_browser_is_installed() {
        if find_browser().is_none() {
            eprintln!("skipped: no Chromium-family browser on this machine");
            return;
        }
        let dir = std::env::temp_dir().join(format!("craft-browser-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let page = dir.join("a page.html");
        std::fs::write(&page, "<title>T</title><h1 style='color:rgb(255,0,0)'>Hello</h1>").unwrap();

        let mut browser = Browser::launch().await.expect("browser should launch");
        browser.open(&file_url(&page), Viewport::DESKTOP).await.unwrap();
        assert_eq!(browser.eval("document.querySelector('h1').innerText").await.unwrap(), "Hello");
        assert_eq!(browser.eval("Promise.resolve(innerWidth)").await.unwrap(), 1280);
        let png = browser.screenshot().await.unwrap();
        assert_eq!(&png[1..4], b"PNG");
        assert!(browser.version().await.unwrap().contains('/'));
        assert!(browser.eval("(() => { throw new Error('boom') })()").await.is_err());
        drop(browser);
        let _ = std::fs::remove_dir_all(dir);
    }
}
