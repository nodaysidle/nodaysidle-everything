//! Clean Room capture helpers shared by the app, phone and game inspectors: user-supplied
//! screenshots, passive live window capture (Hyprland/grim, macOS screencapture) and the
//! current screen plus UI hierarchy of a connected Android phone (adb). Nothing here
//! launches, taps or modifies anything.

use crate::Probe;
use craft_core::{CleanRoomSpec, CraftError, EvidenceKind};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

const MAX_SCREENSHOTS: usize = 12;
const MAX_SCREENSHOT_BYTES: u64 = 32 * 1024 * 1024;
const IMAGE_EXTENSIONS: [&str; 6] = ["png", "jpg", "jpeg", "webp", "gif", "bmp"];

/// Runs an optional external tool passively, killing it after `timeout_secs`.
pub(crate) async fn run_tool(
    program: &Path,
    args: &[&str],
    timeout_secs: u64,
) -> Result<Vec<u8>, String> {
    let name = program.file_name().map_or_else(
        || program.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    match tokio::time::timeout(Duration::from_secs(timeout_secs), command.output()).await {
        Err(_) => Err(format!("{name} timed out after {timeout_secs}s")),
        Ok(Err(err)) => Err(format!("{name} could not run: {err}")),
        Ok(Ok(out)) if !out.status.success() => Err(format!(
            "{name} failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )),
        Ok(Ok(out)) => Ok(out.stdout),
    }
}

pub(crate) fn tool(name: &str) -> Option<PathBuf> {
    which::which(name).ok()
}

pub(crate) fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| IMAGE_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// The target as user-supplied screenshots: an image file, or the images in a folder (sorted, capped).
pub(crate) fn user_images(path: &Path) -> Vec<PathBuf> {
    if path.is_file() {
        return if is_image(path) {
            vec![path.to_path_buf()]
        } else {
            Vec::new()
        };
    }
    let Ok(dir) = std::fs::read_dir(path) else {
        return Vec::new();
    };
    let mut images: Vec<PathBuf> = dir
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && is_image(p))
        .collect();
    images.sort();
    images.truncate(MAX_SCREENSHOTS);
    images
}

/// Copies user screenshots into evidence as `screenshot-1..N` (clean-room safe) and records them.
pub(crate) fn save_user_screenshots(
    probe: &mut Probe<'_>,
    spec: &mut CleanRoomSpec,
    images: &[PathBuf],
) -> Result<usize, CraftError> {
    let mut saved = 0;
    for path in images {
        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        if size > MAX_SCREENSHOT_BYTES {
            probe.warn(format!("Skipped {} (larger than 32 MiB)", path.display()));
            continue;
        }
        let bytes = std::fs::read(path)?;
        saved += 1;
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("png")
            .to_ascii_lowercase();
        let dims = image::image_dimensions(path)
            .map(|(w, h)| format!("{w}x{h} px"))
            .unwrap_or_else(|_| "size unknown".into());
        probe.save(
            &format!("screenshot-{saved}"),
            EvidenceKind::Screenshot,
            &format!("User screenshot {saved}: {name} ({dims})"),
            &format!("screenshot_{saved}.{ext}"),
            &bytes,
            true,
        )?;
        spec.observed_features
            .push(format!("User screenshot {saved} ({name}): {dims}"));
    }
    if saved > 0 {
        probe.info(format!("Recorded {saved} user-supplied screenshot(s)"));
        spec.open_questions.push(
            "Screenshots show static screens only; describe transitions and interactions between them."
                .into(),
        );
    }
    Ok(saved)
}

/// A capturable window found by name.
#[derive(Debug, PartialEq)]
pub(crate) struct Window {
    pub title: String,
    pub class: String,
    pub geometry: String,
    pub width: i64,
    pub height: i64,
}

pub(crate) enum WindowMatch {
    Visible(Window),
    /// Matching window exists but sits on a workspace that is not shown.
    Hidden {
        title: String,
        workspace: i64,
    },
    None,
}

/// Finds a Hyprland client (`hyprctl clients -j`) whose class/initialClass/title contains a needle.
pub(crate) fn find_window(
    clients: &Value,
    visible_workspaces: &[i64],
    needles: &[String],
) -> WindowMatch {
    let mut hidden = WindowMatch::None;
    for client in clients.as_array().into_iter().flatten() {
        let field = |k: &str| client[k].as_str().unwrap_or("").to_string();
        let haystack = format!(
            "{} {} {}",
            field("class"),
            field("initialClass"),
            field("title")
        )
        .to_lowercase();
        let matched = needles
            .iter()
            .any(|n| !n.is_empty() && haystack.contains(&n.to_lowercase()));
        let (w, h) = (
            client["size"][0].as_i64().unwrap_or(0),
            client["size"][1].as_i64().unwrap_or(0),
        );
        if !matched || w <= 0 || h <= 0 || client["hidden"].as_bool() == Some(true) {
            continue;
        }
        let workspace = client["workspace"]["id"].as_i64().unwrap_or(0);
        if !visible_workspaces.is_empty() && !visible_workspaces.contains(&workspace) {
            hidden = WindowMatch::Hidden {
                title: field("title"),
                workspace,
            };
            continue;
        }
        return WindowMatch::Visible(Window {
            title: field("title"),
            class: field("class"),
            geometry: format!(
                "{},{} {w}x{h}",
                client["at"][0].as_i64().unwrap_or(0),
                client["at"][1].as_i64().unwrap_or(0)
            ),
            width: w,
            height: h,
        });
    }
    hidden
}

/// Passively captures the target's open window `frames` times, about a second apart.
/// Evidence ids are `screenshot-window` (one frame) or `screenshot-frame-N`. Never fails the study.
pub(crate) async fn capture_window(
    probe: &mut Probe<'_>,
    spec: &mut CleanRoomSpec,
    needles: &[String],
    frames: usize,
) -> Result<usize, CraftError> {
    let id = |n: usize| {
        if frames == 1 {
            "screenshot-window".to_string()
        } else {
            format!("screenshot-frame-{n}")
        }
    };
    let advice =
        "open the app and re-run, or pass an image or a folder of screenshots as the target";

    if let (Some(hyprctl), Some(grim)) = (tool("hyprctl"), tool("grim")) {
        let clients = match run_tool(&hyprctl, &["clients", "-j"], 10).await {
            Ok(out) => serde_json::from_slice::<Value>(&out).unwrap_or(Value::Null),
            Err(err) => {
                probe.warn(format!("Window capture skipped: {err}; {advice}"));
                spec.open_questions
                    .push("No live window could be captured.".into());
                return Ok(0);
            }
        };
        let visible: Vec<i64> = match run_tool(&hyprctl, &["monitors", "-j"], 10).await {
            Ok(out) => serde_json::from_slice::<Value>(&out)
                .unwrap_or(Value::Null)
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|m| m["activeWorkspace"]["id"].as_i64())
                .collect(),
            Err(_) => Vec::new(),
        };
        let window = match find_window(&clients, &visible, needles) {
            WindowMatch::Visible(w) => w,
            WindowMatch::Hidden { title, workspace } => {
                probe.warn(format!(
                    "Found window \"{title}\" on workspace {workspace}, which is not visible; switch to it and re-run, or {advice}"
                ));
                spec.open_questions
                    .push("The app's window was not visible, so no live capture was taken.".into());
                return Ok(0);
            }
            WindowMatch::None => {
                probe.warn(format!(
                    "No open window matches {}; {advice}",
                    needles.join(" / ")
                ));
                spec.open_questions
                    .push("The app was not open, so no live capture was taken.".into());
                return Ok(0);
            }
        };
        probe.info(format!(
            "Capturing window \"{}\" ({}) at {}",
            window.title, window.class, window.geometry
        ));
        let mut saved = 0;
        for n in 1..=frames {
            if n > 1 {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            match run_tool(&grim, &["-g", &window.geometry, "-"], 15).await {
                Ok(png) if png.starts_with(b"\x89PNG") => {
                    probe.save(
                        &id(n),
                        EvidenceKind::Screenshot,
                        &format!("Live window capture {n} of \"{}\"", window.title),
                        &format!("{}.png", id(n).replace('-', "_")),
                        &png,
                        true,
                    )?;
                    saved += 1;
                }
                Ok(_) => probe.warn("grim returned no PNG data"),
                Err(err) => probe.warn(format!("Window capture failed: {err}")),
            }
        }
        if saved > 0 {
            spec.ui_structure.push(format!(
                "Window \"{}\" ({}x{} px)",
                window.title, window.width, window.height
            ));
            spec.visual_tokens.insert(
                "window.size".into(),
                format!("{}x{}", window.width, window.height),
            );
        }
        return Ok(saved);
    }

    if cfg!(target_os = "macos")
        && let Some(screencapture) = tool("screencapture")
    {
        probe.info("macOS: capturing the whole screen; make sure the app is in front");
        let mut saved = 0;
        for n in 1..=frames {
            if n > 1 {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            let file =
                std::env::temp_dir().join(format!("craft-capture-{}.png", uuid::Uuid::new_v4()));
            let result = run_tool(&screencapture, &["-x", &file.to_string_lossy()], 15).await;
            let bytes = std::fs::read(&file).ok();
            let _ = std::fs::remove_file(&file);
            match (result, bytes) {
                (Ok(_), Some(png)) => {
                    probe.save(
                        &id(n),
                        EvidenceKind::Screenshot,
                        &format!("Screen capture {n}"),
                        &format!("{}.png", id(n).replace('-', "_")),
                        &png,
                        true,
                    )?;
                    saved += 1;
                }
                (Err(err), _) => probe.warn(format!("Screen capture failed: {err}")),
                (Ok(_), None) => {
                    probe.warn("screencapture wrote no file (screen recording permission?)")
                }
            }
        }
        return Ok(saved);
    }

    probe.warn(format!(
        "Live capture skipped: needs hyprctl + grim (Hyprland) or macOS screencapture; {advice}"
    ));
    spec.open_questions
        .push("No live capture tool was available, so the app's look is unknown.".into());
    Ok(0)
}

/// Serials of attached devices in state `device` from `adb devices` output.
pub(crate) fn parse_adb_devices(output: &str) -> Vec<String> {
    output
        .lines()
        .skip(1)
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let serial = parts.next()?;
            (parts.next() == Some("device")).then(|| serial.to_string())
        })
        .collect()
}

/// One visible node of a `uiautomator dump`.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct UiNode {
    pub text: String,
    pub desc: String,
    pub class: String,
    pub package: String,
    pub clickable: bool,
    pub bounds: String,
}

