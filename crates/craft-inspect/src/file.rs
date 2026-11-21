//! File study: public structure and schema of a local file or directory (Clean Room), plus
//! header bytes, strings, binary structure and archive manifests when Deep mode is authorized.

use crate::Probe;
use crate::binary;
use crate::capture::{run_tool, tool};
use craft_core::{
    BehavioralRequirement, CleanRoomSpec, CraftError, CraftRequest, DeepFindings, EventSink,
    EvidenceKind, EvidenceStore, StudyReport, TargetLocation,
};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Most bytes read from a file for sniffing and analysis.
const READ_CAP: u64 = 64 * 1024 * 1024;
const LISTING_CAP: usize = 300;
const EXPECT_CAP: usize = 12;

pub async fn inspect(
    request: &CraftRequest,
    store: &EvidenceStore,
    events: &EventSink,
    run_id: Uuid,
) -> Result<StudyReport, CraftError> {
    let path = match &request.location {
        TargetLocation::Path(p) => p.clone(),
        TargetLocation::Url(u) => {
            return Err(CraftError::InvalidTarget(format!(
                "file inspector expects a local path, got URL: {u}"
            )));
        }
    };
    if !path.exists() {
        return Err(CraftError::InvalidTarget(format!(
            "{} does not exist",
            path.display()
        )));
    }
    let mut probe = Probe {
        store,
        events,
        run_id,
        source: "inspect::file",
        evidence_ids: Vec::new(),
    };

    let blocking_path = path.clone();
    let mut meta = tokio::task::spawn_blocking(move || analyze(&blocking_path))
        .await
        .map_err(|e| CraftError::InspectionFailed(format!("file analysis panicked: {e}")))??;

    let mut open_questions = Vec::new();
    if meta["format"] == "sqlite" {
        match sqlite_schema(&path).await {
            Ok(tables) => meta["sqlite"] = json!({ "tables": tables }),
            Err(err) => {
                probe.warn(format!("SQLite schema skipped: {err}"));
                open_questions.push(
                    "The SQLite schema could not be read (install the sqlite3 CLI and re-run)."
                        .into(),
                );
            }
        }
    }
    probe.save(
        "file-metadata",
        EvidenceKind::FileMetadata,
        "Observed file structure",
        "file_metadata.json",
        &serde_json::to_vec_pretty(&meta)?,
        true,
    )?;
    probe.info(format!(
        "Observed {} ({})",
        meta["name"].as_str().unwrap_or(""),
        meta["format"].as_str().unwrap_or("")
    ));

    let mut spec = CleanRoomSpec::new(
        request.target_kind,
        path.display().to_string(),
        String::new(),
    );
    describe(&mut spec, &meta, &request.prompt);
    spec.open_questions.extend(open_questions);

    let deep_findings = if request.depth.is_deep() {
        probe.info("Deep mode authorized: reading header bytes, strings and internal structure");
        let deep_path = path.clone();
        let deep = tokio::task::spawn_blocking(move || deep_analysis(&deep_path))
            .await
            .map_err(|e| CraftError::InspectionFailed(format!("deep analysis panicked: {e}")))??;
        probe.save(
            "binary-inspection",
            EvidenceKind::BinaryInspection,
            "Header bytes, strings and internal structure (deep)",
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

/// One entry of a capped directory walk, path relative to the root.
pub(crate) struct WalkEntry {
    pub path: String,
    pub size: u64,
    pub is_dir: bool,
}

/// Recursive listing without following symlinks; stops after `max_entries`.
pub(crate) fn walk(root: &Path, max_entries: usize) -> (Vec<WalkEntry>, bool) {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut children: Vec<PathBuf> = read.filter_map(|e| e.ok().map(|e| e.path())).collect();
        children.sort();
        for child in children {
            if out.len() >= max_entries {
                return (out, true);
            }
            let Ok(meta) = std::fs::symlink_metadata(&child) else {
                continue;
            };
            let rel = child
                .strip_prefix(root)
                .unwrap_or(&child)
                .to_string_lossy()
                .replace('\\', "/");
            if meta.is_dir() {
                stack.push(child);
            }
            out.push(WalkEntry {
                path: rel,
                size: meta.len(),
                is_dir: meta.is_dir(),
            });
        }
    }
    (out, false)
}

fn extension_of(name: &str) -> String {
    let base = name.rsplit('/').next().unwrap_or(name);
    match base.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => ext.to_ascii_lowercase(),
        _ => "(none)".into(),
    }
}

/// Entry listing plus counts by extension, from `(name, size, is_dir)` items.
pub(crate) fn summarize_entries(entries: impl IntoIterator<Item = (String, u64, bool)>) -> Value {
    let mut listing = Vec::new();
    let mut by_ext: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    let (mut files, mut dirs, mut total) = (0u64, 0u64, 0u64);
    for (name, size, is_dir) in entries {
        if is_dir {
            dirs += 1;
        } else {
            files += 1;
            total += size;
            let slot = by_ext.entry(extension_of(&name)).or_default();
            slot.0 += 1;
            slot.1 += size;
        }
        if listing.len() < LISTING_CAP {
            listing.push(json!({ "name": name, "size": size }));
        }
    }
    json!({
        "file_count": files,
        "dir_count": dirs,
        "total_bytes": total,
        "listing_truncated": files + dirs > LISTING_CAP as u64,
        "entries": listing,
        "by_extension": by_ext.into_iter().map(|(k, (n, b))| (k, json!({"count": n, "bytes": b}))).collect::<Map<_, _>>(),
    })
}

pub(crate) fn zip_entries(path: &Path) -> Result<Vec<(String, u64, bool)>, String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
    let mut out = Vec::with_capacity(archive.len());
    for i in 0..archive.len() {
        let entry = archive.by_index_raw(i).map_err(|e| e.to_string())?;
        let name = entry.name().map_err(|e| e.to_string())?.into_owned();
        out.push((name, entry.size(), entry.is_dir()));
    }
    Ok(out)
}

