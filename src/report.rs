//! Reports: the JSON-lines file and the long description of one finding.

use crate::scan::{Finding, Fix, ScanOutput, Severity};
use crate::util::{fmt_utc, now_secs};
use crate::world::Store;
use anyhow::{Context, Result};
use serde::Serialize;
use std::io::{BufWriter, Write};
use std::path::Path;

#[derive(Serialize)]
struct HeaderLine<'a> {
    mcfsck: &'a str,
    world: String,
    scanned_at: String,
    quick: bool,
    region_files: u64,
    bytes: u64,
    chunks: u64,
    errors: u64,
    warnings: u64,
}

#[derive(Serialize)]
struct FindingLine<'a> {
    n: u64,
    severity: Severity,
    kind: &'a str,
    problem: &'a str,
    detail: &'a str,
    file: &'a str,
    dimension: &'a str,
    store: Store,
    region: [i32; 2],
    #[serde(skip_serializing_if = "Option::is_none")]
    chunk: Option<[i32; 2]>,
    fix: &'a str,
}

fn fix_name(fix: Fix) -> &'static str {
    match fix {
        Fix::DeleteChunk => "delete_chunk",
        Fix::DeleteFile => "delete_file",
        Fix::None => "none",
    }
}

/// Write the scan as JSON lines: one header object, then one object per finding.
pub fn write_jsonl(w: &mut dyn Write, out: &ScanOutput) -> Result<()> {
    let header = HeaderLine {
        mcfsck: env!("CARGO_PKG_VERSION"),
        world: out.root.display().to_string(),
        scanned_at: fmt_utc(now_secs()),
        quick: out.quick,
        region_files: out.total_files(),
        bytes: out.bytes,
        chunks: out.chunks,
        errors: out.count(Severity::Error),
        warnings: out.count(Severity::Warning),
    };
    serde_json::to_writer(&mut *w, &header)?;
    w.write_all(b"\n")?;
    for f in &out.findings {
        let line = FindingLine {
            n: f.n,
            severity: f.severity(),
            kind: f.kind.code(),
            problem: f.kind.label(),
            detail: &f.detail,
            file: &f.file.rel,
            dimension: &f.file.dim,
            store: f.file.store,
            region: [f.file.rx, f.file.rz],
            chunk: f.pos.map(|(x, z)| [x, z]),
            fix: fix_name(f.fix()),
        };
        serde_json::to_writer(&mut *w, &line)?;
        w.write_all(b"\n")?;
    }
    Ok(())
}

pub fn save_jsonl(path: &Path, out: &ScanOutput) -> Result<()> {
    let file = std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
    let mut w = BufWriter::new(file);
    write_jsonl(&mut w, out).and_then(|()| Ok(w.flush()?)).with_context(|| format!("writing {}", path.display()))
}

/// What fixing a finding means, in a sentence or two.
pub fn fix_text(f: &Finding) -> &'static str {
    match (f.fix(), f.severity(), f.file.store) {
        (Fix::None, _, _) => "Fix: none. There is nothing this tool can safely remove here.",
        (Fix::DeleteFile, _, _) => "Fix: remove the file (a copy is kept in the backup folder).",
        (Fix::DeleteChunk, Severity::Warning, _) => {
            "Fix: the chunk can be removed, but it still loads, so that is rarely what you want. It is not removed unless you select it yourself."
        }
        (Fix::DeleteChunk, _, Store::Entities) => {
            "Fix: remove this chunk's entity data. The terrain stays; the mobs, item frames and other entities that were stored here are gone (they cannot be read as it is)."
        }
        (Fix::DeleteChunk, _, Store::Poi) => {
            "Fix: remove this chunk's point-of-interest data. The game rebuilds it from the blocks when the chunk is next loaded."
        }
        (Fix::DeleteChunk, _, _) => {
            "Fix: remove the chunk from the region file. The game then generates fresh terrain there; what was built in it is lost (it cannot be read as it is). Its entity and point-of-interest data goes with it."
        }
    }
}

