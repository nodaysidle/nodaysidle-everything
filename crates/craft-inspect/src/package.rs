//! Deep mode only: APK / IPA package structure, the binary `AndroidManifest.xml` (AXML) and
//! the IPA's `Info.plist`. Reads the package as a zip; never installs or runs it.

use crate::binary::extract_printable_strings;
use crate::capture::unescape_xml;
use crate::file::{zip_entries, zip_read};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::path::Path;

const MANIFEST_CAP: u64 = 4 * 1024 * 1024;

/// Bucket for counting package entries: `res/`, `lib/<abi>/`, `classes*.dex`, `Payload/X.app/<dir>/` ...
fn bucket(name: &str) -> String {
    let parts: Vec<&str> = name.split('/').collect();
    match parts.as_slice() {
        ["lib", abi, _, ..] => format!("lib/{abi}/"),
        [file] if file.starts_with("classes") && file.ends_with(".dex") => "classes*.dex".into(),
        ["Payload", app, rest @ ..] if app.ends_with(".app") => match rest {
            [dir, _, ..] => format!("Payload/{app}/{dir}/"),
            _ => format!("Payload/{app}/ (files)"),
        },
        [dir, _, ..] => format!("{dir}/"),
        _ => "(root files)".into(),
    }
}

/// Entry counts by top-level folder plus the decoded manifest of an `.apk` or `.ipa`.
pub(crate) fn inspect_package(path: &Path) -> Result<Value, String> {
    let entries = zip_entries(path)?;
    let mut buckets: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    for (name, size, _) in entries.iter().filter(|e| !e.2) {
        let slot = buckets.entry(bucket(name)).or_default();
        slot.0 += 1;
        slot.1 += size;
    }
    let mut out = json!({
        "entry_count": entries.len(),
        "by_folder": buckets.into_iter().map(|(k, (n, b))| (k, json!({"count": n, "bytes": b}))).collect::<Map<_, _>>(),
    });
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ext == "apk" {
        let raw = zip_read(path, "AndroidManifest.xml", MANIFEST_CAP)
            .ok_or("AndroidManifest.xml missing")?;
        out["manifest"] = match decode_axml(&raw) {
            Ok(doc) => android_manifest(&doc),
            Err(err) => {
                json!({ "decode_error": err, "strings_sample": extract_printable_strings(&raw, 4, 64) })
            }
        };
    } else if ext == "ipa" {
        let plist_name = entries.iter().map(|e| &e.0).find(|n| {
            let parts: Vec<&str> = n.split('/').collect();
            parts.len() == 3
                && parts[0] == "Payload"
                && parts[1].ends_with(".app")
                && parts[2] == "Info.plist"
        });
        if let Some(raw) = plist_name.and_then(|n| zip_read(path, n, MANIFEST_CAP)) {
            out["info_plist"] = if raw.starts_with(b"bplist") {
                // shortcut: binary plists are only sampled as strings; add a bplist decoder if exact keys matter.
                json!({ "binary_plist_strings": extract_printable_strings(&raw, 4, 100) })
            } else {
                json!(plist_strings(&String::from_utf8_lossy(&raw)))
            };
        }
    }
    Ok(out)
}