pub(crate) fn zip_listing(path: &Path) -> Result<Value, String> {
    zip_entries(path).map(summarize_entries)
}

/// Reads one zip entry, at most `cap` bytes.
pub(crate) fn zip_read(path: &Path, name: &str, cap: u64) -> Option<Vec<u8>> {
    let file = std::fs::File::open(path).ok()?;
    let mut archive = zip::ZipArchive::new(file).ok()?;
    let entry = archive.by_name(name).ok()?;
    let mut bytes = Vec::new();
    entry.take(cap).read_to_end(&mut bytes).ok()?;
    Some(bytes)
}

/// Streams tar headers (optionally gzip-compressed); never extracts.
pub(crate) fn tar_listing(path: &Path, gzipped: bool) -> Result<Value, String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let reader: Box<dyn Read> = if gzipped {
        Box::new(flate2::read::GzDecoder::new(file))
    } else {
        Box::new(file)
    };
    let mut archive = tar::Archive::new(reader);
    let mut out = Vec::new();
    for entry in archive.entries().map_err(|e| e.to_string())?.take(100_000) {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry
            .path()
            .map_err(|e| e.to_string())?
            .to_string_lossy()
            .into_owned();
        out.push((name, entry.size(), entry.header().entry_type().is_dir()));
    }
    Ok(summarize_entries(out))
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Shape of a JSON value: object keys with value types, `depth` levels deep; arrays by element type.
pub(crate) fn json_shape(v: &Value, depth: usize) -> Value {
    match v {
        Value::Object(map) if depth > 0 => Value::Object(
            map.iter()
                .take(40)
                .map(|(k, v)| (k.clone(), json_shape(v, depth - 1)))
                .collect(),
        ),
        Value::Array(items) => {
            let mut kinds: Vec<&str> = items.iter().take(1000).map(type_name).collect();
            kinds.sort();
            kinds.dedup();
            match (kinds.as_slice(), items.first()) {
                ([], _) => json!("array (empty)"),
                ([_], Some(first @ (Value::Object(_) | Value::Array(_)))) if depth > 0 => {
                    json!({ "array_of": json_shape(first, depth - 1), "length": items.len() })
                }
                _ => json!(format!(
                    "array<{}> ({} items)",
                    kinds.join("|"),
                    items.len()
                )),
            }
        }
        other => json!(type_name(other)),
    }
}

