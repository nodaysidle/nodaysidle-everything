//! Deep mode only: static structure of executables and object files via `goblin`.
//! Never executes the target.

use craft_core::DeepFindings;
use goblin::Object;
use serde_json::{Value, json};
use std::io::Read;
use std::path::{Path, PathBuf};

const LIST_CAP: usize = 200;
/// Largest binary loaded for analysis; goblin needs the whole image in memory.
pub(crate) const BINARY_CAP: u64 = 256 * 1024 * 1024;

/// True when the bytes start with an ELF, PE, or Mach-O magic.
pub(crate) fn is_executable_magic(head: &[u8]) -> bool {
    head.starts_with(b"\x7fELF")
        || head.starts_with(b"MZ")
        || [
            [0xfe, 0xed, 0xfa, 0xce],
            [0xfe, 0xed, 0xfa, 0xcf],
            [0xce, 0xfa, 0xed, 0xfe],
            [0xcf, 0xfa, 0xed, 0xfe],
            [0xca, 0xfe, 0xba, 0xbe],
        ]
        .iter()
        .any(|m| head.starts_with(m))
}

pub(crate) fn file_is_executable(path: &Path) -> bool {
    let mut head = [0u8; 4];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut head))
        .is_ok_and(|()| is_executable_magic(&head))
}

/// Reads up to `cap` bytes of a file.
pub(crate) fn read_capped(path: &Path, cap: u64) -> std::io::Result<(Vec<u8>, bool)> {
    let file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let mut bytes = Vec::with_capacity(len.min(cap) as usize);
    file.take(cap).read_to_end(&mut bytes)?;
    Ok((bytes, len > cap))
}

fn capped<I: IntoIterator<Item = String>>(items: I) -> Vec<String> {
    let mut out: Vec<String> = items.into_iter().filter(|s| !s.is_empty()).collect();
    out.sort();
    out.dedup();
    out.truncate(LIST_CAP);
    out
}

/// Container format, architecture, entry point, sections, libraries, imports and exports.
/// `None` when the bytes are not a recognized executable/object format.
pub(crate) fn analyze(bytes: &[u8]) -> Option<Value> {
    match Object::parse(bytes).ok()? {
        Object::Elf(elf) => {
            let name = |strtab: &goblin::strtab::Strtab, i: usize| {
                strtab.get_at(i).unwrap_or("").to_string()
            };
            Some(json!({
                "format": "ELF",
                "arch": goblin::elf::header::machine_to_str(elf.header.e_machine),
                "bits": if elf.is_64 { 64 } else { 32 },
                "kind": if elf.is_lib { "shared library" } else { "executable" },
                "entry": format!("{:#x}", elf.entry),
                "interpreter": elf.interpreter,
                "sections": elf.section_headers.iter().take(LIST_CAP)
                    .map(|s| json!({"name": name(&elf.shdr_strtab, s.sh_name), "size": s.sh_size}))
                    .collect::<Vec<_>>(),
                "libraries": capped(elf.libraries.iter().map(|l| l.to_string())),
                "imports": capped(elf.dynsyms.iter().filter(|s| s.is_import()).map(|s| name(&elf.dynstrtab, s.st_name))),
                "exports": capped(elf.dynsyms.iter().filter(|s| !s.is_import() && s.st_value != 0 && s.st_bind() != goblin::elf::sym::STB_LOCAL).map(|s| name(&elf.dynstrtab, s.st_name))),
            }))
        }
        Object::PE(pe) => Some(json!({
            "format": "PE",
            "arch": goblin::pe::header::machine_to_str(pe.header.coff_header.machine),
            "bits": if pe.is_64 { 64 } else { 32 },
            "kind": if pe.is_lib { "DLL" } else { "executable" },
            "entry": format!("{:#x}", pe.entry),
            "sections": pe.sections.iter().take(LIST_CAP)
                .map(|s| json!({"name": s.name().unwrap_or(""), "size": s.size_of_raw_data}))
                .collect::<Vec<_>>(),
            "libraries": capped(pe.libraries.iter().map(|l| l.to_string())),
            "imports": capped(pe.imports.iter().map(|i| format!("{}!{}", i.dll, i.name))),
            "exports": capped(pe.exports.iter().filter_map(|e| e.name.map(str::to_string))),
        })),
        Object::Mach(goblin::mach::Mach::Binary(macho)) => Some(macho_json(&macho, None)),
        Object::Mach(goblin::mach::Mach::Fat(fat)) => {
            let arches: Vec<String> = fat
                .iter_arches()
                .filter_map(Result::ok)
                .map(|a| {
                    goblin::mach::constants::cputype::get_arch_name_from_types(
                        a.cputype,
                        a.cpusubtype,
                    )
                    .unwrap_or("unknown")
                    .to_string()
                })
                .collect();
            match fat.get(0).ok()? {
                goblin::mach::SingleArch::MachO(macho) => Some(macho_json(&macho, Some(arches))),
                goblin::mach::SingleArch::Archive(_) => {
                    Some(json!({"format": "Mach-O universal", "arches": arches}))
                }
            }
        }
        Object::Archive(archive) => Some(json!({
            "format": "ar archive",
            "members": capped(archive.members().into_iter().map(str::to_string)),
        })),
        Object::COFF(coff) => Some(json!({
            "format": "COFF",
            "arch": goblin::pe::header::machine_to_str(coff.header.machine),
            "sections": coff.sections.iter().take(LIST_CAP)
                .map(|s| json!({"name": s.name().unwrap_or(""), "size": s.size_of_raw_data}))
                .collect::<Vec<_>>(),
        })),
        _ => None,
    }
}