/// A decoded binary XML document: the string pool and each start element with its attributes.
#[derive(Debug, Default)]
pub(crate) struct Axml {
    pub strings: Vec<String>,
    pub elements: Vec<(String, Vec<(String, String)>)>,
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn decode_string_pool(b: &[u8], chunk: usize) -> Option<Vec<String>> {
    let count = u32_at(b, chunk + 8)? as usize;
    let utf8 = u32_at(b, chunk + 16)? & 0x100 != 0;
    let strings_start = chunk + u32_at(b, chunk + 20)? as usize;
    let header_size = u16_at(b, chunk + 2)? as usize;
    let mut out = Vec::with_capacity(count.min(100_000));
    for i in 0..count.min(100_000) {
        let mut at = strings_start + u32_at(b, chunk + header_size + i * 4)? as usize;
        let s = if utf8 {
            // UTF-16 length then UTF-8 length, each 1 or 2 bytes (high bit = two-byte form).
            let skip_len = |at: &mut usize| -> Option<usize> {
                let first = *b.get(*at)? as usize;
                *at += 1;
                if first & 0x80 != 0 {
                    let second = *b.get(*at)? as usize;
                    *at += 1;
                    Some(((first & 0x7f) << 8) | second)
                } else {
                    Some(first)
                }
            };
            skip_len(&mut at)?;
            let len = skip_len(&mut at)?;
            String::from_utf8_lossy(b.get(at..at + len)?).into_owned()
        } else {
            let mut len = u16_at(b, at)? as usize;
            at += 2;
            if len & 0x8000 != 0 {
                len = ((len & 0x7fff) << 16) | u16_at(b, at)? as usize;
                at += 2;
            }
            let units: Vec<u16> = (0..len)
                .map(|k| u16_at(b, at + k * 2))
                .collect::<Option<_>>()?;
            String::from_utf16_lossy(&units)
        };
        out.push(s);
    }
    Some(out)
}

/// Decodes Android binary XML: string pool chunk (0x0001) and start-element chunks (0x0102).
pub(crate) fn decode_axml(b: &[u8]) -> Result<Axml, String> {
    if u16_at(b, 0) != Some(0x0003) {
        return Err("not an Android binary XML document".into());
    }
    let mut doc = Axml::default();
    let mut at = u16_at(b, 2).ok_or("truncated header")? as usize;
    while at + 8 <= b.len() {
        let kind = u16_at(b, at).ok_or("truncated chunk")?;
        let header_size = u16_at(b, at + 2).ok_or("truncated chunk")? as usize;
        let size = u32_at(b, at + 4).ok_or("truncated chunk")? as usize;
        if size < 8 {
            return Err(format!("invalid chunk size {size} at {at}"));
        }
        let string = |i: u32| doc.strings.get(i as usize).cloned().unwrap_or_default();
        match kind {
            0x0001 => doc.strings = decode_string_pool(b, at).ok_or("malformed string pool")?,
            0x0102 => {
                let body = at + header_size;
                let name = string(u32_at(b, body + 4).ok_or("truncated element")?);
                let attr_start = u16_at(b, body + 8).ok_or("truncated element")? as usize;
                let attr_size = u16_at(b, body + 10).ok_or("truncated element")? as usize;
                let attr_count = u16_at(b, body + 12).ok_or("truncated element")? as usize;
                let mut attrs = Vec::with_capacity(attr_count);
                for k in 0..attr_count {
                    let a = body + attr_start + k * attr_size;
                    let attr_name = string(u32_at(b, a + 4).ok_or("truncated attribute")?);
                    let raw = u32_at(b, a + 8).ok_or("truncated attribute")?;
                    let data_type = *b.get(a + 15).ok_or("truncated attribute")?;
                    let data = u32_at(b, a + 16).ok_or("truncated attribute")?;
                    let value = if raw != u32::MAX {
                        string(raw)
                    } else if data_type == 0x03 {
                        string(data)
                    } else if data_type == 0x12 {
                        (data != 0).to_string()
                    } else {
                        format!("{data:#x}")
                    };
                    attrs.push((attr_name, value));
                }
                doc.elements.push((name, attrs));
            }
            _ => {}
        }
        at += size;
    }
    if doc.strings.is_empty() {
        return Err("no string pool found".into());
    }
    Ok(doc)
}

/// Package name, permissions and components from a decoded `AndroidManifest.xml`.
pub(crate) fn android_manifest(doc: &Axml) -> Value {
    let attr = |attrs: &[(String, String)], key: &str| {
        attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    };
    let mut package = None;
    let mut permissions = Vec::new();
    let mut components: Vec<String> = Vec::new();
    for (name, attrs) in &doc.elements {
        match name.as_str() {
            "manifest" => package = attr(attrs, "package"),
            "uses-permission" | "uses-permission-sdk-23" => permissions.extend(attr(attrs, "name")),
            "activity" | "activity-alias" | "service" | "receiver" | "provider" => {
                if let Some(n) = attr(attrs, "name") {
                    components.push(format!("{name}: {n}"));
                }
            }
            _ => {}
        }
    }
    components.truncate(200);
    json!({ "package": package, "permissions": permissions, "components": components, "string_pool_size": doc.strings.len() })
}

/// `<key>K</key><string>V</string>` pairs of an XML property list.
pub(crate) fn plist_strings(xml: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut rest = xml;
    while let Some(k) = rest.find("<key>") {
        rest = &rest[k + 5..];
        let Some(end) = rest.find("</key>") else {
            break;
        };
        let key = rest[..end].trim().to_string();
        rest = &rest[end + 6..];
        let trimmed = rest.trim_start();
        if let Some(value) = trimmed.strip_prefix("<string>")
            && let Some(close) = value.find("</string>")
        {
            out.insert(key, unescape_xml(&value[..close]));
            if out.len() >= 200 {
                break;
            }
        }
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Builds a minimal UTF-16 AXML document: one string pool and the given start elements,
    /// each attribute as (name, string value).
    pub(crate) fn build_axml(elements: &[(&str, &[(&str, &str)])]) -> Vec<u8> {
        let mut strings: Vec<String> = Vec::new();
        let mut index = |s: &str| -> u32 {
            if let Some(i) = strings.iter().position(|x| x == s) {
                return i as u32;
            }
            strings.push(s.to_string());
            (strings.len() - 1) as u32
        };
        let resolved: Vec<(u32, Vec<(u32, u32)>)> = elements
            .iter()
            .map(|(name, attrs)| {
                (
                    index(name),
                    attrs.iter().map(|(k, v)| (index(k), index(v))).collect(),
                )
            })
            .collect();

        let mut data = Vec::new();
        let mut offsets = Vec::new();
        for s in &strings {
            offsets.push(data.len() as u32);
            let units: Vec<u16> = s.encode_utf16().collect();
            data.extend((units.len() as u16).to_le_bytes());
            units.iter().for_each(|u| data.extend(u.to_le_bytes()));
            data.extend([0, 0]);
        }
        while data.len() % 4 != 0 {
            data.push(0);
        }
        let header = 28usize;
        let strings_start = header + offsets.len() * 4;
        let mut pool = Vec::new();
        pool.extend(1u16.to_le_bytes());
        pool.extend((header as u16).to_le_bytes());
        pool.extend(((strings_start + data.len()) as u32).to_le_bytes());
        pool.extend((strings.len() as u32).to_le_bytes());
        pool.extend(0u32.to_le_bytes()); // styles
        pool.extend(0u32.to_le_bytes()); // flags: UTF-16
        pool.extend((strings_start as u32).to_le_bytes());
        pool.extend(0u32.to_le_bytes());
        offsets.iter().for_each(|o| pool.extend(o.to_le_bytes()));
        pool.extend(&data);

        let mut body = pool;
        for (name, attrs) in resolved {
            let mut el = Vec::new();
            el.extend(0x0102u16.to_le_bytes());
            el.extend(16u16.to_le_bytes());
            el.extend(((16 + 20 + attrs.len() * 20) as u32).to_le_bytes());
            el.extend(1u32.to_le_bytes()); // line
            el.extend(u32::MAX.to_le_bytes()); // comment
            el.extend(u32::MAX.to_le_bytes()); // ns
            el.extend(name.to_le_bytes());
            el.extend(20u16.to_le_bytes()); // attributeStart
            el.extend(20u16.to_le_bytes()); // attributeSize
            el.extend((attrs.len() as u16).to_le_bytes());
            el.extend([0u8; 6]); // id/class/style indexes
            for (k, v) in attrs {
                el.extend(u32::MAX.to_le_bytes()); // ns
                el.extend(k.to_le_bytes());
                el.extend(v.to_le_bytes()); // raw value
                el.extend(8u16.to_le_bytes());
                el.push(0);
                el.push(0x03); // TYPE_STRING
                el.extend(v.to_le_bytes());
            }
            body.extend(el);
        }
        let mut doc = Vec::new();
        doc.extend(3u16.to_le_bytes());
        doc.extend(8u16.to_le_bytes());
        doc.extend(((8 + body.len()) as u32).to_le_bytes());
        doc.extend(body);
        doc
    }

    #[test]
    fn axml_string_pool_and_manifest_are_decoded() {
        let raw = build_axml(&[
            ("manifest", &[("package", "com.example.notes")]),
            ("uses-permission", &[("name", "android.permission.CAMERA")]),
            ("activity", &[("name", "com.example.notes.MainActivity")]),
        ]);
        let doc = decode_axml(&raw).unwrap();
        assert!(
            doc.strings
                .contains(&"android.permission.CAMERA".to_string())
        );
        let manifest = android_manifest(&doc);
        assert_eq!(manifest["package"], "com.example.notes");
        assert_eq!(
            manifest["permissions"],
            json!(["android.permission.CAMERA"])
        );
        assert_eq!(
            manifest["components"],
            json!(["activity: com.example.notes.MainActivity"])
        );
        assert!(decode_axml(b"<?xml").is_err());
        assert!(
            decode_axml(&raw[..raw.len() - 10]).is_err(),
            "truncated attribute must not decode"
        );
    }

    #[test]
    fn plist_pairs_and_buckets() {
        let plist = "<dict><key>CFBundleIdentifier</key>\n<string>com.example.App</string><key>LSRequiresIPhoneOS</key><true/><key>CFBundleName</key><string>A &amp; B</string></dict>";
        let pairs = plist_strings(plist);
        assert_eq!(pairs["CFBundleIdentifier"], "com.example.App");
        assert_eq!(pairs["CFBundleName"], "A & B");
        assert!(!pairs.contains_key("LSRequiresIPhoneOS"));
        assert_eq!(bucket("lib/arm64-v8a/libfoo.so"), "lib/arm64-v8a/");
        assert_eq!(bucket("classes2.dex"), "classes*.dex");
        assert_eq!(bucket("res/layout/main.xml"), "res/");
        assert_eq!(bucket("Payload/X.app/Assets.car"), "Payload/X.app/ (files)");
        assert_eq!(
            bucket("Payload/X.app/Frameworks/A.framework/A"),
            "Payload/X.app/Frameworks/"
        );
    }
}