/// Visible names of a JSON document: top-level keys, or the keys of the first record of a top-level array.
fn json_top_keys(v: &Value) -> Vec<String> {
    match v {
        Value::Object(map) => map.keys().cloned().collect(),
        Value::Array(items) => items
            .first()
            .and_then(Value::as_object)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn split_csv_line(line: &str, delimiter: char) -> Vec<String> {
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                field.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            c if c == delimiter && !quoted => {
                fields.push(std::mem::take(&mut field).trim().to_string())
            }
            c => field.push(c),
        }
    }
    fields.push(field.trim().to_string());
    fields
}

/// Delimiter, header columns and data row count of CSV/TSV text.
pub(crate) fn csv_shape(text: &str, tsv: bool) -> Value {
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let header = lines.next().unwrap_or("");
    let delimiter = if tsv {
        '\t'
    } else {
        [',', ';', '\t', '|']
            .into_iter()
            .max_by_key(|d| header.matches(*d).count())
            .unwrap_or(',')
    };
    json!({
        "delimiter": delimiter.to_string(),
        "columns": split_csv_line(header, delimiter),
        // shortcut: counts lines, so quoted fields with embedded newlines over-count; use a CSV parser if that matters.
        "rows": lines.count(),
    })
}

fn text_shape(text: &str) -> Value {
    let head: Vec<String> = text
        .lines()
        .take(5)
        .map(|l| l.chars().take(120).collect())
        .collect();
    json!({ "lines": text.lines().count(), "head": head })
}

/// Table and column schema via the sqlite3 CLI, opened read-only.
async fn sqlite_schema(path: &Path) -> Result<Value, String> {
    let sqlite = tool("sqlite3").ok_or("the sqlite3 CLI is not installed")?;
    let query = "SELECT m.name AS tbl, p.name AS col, p.type AS type FROM sqlite_master m \
                 JOIN pragma_table_info(m.name) p WHERE m.type IN ('table','view') \
                 AND m.name NOT LIKE 'sqlite_%' ORDER BY m.name, p.cid";
    let out = run_tool(
        &sqlite,
        &[
            "-readonly",
            "-batch",
            "-json",
            &path.to_string_lossy(),
            query,
        ],
        15,
    )
    .await?;
    let rows: Value = if out.iter().all(u8::is_ascii_whitespace) {
        json!([])
    } else {
        serde_json::from_slice(&out).map_err(|e| format!("unexpected sqlite3 output: {e}"))?
    };
    let mut tables: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for row in rows.as_array().into_iter().flatten() {
        let column = match row["type"].as_str().filter(|t| !t.is_empty()) {
            Some(t) => format!("{} {t}", row["col"].as_str().unwrap_or("")),
            None => row["col"].as_str().unwrap_or("").to_string(),
        };
        tables
            .entry(row["tbl"].as_str().unwrap_or("").to_string())
            .or_default()
            .push(column);
    }
    Ok(json!(tables))
}