fn macho_json(macho: &goblin::mach::MachO, universal_arches: Option<Vec<String>>) -> Value {
    json!({
        "format": if universal_arches.is_some() { "Mach-O universal" } else { "Mach-O" },
        "arch": goblin::mach::constants::cputype::get_arch_name_from_types(macho.header.cputype, macho.header.cpusubtype).unwrap_or("unknown"),
        "arches": universal_arches,
        "bits": if macho.is_64 { 64 } else { 32 },
        "entry": format!("{:#x}", macho.entry),
        "sections": macho.segments.iter().take(LIST_CAP)
            .map(|s| json!({"name": s.name().unwrap_or(""), "size": s.filesize}))
            .collect::<Vec<_>>(),
        "libraries": capped(macho.libs.iter().map(|l| l.to_string())),
        "imports": capped(macho.imports().unwrap_or_default().into_iter().map(|i| i.name.to_string())),
        "exports": capped(macho.exports().unwrap_or_default().into_iter().map(|e| e.name)),
    })
}

/// Folds a binary analysis into the deep findings.
pub(crate) fn add_findings(findings: &mut DeepFindings, label: &str, analysis: &Value) {
    let details = &mut findings.format_details;
    for key in ["format", "arch", "bits", "kind", "entry", "interpreter"] {
        match &analysis[key] {
            Value::Null => {}
            Value::String(s) => {
                details.insert(format!("{label}.{key}"), s.clone());
            }
            other => {
                details.insert(format!("{label}.{key}"), other.to_string());
            }
        }
    }
    let list = |k: &str| -> Vec<String> {
        analysis[k]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect()
    };
    let libraries = list("libraries");
    if !libraries.is_empty() {
        details.insert(format!("{label}.libraries"), libraries.join(", "));
    }
    let sections = analysis["sections"].as_array().map_or(0, Vec::len);
    let (imports, exports) = (list("imports"), list("exports"));
    findings.notes.push(format!(
        "{label}: {} binary with {sections} section(s)/segment(s), {} linked librar(ies), {} import(s), {} export(s) (each list capped at {LIST_CAP})",
        analysis["format"].as_str().unwrap_or("unknown"),
        libraries.len(),
        imports.len(),
        exports.len()
    ));
    findings
        .extracted_symbols
        .extend(imports.into_iter().map(|s| format!("import: {s}")));
    findings
        .extracted_symbols
        .extend(exports.into_iter().map(|s| format!("export: {s}")));
}

/// Picks the largest executable-looking file under `dir` (walk capped).
pub(crate) fn largest_executable(dir: &Path) -> Option<PathBuf> {
    let (entries, _) = crate::file::walk(dir, 5_000);
    entries
        .into_iter()
        .filter(|e| !e.is_dir)
        .map(|e| (e.size, dir.join(&e.path)))
        .filter(|(_, p)| file_is_executable(p))
        .max_by_key(|(size, _)| *size)
        .map(|(_, p)| p)
}

pub(crate) fn extract_printable_strings(
    bytes: &[u8],
    min_len: usize,
    max_count: usize,
) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = Vec::new();
    for &b in bytes {
        if (0x20..=0x7e).contains(&b) {
            current.push(b);
        } else {
            if current.len() >= min_len
                && let Ok(s) = String::from_utf8(std::mem::take(&mut current))
            {
                out.push(s);
                if out.len() >= max_count {
                    return out;
                }
            }
            current.clear();
        }
    }
    if current.len() >= min_len
        && out.len() < max_count
        && let Ok(s) = String::from_utf8(current)
    {
        out.push(s);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analyzes_the_test_binary_itself() {
        let exe = std::env::current_exe().unwrap();
        let (bytes, _) = read_capped(&exe, BINARY_CAP).unwrap();
        let analysis = analyze(&bytes).expect("test binary is an executable");
        let format = analysis["format"].as_str().unwrap();
        assert!(["ELF", "PE", "Mach-O"].contains(&format), "{format}");
        assert!(
            analysis["sections"]
                .as_array()
                .is_some_and(|s| !s.is_empty())
        );
        assert!(
            analysis["imports"]
                .as_array()
                .is_some_and(|s| !s.is_empty()),
            "{analysis}"
        );
        let mut findings = DeepFindings::default();
        add_findings(&mut findings, "binary", &analysis);
        assert_eq!(findings.format_details["binary.format"], format);
        assert!(
            findings
                .extracted_symbols
                .iter()
                .any(|s| s.starts_with("import: "))
        );
        assert!(file_is_executable(&exe));
    }

    #[test]
    fn non_binaries_are_not_analyzed() {
        assert!(analyze(b"just some text, not a binary").is_none());
        assert!(!is_executable_magic(b"PK\x03\x04"));
    }

    #[test]
    fn printable_strings_are_extracted() {
        assert_eq!(
            extract_printable_strings(b"\0\0hello world\0ab\0libfoo.so", 6, 10),
            vec!["hello world", "libfoo.so"]
        );
    }
}