impl UiNode {
    pub fn label(&self) -> &str {
        if self.text.is_empty() {
            &self.desc
        } else {
            &self.text
        }
    }
}

pub(crate) fn unescape_xml(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(end) = rest.find(';') else { break };
        let entity = &rest[1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => entity
                .strip_prefix("#x")
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| entity.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// `key="value"` attributes of one XML start tag body.
pub(crate) fn xml_attributes(tag: &str) -> Vec<(String, String)> {
    let mut attrs = Vec::new();
    let mut rest = tag;
    while let Some(eq) = rest.find("=\"") {
        let key = rest[..eq]
            .rsplit(|c: char| c.is_whitespace())
            .next()
            .unwrap_or("")
            .to_string();
        let after = &rest[eq + 2..];
        let Some(close) = after.find('"') else { break };
        attrs.push((key, unescape_xml(&after[..close])));
        rest = &after[close + 1..];
    }
    attrs
}

/// Parses `uiautomator dump` XML into its nodes (attribute values never contain raw quotes).
pub(crate) fn parse_ui_nodes(xml: &str) -> Vec<UiNode> {
    let mut nodes = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<node") {
        rest = &rest[start + 5..];
        let mut end = rest.len();
        let mut in_quote = false;
        for (i, c) in rest.char_indices() {
            match c {
                '"' => in_quote = !in_quote,
                '>' if !in_quote => {
                    end = i;
                    break;
                }
                _ => {}
            }
        }
        let mut node = UiNode::default();
        for (key, value) in xml_attributes(&rest[..end]) {
            match key.as_str() {
                "text" => node.text = value.trim().to_string(),
                "content-desc" => node.desc = value.trim().to_string(),
                "class" => node.class = value,
                "package" => node.package = value,
                "clickable" => node.clickable = value == "true",
                "bounds" => node.bounds = value,
                _ => {}
            }
        }
        nodes.push(node);
        rest = &rest[end..];
    }
    nodes
}

/// Adds the visible phone UI to the spec: structure lines and checkable labels.
pub(crate) fn describe_ui(spec: &mut CleanRoomSpec, nodes: &[UiNode]) {
    let mut tappable = Vec::new();
    let mut labels = Vec::new();
    for node in nodes.iter().filter(|n| !n.label().is_empty()) {
        let class = node.class.rsplit('.').next().unwrap_or(&node.class);
        if spec.ui_structure.len() < 80 {
            spec.ui_structure.push(format!(
                "{class} \"{}\" at {}{}",
                node.label(),
                node.bounds,
                if node.clickable { " (tappable)" } else { "" }
            ));
        }
        let label = node.label().to_string();
        // Only short labels make reliable mechanical checks.
        if label.chars().count() <= 60 {
            let bucket = if node.clickable {
                &mut tappable
            } else {
                &mut labels
            };
            if !bucket.contains(&label) {
                bucket.push(label);
            }
        }
    }
    for (id, area, what, items) in [
        (
            "REQ-PHONE-TAPPABLE",
            "interaction",
            "These tappable controls are visible",
            tappable,
        ),
        (
            "REQ-PHONE-TEXT",
            "content",
            "These labels are visible on screen",
            labels,
        ),
    ] {
        if !items.is_empty() {
            let items: Vec<String> = items.into_iter().take(12).collect();
            spec.requirements.push(
                craft_core::BehavioralRequirement::new(
                    id,
                    area,
                    format!("{what}: {}", items.join(", ")),
                )
                .expecting(items),
            );
        }
    }
}

/// Undoes the `\n` -> `\r\n` translation an old `adb shell` pty applies to binary output.
pub(crate) fn undo_pty_newlines(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\r' && bytes.get(i + 1) == Some(&b'\n') {
            i += 1;
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

/// Runs a passive device command via `exec-out`; devices older than Android 5 lack it, so fall
/// back to `shell` and repair its newline translation when a PNG shows it.
async fn adb_output(adb: &Path, serial: &str, command: &[&str]) -> Result<Vec<u8>, String> {
    let with = |mode: &'static str| [&["-s", serial, mode][..], command].concat();
    match run_tool(adb, &with("exec-out"), 20).await {
        Ok(out) => Ok(out),
        Err(_) => {
            let out = run_tool(adb, &with("shell"), 20).await?;
            Ok(if out.starts_with(b"\x89PNG\r\r\n") {
                undo_pty_newlines(&out)
            } else {
                out
            })
        }
    }
}

/// Captures what the connected phone shows right now. Never starts or touches the app.
pub(crate) async fn capture_phone(
    probe: &mut Probe<'_>,
    spec: &mut CleanRoomSpec,
    expected_package: Option<&str>,
) -> Result<usize, CraftError> {
    let advice = "install Android platform-tools (adb), connect the phone with USB debugging on, open the app, and re-run; or pass a folder of screenshots";
    let Some(adb) = tool("adb") else {
        probe.warn(format!(
            "Phone capture skipped: adb is not installed; {advice}"
        ));
        spec.open_questions
            .push("No phone was captured; the app's screens are unknown.".into());
        return Ok(0);
    };
    let devices = match run_tool(&adb, &["devices"], 10).await {
        Ok(out) => parse_adb_devices(&String::from_utf8_lossy(&out)),
        Err(err) => {
            probe.warn(format!("Phone capture skipped: {err}; {advice}"));
            Vec::new()
        }
    };
    let Some(serial) = devices.first() else {
        probe.warn(format!(
            "Phone capture skipped: no authorized device in adb devices; {advice}"
        ));
        spec.open_questions
            .push("No phone was connected; the app's screens are unknown.".into());
        return Ok(0);
    };
    probe.info(format!(
        "Capturing whatever is on screen of device {serial} right now (the app is not started for you; open it first)"
    ));
    let mut saved = 0;
    match adb_output(&adb, serial, &["screencap", "-p"]).await {
        Ok(png) if png.starts_with(b"\x89PNG") => {
            probe.save(
                "screenshot-device",
                EvidenceKind::Screenshot,
                &format!("Current screen of device {serial}"),
                "screenshot_device.png",
                &png,
                true,
            )?;
            saved += 1;
        }
        Ok(_) => probe.warn("adb screencap returned no PNG (is the screen locked?)"),
        Err(err) => probe.warn(format!("Screen capture failed: {err}")),
    }
    match adb_output(&adb, serial, &["uiautomator", "dump", "/dev/tty"]).await {
        Ok(out) => {
            let text = String::from_utf8_lossy(&out);
            let xml = match (text.find("<hierarchy"), text.rfind("</hierarchy>")) {
                (Some(a), Some(b)) if b > a => &text[a..b + "</hierarchy>".len()],
                _ => "",
            };
            let nodes = parse_ui_nodes(xml);
            if nodes.is_empty() {
                probe.warn("uiautomator returned no UI hierarchy (secure screen or animation in progress?)");
            } else {
                probe.save(
                    "ui-hierarchy-device",
                    EvidenceKind::DomSnapshot,
                    "Visible UI hierarchy (uiautomator)",
                    "ui_hierarchy_device.xml",
                    xml.as_bytes(),
                    true,
                )?;
                let mut shown: Vec<&str> = nodes
                    .iter()
                    .map(|n| n.package.as_str())
                    .filter(|p| !p.is_empty())
                    .collect();
                shown.sort_unstable();
                shown.dedup();
                if let Some(expected) = expected_package
                    && !shown.contains(&expected)
                {
                    probe.warn(format!(
                        "The phone shows {}, not {expected}; open {expected} and re-run for a matching capture",
                        shown.join(", ")
                    ));
                    spec.open_questions.push(format!(
                        "The captured screen belongs to {}, not {expected}.",
                        shown.join(", ")
                    ));
                }
                describe_ui(spec, &nodes);
                probe.info(format!("Captured {} UI nodes", nodes.len()));
            }
        }
        Err(err) => probe.warn(format!("UI hierarchy dump failed: {err}")),
    }
    spec.open_questions.push(
        "Only the screen shown at capture time was observed; describe the other screens and flows."
            .into(),
    );
    Ok(saved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pty_newline_translation_is_undone() {
        assert_eq!(
            undo_pty_newlines(b"\x89PNG\r\r\n\x1a\r\n\r"),
            b"\x89PNG\r\n\x1a\n\r"
        );
    }

    #[test]
    fn adb_devices_lists_only_ready_devices() {
        let out = "List of devices attached\nR58M123\tdevice\nemulator-5554\toffline\nABC\tunauthorized\n\n";
        assert_eq!(parse_adb_devices(out), vec!["R58M123".to_string()]);
    }

    #[test]
    fn uiautomator_xml_is_parsed_into_nodes() {
        let xml = r#"<?xml version='1.0' encoding='UTF-8' standalone='yes' ?><hierarchy rotation="0"><node index="0" text="" resource-id="" class="android.widget.FrameLayout" package="com.example.app" content-desc="" clickable="false" bounds="[0,0][1080,2400]"><node index="0" text="Sign in &amp; go" class="android.widget.Button" package="com.example.app" content-desc="" clickable="true" bounds="[40,900][1040,1040]" /><node index="1" text="" class="android.widget.ImageView" package="com.example.app" content-desc="Profile &quot;me&quot;" clickable="false" bounds="[0,0][90,90]" /></node></hierarchy>"#;
        let nodes = parse_ui_nodes(xml);
        assert_eq!(nodes.len(), 3);
        assert_eq!(nodes[1].text, "Sign in & go");
        assert!(nodes[1].clickable);
        assert_eq!(nodes[1].bounds, "[40,900][1040,1040]");
        assert_eq!(nodes[2].label(), "Profile \"me\"");
        assert_eq!(nodes[0].package, "com.example.app");

        let mut spec =
            CleanRoomSpec::new(craft_core::TargetKind::PhoneApp, "x".into(), String::new());
        describe_ui(&mut spec, &nodes);
        assert!(
            spec.ui_structure
                .iter()
                .any(|l| l == "Button \"Sign in & go\" at [40,900][1040,1040] (tappable)")
        );
        let tappable = spec
            .requirements
            .iter()
            .find(|r| r.id == "REQ-PHONE-TAPPABLE")
            .unwrap();
        assert_eq!(tappable.expect_text, vec!["Sign in & go".to_string()]);
    }

    #[test]
    fn window_lookup_matches_class_and_respects_visibility() {
        let clients = serde_json::json!([
            {"class": "kitty", "initialClass": "kitty", "title": "shell", "at": [0, 0], "size": [800, 600], "workspace": {"id": 1}},
            {"class": "org.gnome.Calculator", "initialClass": "org.gnome.Calculator", "title": "Calculator", "at": [10, 20], "size": [400, 500], "workspace": {"id": 2}}
        ]);
        let needles = vec!["calculator".to_string()];
        match find_window(&clients, &[2], &needles) {
            WindowMatch::Visible(w) => assert_eq!(w.geometry, "10,20 400x500"),
            _ => panic!("expected a visible window"),
        }
        assert!(matches!(
            find_window(&clients, &[1], &needles),
            WindowMatch::Hidden { workspace: 2, .. }
        ));
        assert!(matches!(
            find_window(&clients, &[1, 2], &["nope".into()]),
            WindowMatch::None
        ));
    }
}