/// Clean Room analysis: public structure of the file or directory (bounded reads).
pub(crate) fn analyze(path: &Path) -> Result<Value, CraftError> {
    let name = path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    let lower = name.to_lowercase();
    if path.is_dir() {
        let (entries, truncated) = walk(path, 5_000);
        let mut tree = summarize_entries(entries.into_iter().map(|e| (e.path, e.size, e.is_dir)));
        tree["walk_truncated"] = json!(truncated);
        return Ok(
            json!({ "name": name, "kind": "directory", "format": "directory", "tree": tree }),
        );
    }

    let (bytes, truncated) = binary::read_capped(path, READ_CAP).map_err(|e| {
        CraftError::InspectionFailed(format!("cannot read {}: {e}", path.display()))
    })?;
    let size = std::fs::metadata(path)
        .map(|m| m.len())
        .unwrap_or(bytes.len() as u64);
    let extension = extension_of(&name);
    let sniffed = infer::get(&bytes);
    let text = match std::str::from_utf8(&bytes) {
        Ok(s) => Some(s),
        // A cut at the read cap may split a character; the rest is still text.
        Err(e) if e.error_len().is_none() => std::str::from_utf8(&bytes[..e.valid_up_to()]).ok(),
        Err(_) => None,
    };
    let mime = sniffed
        .map(|t| t.mime_type().to_string())
        .unwrap_or_else(|| {
            match extension.as_str() {
                "json" => "application/json",
                "csv" => "text/csv",
                "tsv" => "text/tab-separated-values",
                "md" => "text/markdown",
                "html" | "htm" => "text/html",
                _ if text.is_some() => "text/plain",
                _ => "application/octet-stream",
            }
            .to_string()
        });

    let mut meta = json!({
        "name": name, "kind": "file", "size_bytes": size, "extension": extension,
        "mime": mime, "text": text.is_some(), "read_truncated": truncated,
    });
    let format = if bytes.starts_with(b"SQLite format 3\0") {
        "sqlite"
    } else if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        match tar_listing(path, true) {
            Ok(listing) => {
                meta["archive"] = listing;
                "tar.gz"
            }
            Err(_) => "gzip",
        }
    } else if mime == "application/x-tar" {
        meta["archive"] = tar_listing(path, false).unwrap_or_else(|e| json!({ "error": e }));
        "tar"
    } else if bytes.starts_with(b"PK\x03\x04") || bytes.starts_with(b"PK\x05\x06") {
        meta["archive"] = zip_listing(path).unwrap_or_else(|e| json!({ "error": e }));
        "zip"
    } else if mime.starts_with("image/") {
        if let Ok((w, h)) = image::image_dimensions(path) {
            meta["image"] = json!({ "width": w, "height": h });
        }
        "image"
    } else if let Some(text) = text {
        if extension == "json" || (text.trim_start().starts_with(['{', '[']) && !truncated) {
            match serde_json::from_str::<Value>(text) {
                Ok(doc) => {
                    meta["json"] = json!({ "shape": json_shape(&doc, 3), "top_level_keys": json_top_keys(&doc) });
                    "json"
                }
                Err(e) => {
                    meta["json_error"] = json!(if truncated {
                        "too large to parse within the read cap".to_string()
                    } else {
                        e.to_string()
                    });
                    meta["text_shape"] = text_shape(text);
                    "text"
                }
            }
        } else if extension == "csv" || extension == "tsv" {
            meta["csv"] = csv_shape(text, extension == "tsv");
            "csv"
        } else {
            meta["text_shape"] = text_shape(text);
            "text"
        }
    } else if binary::is_executable_magic(&bytes) {
        "executable"
    } else {
        "binary"
    };
    meta["format"] = json!(format);
    Ok(meta)
}

fn strings_of(v: &Value) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s.as_str().map(str::to_string))
        .collect()
}

/// Short, distinct names suitable for mechanical visibility checks.
fn checkable(names: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for n in names {
        if !n.is_empty() && n.chars().count() <= 40 && !out.contains(&n) {
            out.push(n);
        }
    }
    out.truncate(EXPECT_CAP);
    out
}