/// Everything known about one finding, for the details view.
pub fn details(f: &Finding) -> Vec<String> {
    let mut lines = vec![
        format!("#{}  {}  {}", f.n, f.severity().short(), f.kind.label()),
        format!("what:   {}", f.detail),
        format!("where:  {} ({}), {} store", f.file.dim_short(), f.file.dim, f.file.store.name()),
        format!("file:   {}", f.file.rel),
    ];
    if let Some((x, z)) = f.pos {
        lines.push(format!(
            "chunk:  {x},{z}   blocks x {}..{}, z {}..{}",
            i64::from(x) * 16,
            i64::from(x) * 16 + 15,
            i64::from(z) * 16,
            i64::from(z) * 16 + 15
        ));
        lines.push(format!("go to:  /execute in {} run tp @s {} ~ {}", f.file.dim, i64::from(x) * 16 + 8, i64::from(z) * 16 + 8));
    }
    if let Some(slot) = f.slot {
        let (off, cnt) = (slot.loc >> 8, slot.loc & 0xFF);
        let saved = if slot.ts == 0 { "never".to_string() } else { fmt_utc(i64::from(slot.ts)) };
        lines.push(format!("entry:  header slot {}, {cnt} sector(s) from sector {off}, last saved {saved}", slot.idx));
    }
    lines.push(String::new());
    lines.push(f.kind.explain().to_string());
    lines.push(String::new());
    lines.push(fix_text(f).to_string());
    if f.slot.is_some() && f.severity() == Severity::Error {
        lines.push(String::new());
        lines.push(crate::repair::hint(f.kind));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::tests::{sample_world, scan};
    use crate::scan::Kind;
    use crate::testutil::TempDir;

    #[test]
    fn jsonl_is_one_valid_object_per_line() {
        let tmp = TempDir::new("json");
        sample_world(tmp.path());
        let out = scan(tmp.path(), false, 2);
        let mut buf = Vec::new();
        write_jsonl(&mut buf, &out).unwrap();
        let text = String::from_utf8(buf).unwrap();
        let lines: Vec<serde_json::Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines.len(), out.findings.len() + 1);
        assert_eq!(lines[0]["chunks"], 35);
        assert_eq!(lines[0]["errors"].as_u64().unwrap() + lines[0]["warnings"].as_u64().unwrap(), 24);
        let wrong = lines.iter().find(|l| l["kind"] == "wrong_position" && l["store"] == "region" && l["chunk"][0] == 12).unwrap();
        assert_eq!(wrong["severity"], "error");
        assert_eq!(wrong["fix"], "delete_chunk");
        assert_eq!(wrong["file"], "region/r.0.0.mca");
        assert_eq!(wrong["dimension"], "minecraft:overworld");
        let whole = lines.iter().find(|l| l["kind"] == "no_header").unwrap();
        assert!(whole.get("chunk").is_none());
        assert_eq!(whole["fix"], "delete_file");

        let path = tmp.path().join("report.jsonl");
        save_jsonl(&path, &out).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), lines.len());
    }

    #[test]
    fn details_for_every_kind() {
        let tmp = TempDir::new("detail");
        sample_world(tmp.path());
        let out = scan(tmp.path(), false, 2);
        for f in &out.findings {
            let d = details(f);
            assert!(d.len() >= 7, "{d:?}");
            assert!(d.iter().any(|l| l.starts_with("Fix:")));
        }
        let f = out.findings.iter().find(|f| f.kind == Kind::BadSection).unwrap();
        let d = details(f).join("\n");
        assert!(d.contains("chunk:  13,0   blocks x 208..223, z 0..15"), "{d}");
        assert!(d.contains("/execute in minecraft:overworld run tp @s 216 ~ 8"), "{d}");
        assert!(d.contains("last saved 2023-11-14 22:13:20 UTC"), "{d}");
    }

    #[test]
    fn every_kind_has_its_own_code() {
        let mut codes: Vec<&str> = Kind::ALL.iter().map(|k| k.code()).collect();
        codes.sort();
        codes.dedup();
        assert_eq!(codes.len(), Kind::ALL.len());
        assert!(Kind::ALL.iter().all(|k| !k.label().is_empty() && !k.explain().is_empty()));
    }
}