fn archive_outline(archive: &Value) -> Vec<String> {
    let mut lines = vec![format!(
        "{} files, {} folders, {} bytes uncompressed",
        archive["file_count"], archive["dir_count"], archive["total_bytes"]
    )];
    if let Some(by_ext) = archive["by_extension"].as_object() {
        let mut exts: Vec<_> = by_ext.iter().collect();
        exts.sort_by_key(|(_, v)| std::cmp::Reverse(v["count"].as_u64().unwrap_or(0)));
        lines.push(format!(
            "By extension: {}",
            exts.iter()
                .take(12)
                .map(|(k, v)| format!(".{k} x{}", v["count"]))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    lines.extend(
        archive["entries"]
            .as_array()
            .into_iter()
            .flatten()
            .take(60)
            .map(|e| format!("{} ({} bytes)", e["name"].as_str().unwrap_or(""), e["size"])),
    );
    if archive["listing_truncated"] == true || archive["walk_truncated"] == true {
        lines.push("... listing truncated".into());
    }
    lines
}

/// Turns the observed structure into the clean-room spec.
pub(crate) fn describe(spec: &mut CleanRoomSpec, meta: &Value, prompt: &str) {
    let s = |v: &Value| v.as_str().unwrap_or("").to_string();
    let name = s(&meta["name"]);
    let format = s(&meta["format"]);
    spec.requirements.push(
        BehavioralRequirement::new("REQ-FILE-GOAL", "goal", prompt.to_string())
            .signal("The recreation fulfils the user's request."),
    );
    for key in ["format", "mime", "extension", "size_bytes"] {
        if !meta[key].is_null() {
            let value = meta[key]
                .as_str()
                .map_or_else(|| meta[key].to_string(), str::to_string);
            spec.visual_tokens.insert(format!("file.{key}"), value);
        }
    }
    if meta["read_truncated"] == true {
        spec.open_questions.push(format!(
            "Only the first {} MiB were analyzed; later content may differ.",
            READ_CAP / 1024 / 1024
        ));
    }

    let mut expect: Option<(&str, &str, Vec<String>)> = None;
    match format.as_str() {
        "directory" => {
            let tree = &meta["tree"];
            spec.ui_structure = archive_outline(tree);
            spec.observed_features.push(format!(
                "Folder with {} files in {} subfolders",
                tree["file_count"], tree["dir_count"]
            ));
        }
        "json" => {
            let keys = strings_of(&meta["json"]["top_level_keys"]);
            spec.ui_structure = serde_json::to_string_pretty(&meta["json"]["shape"])
                .unwrap_or_default()
                .lines()
                .take(120)
                .map(str::to_string)
                .collect();
            spec.observed_features.push(format!(
                "JSON document with top-level keys: {}",
                keys.join(", ")
            ));
            expect = Some((
                "REQ-FILE-JSON-KEYS",
                "The output uses these top-level keys",
                keys,
            ));
        }
        "csv" => {
            let csv = &meta["csv"];
            let columns = strings_of(&csv["columns"]);
            spec.visual_tokens
                .insert("csv.delimiter".into(), s(&csv["delimiter"]));
            spec.visual_tokens
                .insert("csv.rows".into(), csv["rows"].to_string());
            spec.ui_structure = vec![
                format!("Header: {}", columns.join(" | ")),
                format!("{} data rows", csv["rows"]),
            ];
            spec.observed_features.push(format!(
                "Delimited table with {} columns and {} rows",
                columns.len(),
                csv["rows"]
            ));
            expect = Some((
                "REQ-FILE-CSV-COLUMNS",
                "The output has these columns",
                columns,
            ));
        }
        "sqlite" => {
            let tables = meta["sqlite"]["tables"]
                .as_object()
                .cloned()
                .unwrap_or_default();
            spec.ui_structure = tables
                .iter()
                .map(|(t, cols)| format!("table {t}({})", strings_of(cols).join(", ")))
                .collect();
            spec.observed_features
                .push(format!("SQLite database with {} table(s)", tables.len()));
            expect = Some((
                "REQ-FILE-SQLITE-TABLES",
                "The database has these tables",
                tables.keys().cloned().collect(),
            ));
        }
        "zip" | "tar" | "tar.gz" => {
            let archive = &meta["archive"];
            spec.ui_structure = archive_outline(archive);
            spec.observed_features.push(format!(
                "{format} archive with {} files",
                archive["file_count"]
            ));
        }
        "image" => {
            let image = &meta["image"];
            if !image.is_null() {
                spec.visual_tokens.insert(
                    "image.size".into(),
                    format!("{}x{}", image["width"], image["height"]),
                );
                spec.observed_features.push(format!(
                    "Image of {}x{} pixels",
                    image["width"], image["height"]
                ));
            }
        }
        "text" => {
            let shape = &meta["text_shape"];
            spec.visual_tokens
                .insert("text.lines".into(), shape["lines"].to_string());
            spec.observed_features
                .push(format!("Text with {} lines", shape["lines"]));
            spec.ui_structure = strings_of(&shape["head"])
                .into_iter()
                .map(|l| format!("| {l}"))
                .collect();
        }
        _ => {
            spec.observed_features
                .push(format!("Binary file ({})", s(&meta["mime"])));
            spec.open_questions.push(
                "The file is binary; its behaviour cannot be observed in Clean Room. Describe what it should do.".into(),
            );
        }
    }
    let size_desc = if meta["kind"] == "directory" {
        "folder".to_string()
    } else {
        format!("{} bytes", meta["size_bytes"])
    };
    spec.overview = format!("Local {format} \"{name}\" ({size_desc}). User goal: {prompt}");
    spec.requirements.push(
        BehavioralRequirement::new(
            "REQ-FILE-FORMAT",
            "output",
            format!("The output is a {format} like the original"),
        )
        .signal("The produced file opens as the same format with the same structure."),
    );
    if let Some((id, what, names)) = expect {
        let names = checkable(names);
        if !names.is_empty() {
            spec.requirements.push(
                BehavioralRequirement::new(
                    id,
                    "structure",
                    format!("{what}: {}", names.join(", ")),
                )
                .expecting(names),
            );
        }
    }
}

/// Deep only: header bytes, strings sample, binary structure and archive manifests.
fn deep_analysis(path: &Path) -> Result<Value, CraftError> {
    if path.is_dir() {
        return Ok(
            json!({ "note": "directory target: see the clean-room tree; no single file to decode" }),
        );
    }
    let (bytes, truncated) = binary::read_capped(path, binary::BINARY_CAP).map_err(|e| {
        CraftError::InspectionFailed(format!("cannot read {}: {e}", path.display()))
    })?;
    let magic: Vec<String> = bytes.iter().take(16).map(|b| format!("{b:02x}")).collect();
    let mut deep = json!({
        "magic_hex_16": magic.join(" "),
        "byte_len": bytes.len(),
        "read_truncated": truncated,
        "strings_sample": binary::extract_printable_strings(&bytes, 6, 64),
    });
    if let Some(analysis) = binary::analyze(&bytes) {
        deep["binary"] = analysis;
    }
    if bytes.starts_with(b"PK\x03\x04")
        && let Ok(entries) = zip_entries(path)
    {
        let mut manifests = Map::new();
        for (name, _, _) in entries
            .iter()
            .filter(|(n, _, _)| is_manifest_like(n))
            .take(8)
        {
            if let Some(raw) = zip_read(path, name, 64 * 1024) {
                let content = match String::from_utf8(raw.clone()) {
                    Ok(text) => json!(text.chars().take(4096).collect::<String>()),
                    Err(_) => json!(binary::extract_printable_strings(&raw, 4, 64)),
                };
                manifests.insert(name.clone(), content);
            }
        }
        deep["archive_manifests"] = Value::Object(manifests);
    }
    Ok(deep)
}

fn is_manifest_like(name: &str) -> bool {
    let base = name.rsplit('/').next().unwrap_or(name).to_lowercase();
    [
        "manifest.mf",
        "androidmanifest.xml",
        "info.plist",
        "package.json",
        "manifest.json",
        "pom.xml",
        "cargo.toml",
        "pyproject.toml",
        "metadata",
    ]
    .contains(&base.as_str())
}

fn deep_findings(deep: &Value) -> DeepFindings {
    let mut findings = DeepFindings::default();
    for key in ["magic_hex_16", "byte_len"] {
        findings.format_details.insert(
            key.into(),
            deep[key]
                .as_str()
                .map_or_else(|| deep[key].to_string(), str::to_string),
        );
    }
    if deep["binary"].is_object() {
        binary::add_findings(&mut findings, "binary", &deep["binary"]);
    } else {
        findings.extracted_symbols = strings_of(&deep["strings_sample"])
            .into_iter()
            .take(32)
            .collect();
    }
    if let Some(manifests) = deep["archive_manifests"].as_object() {
        findings
            .assets_discovered
            .extend(manifests.keys().map(|k| format!("manifest: {k}")));
    }
    if let Some(note) = deep["note"].as_str() {
        findings.notes.push(note.into());
    }
    findings.notes.push(format!(
        "{} printable string(s) sampled (stored in the binary-inspection evidence)",
        deep["strings_sample"].as_array().map_or(0, Vec::len)
    ));
    findings
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_shape_summarizes_keys_and_arrays() {
        let doc = json!({"name": "x", "tags": ["a", "b"], "items": [{"id": 1, "ok": true}], "meta": {"v": null}});
        let shape = json_shape(&doc, 3);
        assert_eq!(shape["name"], "string");
        assert_eq!(shape["tags"], "array<string> (2 items)");
        assert_eq!(shape["items"]["array_of"]["id"], "number");
        assert_eq!(shape["meta"]["v"], "null");
        assert_eq!(json_top_keys(&json!([{"a": 1, "b": 2}])), vec!["a", "b"]);
    }

    #[test]
    fn csv_shape_finds_delimiter_columns_and_rows() {
        let shape = csv_shape(
            "id;\"full name\";email\n1;\"Doe; Jane\";j@x\n2;Bob;b@x\n",
            false,
        );
        assert_eq!(shape["delimiter"], ";");
        assert_eq!(shape["columns"], json!(["id", "full name", "email"]));
        assert_eq!(shape["rows"], 2);
        assert_eq!(
            csv_shape("a\tb\n1\t2\n", true)["columns"],
            json!(["a", "b"])
        );
    }
}
