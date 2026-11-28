//! Repairing chunks without losing anything. Some findings are damage to the
//! bookkeeping around a chunk, not to the chunk: a wrong length field or
//! compression marker, a wrong sector count, a header entry that points
//! nowhere while the data still sits in the file, or data filed under the
//! wrong chunk. Those can be put right by rewriting a few bytes.
//!
//! Rules, on top of those of [`crate::fix`]:
//! * a repair is only made if the chunk then passes every check of the scan;
//! * only a 5-byte chunk header and 4-byte entries of the region header are
//!   ever written; chunk data is never moved or rewritten;
//! * what cannot be repaired this way is left exactly as it is.

use crate::codec;
use crate::fix::{self, be32, write_at, Backup, FixOptions, Outcome, Progress, Report, Status};
use crate::scan::{Checker, Finding, Kind, Severity, HEADER, SECTOR, SLOTS};
use crate::world::{self, FileRef, Store};
use anyhow::Result;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{ErrorKind, Read, Seek, SeekFrom};
use std::ops::Range;
use std::path::Path;
use std::sync::atomic::Ordering;

/// The most sectors one chunk can have.
const MAX_SECTORS: usize = 255;

const CHANGED: &str = "it changed since the scan; scan again";

/// What `repair` can try for a kind of finding, or why there is nothing to try.
pub fn prospect(kind: Kind) -> std::result::Result<&'static str, &'static str> {
    match kind {
        Kind::NegativeLength | Kind::Truncated | Kind::MissingStream => {
            Ok("if the compressed data behind the bad length field is intact, the length is corrected")
        }
        Kind::BadCompression => Ok("if the data is intact, its compression marker is corrected"),
        Kind::ZeroSectors => Ok("if the data is still where the entry points, its sector count is corrected"),
        Kind::HeaderOverlap | Kind::OutOfBounds => {
            Ok("if an intact copy of the chunk is found in the unused parts of the file, the entry is pointed at it")
        }
        Kind::WrongPosition => Ok("if the chunk the data belongs to has no usable data of its own, the data is filed under it"),
        Kind::Corrupt => Err("its compressed data is damaged, and the format keeps no spare copy"),
        Kind::BadNbt => Err("the chunk data itself is damaged"),
        Kind::BadSection => Err("the data of a section is damaged"),
        Kind::NoPosition => Err("the data is not a chunk of this kind"),
        Kind::ExternalMissing => Err("its data was in a .mcc file that is gone"),
        Kind::NoChunkHeader => Err("the file ends before its data"),
        Kind::Unreadable => Err("it cannot be read"),
        Kind::NoHeader => Err("the file has no header and no chunks"),
        Kind::OrphanMcc => Err("nothing refers to this file; it can only be removed"),
        Kind::TruncatedStream => Err("it still loads; saving it in the game rewrites it"),
        Kind::SharedSectors => Err("this chunk reads fine; the other chunk is the one to repair or remove"),
        Kind::Unverified => Err("it could not be looked into"),
    }
}

/// One line for the details view.
pub fn hint(kind: Kind) -> String {
    match prospect(kind) {
        Ok(what) => format!("Repair: can be tried without losing anything - {what}."),
        Err(why) => format!("Repair: not possible - {why}."),
    }
}

/// A complete, valid chunk record that no header entry points at.
struct Stray {
    sector: usize,
    cnt: usize,
    /// `LastUpdate`: the game tick it was saved at.
    tick: Option<i64>,
}

/// A planned write, on behalf of finding `owner`.
struct Edit {
    owner: usize,
    at: u64,
    bytes: Vec<u8>,
}

struct Region<'a> {
    f: &'a FileRef,
    file: File,
    size: u64,
    sectors: usize,
    /// The region header, with the planned changes already made.
    header: Vec<u8>,
    /// How many header entries claim each sector.
    usage: Vec<u16>,
    checker: Checker,
    strays: Option<HashMap<(i32, i32), Vec<Stray>>>,
    edits: Vec<Edit>,
    /// Slots repaired from their own data in this run.
    repaired: HashSet<usize>,
}

type Attempt<T> = std::result::Result<T, String>;

fn scheme_text(scheme: u8) -> String {
    if codec::known_scheme(scheme) {
        codec::scheme_name(scheme).to_string()
    } else {
        scheme.to_string()
    }
}

/// Whether `data` starts like a stream of this compression scheme.
fn looks_like(scheme: u8, data: &[u8]) -> bool {
    match (scheme, data) {
        (1, [0x1F, 0x8B, ..]) => true,
        (2, [a, b, ..]) => a & 0x0F == 8 && a >> 4 <= 7 && (u16::from(*a) << 8 | u16::from(*b)) % 31 == 0,
        _ => false,
    }
}

impl Region<'_> {
    fn loc(&self, idx: usize) -> u32 {
        be32(&self.header[idx * 4..])
    }

    fn ts(&self, idx: usize) -> u32 {
        be32(&self.header[SECTOR as usize + idx * 4..])
    }

    /// The sectors inside the file that a header entry claims.
    fn span(&self, loc: u32) -> Range<usize> {
        let (off, cnt) = ((loc >> 8) as usize, (loc & 0xFF) as usize);
        if off < 2 || cnt == 0 || off >= self.sectors {
            0..0
        } else {
            off..(off + cnt).min(self.sectors)
        }
    }

    fn set_loc(&mut self, owner: usize, idx: usize, loc: u32) {
        for s in self.span(self.loc(idx)) {
            self.usage[s] -= 1;
        }
        self.header[idx * 4..idx * 4 + 4].copy_from_slice(&loc.to_be_bytes());
        for s in self.span(loc) {
            self.usage[s] += 1;
        }
        self.edits.push(Edit { owner, at: idx as u64 * 4, bytes: loc.to_be_bytes().to_vec() });
    }

    fn set_ts(&mut self, owner: usize, idx: usize, ts: u32) {
        let at = SECTOR as usize + idx * 4;
        if self.ts(idx) != ts {
            self.header[at..at + 4].copy_from_slice(&ts.to_be_bytes());
            self.edits.push(Edit { owner, at: at as u64, bytes: ts.to_be_bytes().to_vec() });
        }
    }

    /// Up to `n` sectors from `sector`, as far as the file goes.
    fn read(&mut self, sector: usize, n: usize) -> Attempt<Vec<u8>> {
        let start = sector as u64 * SECTOR;
        let len = (n as u64 * SECTOR).min(self.size.saturating_sub(start)) as usize;
        let mut buf = vec![0; len];
        self.file
            .seek(SeekFrom::Start(start))
            .and_then(|_| self.file.read_exact(&mut buf))
            .map_err(|e| format!("its sectors cannot be read: {e}"))?;
        Ok(buf)
    }

    /// Put the chunk header and sector count right, if the data behind them
    /// is a complete chunk.
    fn recover(&mut self, owner: usize, idx: usize) -> Attempt<String> {
        let f = self.f;
        let pos = f.chunk_pos(idx);
        let loc = self.loc(idx);
        let (off, cnt) = ((loc >> 8) as usize, (loc & 0xFF) as usize);
        if off < 2 || off >= self.sectors {
            return Err("its header entry does not point at data".to_string());
        }
        let own = self.span(loc);
        if own.clone().any(|s| self.usage[s] > 1) || (cnt == 0 && self.usage[off] > 0) {
            return Err("its sectors are also claimed by another chunk".to_string());
        }
        // Its own sectors, and the unused ones that follow: its data may be
        // longer than the header entry says.
        let mut n = own.len();
        while off + n < self.sectors && n < MAX_SECTORS && self.usage[off + n] == 0 {
            n += 1;
        }
        let mut data = self.read(off, n)?;
        if data.len() < 5 {
            return Err("the file ends before its data".to_string());
        }
        let original = [data[0], data[1], data[2], data[3], data[4]];
        let length = i32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        let version = data[4];
        if version & 0x80 != 0 {
            return Err("its data is in a .mcc file, which this repair does not cover".to_string());
        }

        let mut found = false;
        // The declared length, with the declared scheme or any other.
        if length > 0 && (length - 1) as usize <= data.len() - 5 {
            let schemes: &[u8] = if codec::known_scheme(version) { &[version] } else { &[2, 1, 4, 3] };
            for &scheme in schemes {
                data[4] = scheme;
                if self.checker.check(f, pos, &data).is_none() {
                    found = true;
                    break;
                }
            }
        }
        // The length the compressed stream itself turns out to have.
        if !found {
            for scheme in [2u8, 1] {
                if !looks_like(scheme, &data[5..]) {
                    continue;
                }
                let Some(len) = self.checker.stream_len(scheme, &data[5..]) else { continue };
                let Ok(declared) = i32::try_from(len + 1) else { continue };
                data[..4].copy_from_slice(&declared.to_be_bytes());
                data[4] = scheme;
                if self.checker.check(f, pos, &data).is_none() {
                    found = true;
                    break;
                }
            }
        }
        if !found {
            return Err("the data behind it is not an intact chunk".to_string());
        }

        let now = i32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        let need = (now as usize + 4).div_ceil(SECTOR as usize);
        let sectors = cnt.max(need);
        let mut fixed = Vec::new();
        if now != length {
            fixed.push(format!("its length field ({length} -> {now} bytes)"));
        }
        if data[4] != version {
            fixed.push(format!("its compression marker ({} -> {})", scheme_text(version), scheme_text(data[4])));
        }
        if sectors != cnt {
            fixed.push(format!("its sector count ({cnt} -> {sectors})"));
        }
        if fixed.is_empty() {
            return Err("it reads fine now; scan again".to_string());
        }
        if data[..5] != original {
            self.edits.push(Edit { owner, at: off as u64 * SECTOR, bytes: data[..5].to_vec() });
        }
        if sectors != cnt {
            self.set_loc(owner, idx, (off as u32) << 8 | sectors as u32);
        }
        Ok(format!("corrected {}", fixed.join(" and ")))
    }

    /// Find every complete chunk in the sectors that no header entry claims.
    fn find_strays(&mut self) -> HashMap<(i32, i32), Vec<Stray>> {
        let mut found: HashMap<(i32, i32), Vec<Stray>> = HashMap::new();
        let mut s = 2;
        while s < self.sectors {
            let step = self.stray_at(s).map_or(1, |(pos, stray)| {
                let cnt = stray.cnt;
                found.entry(pos).or_default().push(stray);
                cnt
            });
            s += step;
        }
        found
    }

    fn stray_at(&mut self, s: usize) -> Option<((i32, i32), Stray)> {
        if self.usage[s] != 0 || s >= 1 << 24 {
            return None;
        }
        let head = self.read(s, 1).ok()?;
        if head.len() < 5 {
            return None;
        }
        let length = i32::from_be_bytes([head[0], head[1], head[2], head[3]]);
        if length < 1 || !codec::known_scheme(head[4]) {
            return None;
        }
        let bytes = length as u64 + 4;
        let cnt = bytes.div_ceil(SECTOR) as usize;
        if cnt > MAX_SECTORS || s as u64 * SECTOR + bytes > self.size || (s..s + cnt).any(|x| self.usage[x] != 0) {
            return None;
        }
        let data = if cnt == 1 { head } else { self.read(s, cnt).ok()? };
        let (pos, tick) = self.checker.identify(self.f, &data)?;
        Some((pos, Stray { sector: s, cnt, tick }))
    }

    /// Point a header entry that leads nowhere at the chunk's data, if a
    /// complete copy of it is still in the file.
    fn relink(&mut self, owner: usize, idx: usize) -> Attempt<String> {
        if self.f.store == Store::Poi {
            return Err("point-of-interest data does not say which chunk it belongs to, so it cannot be searched for".to_string());
        }
        let pos = self.f.chunk_pos(idx);
        if self.strays.is_none() {
            let strays = self.find_strays();
            self.strays = Some(strays);
        }
        let usage = &self.usage;
        let copies: Vec<&Stray> = self
            .strays
            .as_ref()
            .and_then(|all| all.get(&pos))
            .map(|list| list.iter().filter(|c| (c.sector..c.sector + c.cnt).all(|s| usage[s] == 0)).collect())
            .unwrap_or_default();
        let pick = match copies.as_slice() {
            [] => return Err("no intact copy of it is left in the unused parts of the file".to_string()),
            [only] => only,
            many => {
                let newest = many.iter().filter_map(|c| c.tick).max();
                let mut best = many.iter().filter(|c| c.tick.is_some() && c.tick == newest);
                match (many.iter().all(|c| c.tick.is_some()), best.next(), best.next()) {
                    (true, Some(one), None) => one,
                    _ => {
                        return Err(format!(
                            "{} intact copies of it are left in the file, and they do not say which is the newest",
                            many.len()
                        ))
                    }
                }
            }
        };
        let (sector, cnt, n) = (pick.sector, pick.cnt, copies.len());
        self.set_loc(owner, idx, (sector as u32) << 8 | cnt as u32);
        Ok(format!(
            "pointed its header entry at the {} found at sector {sector} (if the game saved the chunk again after that copy, this is its earlier state)",
            if n == 1 { "intact copy".to_string() } else { format!("newest of {n} intact copies") }
        ))
    }

    /// Whether the chunk in slot `idx` cannot be loaded as things are.
    fn broken(&mut self, idx: usize) -> bool {
        let span = self.span(self.loc(idx));
        if span.is_empty() {
            return true;
        }
        let pos = self.f.chunk_pos(idx);
        match self.read(span.start, span.len()) {
            Ok(data) => self.checker.check(self.f, pos, &data).is_some_and(|(kind, _)| kind.severity() == Severity::Error),
            Err(_) => false,
        }
    }

    /// File data that sits under the wrong chunk under the chunk it is.
    /// Returns the slot that received it, if one did.
    fn refile(&mut self, owner: usize, idx: usize) -> Attempt<(String, Option<usize>)> {
        let f = self.f;
        let loc = self.loc(idx);
        let span = self.span(loc);
        if span.is_empty() {
            return Err("its header entry does not point at data".to_string());
        }
        let data = self.read(span.start, span.len())?;
        let Some((target, _)) = self.checker.identify(f, &data) else {
            return Err("the data stored here is not an intact chunk".to_string());
        };
        if target == f.chunk_pos(idx) {
            return Err("it reads fine now; scan again".to_string());
        }
        let (x, z) = target;
        if (x.div_euclid(32), z.div_euclid(32)) != (f.rx, f.rz) {
            return Err(format!(
                "its data is chunk {x},{z}, which belongs in {}; moving data to another region file is not supported",
                world::region_name(x.div_euclid(32), z.div_euclid(32))
            ));
        }
        let to = world::chunk_index(x, z);
        if self.loc(to) == loc {
            self.set_loc(owner, idx, 0);
            let own = self.own_data(owner, idx);
            return Ok((format!("removed this entry: it was a second entry for the data of chunk {x},{z}, which keeps it; {own}"), None));
        }
        if self.repaired.contains(&to) {
            return Err(format!("its data is a second copy of chunk {x},{z}, which was just repaired from its own data"));
        }
        if self.loc(to) != 0 && !self.broken(to) {
            return Err(format!(
                "its data is a second copy of chunk {x},{z}, which has intact data of its own; this entry can only be removed"
            ));
        }
        let ts = self.ts(idx);
        self.set_loc(owner, to, loc);
        self.set_ts(owner, to, ts);
        self.set_loc(owner, idx, 0);
        let own = self.own_data(owner, idx);
        Ok((format!("filed its data under chunk {x},{z}, which is what it is; {own}"), Some(to)))
    }

    /// For a slot that was just emptied because what it pointed at belongs
    /// to another chunk: look for the chunk's own data. Says what came of it.
    fn own_data(&mut self, owner: usize, idx: usize) -> String {
        match self.relink(owner, idx) {
            Ok(how) => {
                self.repaired.insert(idx);
                format!("this chunk's own data was still in the file: {how}")
            }
            Err(_) => {
                self.set_ts(owner, idx, 0);
                "nothing else was stored for this chunk, so the game generates it".to_string()
            }
        }
    }
}

/// Repair what can be repaired in one region file. Returns the status of each
/// of `items` (finding number in the caller's list, finding).
fn repair_file(
    f: &FileRef,
    items: &[(usize, &Finding)],
    dry_run: bool,
    backup: &mut Backup,
    warnings: &mut Vec<String>,
) -> HashMap<usize, Status> {
    let all = |why: String| items.iter().map(|&(i, _)| (i, Status::Skipped(why.clone()))).collect();
    let mut file = match File::options().read(true).write(!dry_run).open(&f.path) {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => return all("its region file no longer exists".to_string()),
        Err(e) => return all(format!("its region file cannot be opened: {e}")),
    };
    let size = match file.metadata() {
        Ok(m) => m.len(),
        Err(e) => return all(format!("its region file cannot be examined: {e}")),
    };
    let mut header = vec![0u8; HEADER as usize];
    match file.read_exact(&mut header) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::UnexpectedEof => return all(CHANGED.to_string()),
        Err(e) => return all(format!("the header of its region file cannot be read: {e}")),
    }
    let sectors = size.div_ceil(SECTOR) as usize;
    let mut region = Region {
        f,
        file,
        size,
        sectors,
        header,
        usage: vec![0; sectors],
        checker: Checker::default(),
        strays: None,
        edits: Vec::new(),
        repaired: HashSet::new(),
    };
    region.usage[0] = 1;
    region.usage[1] = 1;
    for idx in 0..SLOTS {
        for s in region.span(region.loc(idx)) {
            region.usage[s] += 1;
        }
    }

    let mut results: HashMap<usize, Status> = HashMap::new();
    let mut by_slot: HashMap<usize, usize> = HashMap::new();
    let mut live: Vec<(usize, usize, Kind)> = Vec::new();
    for &(i, finding) in items {
        let Some(slot) = finding.slot else { continue };
        let idx = slot.idx as usize;
        if (region.loc(idx), region.ts(idx)) != (slot.loc, slot.ts) {
            results.insert(i, Status::Skipped(CHANGED.to_string()));
        } else {
            by_slot.insert(idx, i);
            live.push((i, idx, finding.kind));
        }
    }

    // First what can be repaired from a chunk's own data ...
    for &(i, idx, kind) in &live {
        let attempt = match kind {
            Kind::NegativeLength | Kind::Truncated | Kind::MissingStream | Kind::BadCompression => region.recover(i, idx),
            Kind::ZeroSectors => region.recover(i, idx).or_else(|_| region.relink(i, idx)),
            Kind::HeaderOverlap | Kind::OutOfBounds => region.relink(i, idx),
            Kind::WrongPosition => continue,
            other => Err(prospect(other).err().unwrap_or("there is nothing to repair").to_string()),
        };
        if attempt.is_ok() {
            region.repaired.insert(idx);
        }
        results.insert(i, attempt.map_or_else(Status::NotRepairable, Status::Repaired));
    }
    // ... then data that sits under the wrong chunk, which may be just what
    // a chunk that could not be repaired is missing.
    let mut followers: Vec<(usize, usize)> = Vec::new();
    for &(i, idx, kind) in &live {
        if kind != Kind::WrongPosition {
            continue;
        }
        match region.refile(i, idx) {
            Ok((how, received)) => {
                results.insert(i, Status::Repaired(how));
                let other = received.and_then(|to| by_slot.get(&to).copied());
                if let Some(j) = other.filter(|j| matches!(results.get(j), Some(Status::NotRepairable(_)))) {
                    let (x, z) = f.chunk_pos(idx);
                    results.insert(j, Status::Repaired(format!("its data was found filed under chunk {x},{z} and was filed here")));
                    followers.push((i, j));
                }
            }
            Err(why) => {
                results.insert(i, Status::NotRepairable(why));
            }
        }
    }

    let Region { mut file, mut edits, .. } = region;
    if dry_run || edits.is_empty() {
        return results;
    }
    if let Err(e) = backup.save(&f.path) {
        let why = format!("its region file could not be backed up ({e:#}), so it was left alone");
        for status in results.values_mut().filter(|s| matches!(s, Status::Repaired(_))) {
            *status = Status::Skipped(why.clone());
        }
        return results;
    }
    // Chunk headers before the region header: an entry is only changed once
    // what it will point at is right.
    edits.sort_by_key(|e| e.at < HEADER);
    let mut failed: HashSet<usize> = HashSet::new();
    for edit in &edits {
        if failed.contains(&edit.owner) {
            continue;
        }
        if let Err(e) = write_at(&mut file, edit.at, &edit.bytes) {
            failed.insert(edit.owner);
            results.insert(edit.owner, Status::Skipped(format!("its region file could not be written: {e}")));
        }
    }
    for (leader, follower) in followers {
        if failed.contains(&leader) {
            results.insert(follower, Status::Skipped("its region file could not be written".to_string()));
        }
    }
    if let Err(e) = file.sync_all() {
        warnings.push(format!("{}: could not be flushed to disk: {e}", f.path.display()));
    }
    results
}

/// Repair `findings` (from a scan of `root`) where that loses nothing, or
/// with `dry_run` report what would be repaired. Nothing is ever removed.
pub fn execute(root: &Path, findings: &[&Finding], opts: &FixOptions, progress: &Progress) -> Result<Report> {
    let mut warnings = Vec::new();
    let _lock = fix::acquire(root, opts, &mut warnings)?;

    let mut outcomes: Vec<Outcome> = findings
        .iter()
        .map(|f| Outcome {
            n: f.n,
            fix: f.fix(),
            status: Status::NotRepairable(prospect(f.kind).err().unwrap_or("there is nothing to repair").to_string()),
            notes: Vec::new(),
        })
        .collect();
    let mut by_file: BTreeMap<&Path, Vec<(usize, &Finding)>> = BTreeMap::new();
    for (i, f) in findings.iter().enumerate() {
        if f.slot.is_some() {
            by_file.entry(f.file.path.as_path()).or_default().push((i, f));
        }
    }
    progress.total.store(by_file.len() as u64, Ordering::Relaxed);
    progress.done.store(0, Ordering::Relaxed);

    let mut backup = Backup::new(root, opts.backup);
    let mut files = 0;
    for items in by_file.values() {
        let results = repair_file(&items[0].1.file, items, opts.dry_run, &mut backup, &mut warnings);
        if results.values().any(|s| matches!(s, Status::Repaired(_))) {
            files += 1;
        }
        for (i, status) in results {
            outcomes[i].status = status;
        }
        progress.done.fetch_add(1, Ordering::Relaxed);
    }
    Ok(Report { dry_run: opts.dry_run, repair: true, outcomes, backup_dir: backup.into_dir(), files, warnings })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::testing::{gzip, zlib};
    use crate::scan::tests::{sample_world, scan};
    use crate::scan::ScanOutput;
    use crate::testutil::{entities, good_terrain, Nbt, Region as Builder, TempDir};
    use std::fs;
    use std::path::PathBuf;

    const DRY: FixOptions = FixOptions { dry_run: true, backup: true, force: false, linked: true };
    const REAL: FixOptions = FixOptions { dry_run: false, ..DRY };

    fn run(out: &ScanOutput, opts: &FixOptions) -> Report {
        let picked: Vec<&Finding> = out.findings.iter().collect();
        execute(&out.root, &picked, opts, &Progress::default()).unwrap()
    }

    fn statuses(report: &Report) -> Vec<Status> {
        report.outcomes.iter().map(|o| o.status.clone()).collect()
    }

    fn region(tmp: &TempDir) -> PathBuf {
        tmp.path().join("region/r.0.0.mca")
    }

    /// The findings, by slot.
    fn kinds(out: &ScanOutput) -> Vec<(Option<u16>, Kind)> {
        let mut v: Vec<_> = out.findings.iter().map(|f| (f.slot.map(|s| s.idx), f.kind)).collect();
        v.sort();
        v
    }

    /// A scan with its findings in the order of their slots.
    fn sorted_scan(root: &Path) -> ScanOutput {
        let mut out = scan(root, false, 2);
        out.findings.sort_by_key(|f| f.slot.map(|s| s.idx));
        out
    }

    fn slots(path: &Path) -> Vec<usize> {
        let buf = fs::read(path).unwrap();
        (0..1024).filter(|i| be32(&buf[i * 4..]) != 0).collect()
    }

    /// Terrain that says when it was saved.
    fn dated(x: i32, z: i32, tick: i64, marker: &str) -> Vec<u8> {
        let mut n = Nbt::root();
        n.int("DataVersion", 4000).int("xPos", x).int("zPos", z).long("LastUpdate", tick).string("Status", marker).end();
        n.0
    }

    /// Dry run, real run, rescan: the dry run writes nothing and predicts
    /// the real run, which leaves exactly `left` to be found.
    fn repaired(tmp: &TempDir, expect: &[&str], left: &[(Option<u16>, Kind)]) -> Report {
        let path = region(tmp);
        let before = fs::read(&path).unwrap();
        let out = sorted_scan(tmp.path());
        let dry = run(&out, &DRY);
        assert_eq!(fs::read(&path).unwrap(), before, "the dry run wrote to the world");
        assert!(!tmp.path().join(fix::BACKUP_DIR).exists());
        assert!(dry.backup_dir.is_none());
        let real = run(&out, &REAL);
        assert_eq!(statuses(&dry), statuses(&real));
        assert_eq!(dry.files, real.files);
        assert_eq!(real.outcomes.len(), expect.len(), "{:?}", real.outcomes);
        for (o, want) in real.outcomes.iter().zip(expect) {
            match &o.status {
                Status::Repaired(how) => assert!(how.contains(want), "{how:?} lacks {want:?}"),
                Status::NotRepairable(why) => assert!(want.starts_with('!') && why.contains(&want[1..]), "{why:?} vs {want:?}"),
                other => panic!("{other:?} vs {want:?}"),
            }
        }
        if real.repair_counts().0 > 0 {
            let dir = real.backup_dir.clone().expect("no backup");
            assert_eq!(fs::read(dir.join("region/r.0.0.mca")).unwrap(), before);
            assert_eq!(fs::read(&path).unwrap().len(), before.len(), "a repair never changes the size of a file");
        } else {
            assert_eq!(fs::read(&path).unwrap(), before);
            assert!(real.backup_dir.is_none());
        }
        assert_eq!(kinds(&scan(tmp.path(), false, 2)), left);
        real
    }

    #[test]
    fn chunk_headers_are_corrected() {
        let tmp = TempDir::new("repair-header");
        let mut r = Builder::new();
        r.add(0, &good_terrain(0, 0));
        // 1: negative length, 2: length 0, 3: length far too large.
        for (idx, length) in [(1usize, -1i32), (2, 0), (3, 9_000_000)] {
            let (off, _) = r.add(idx, &good_terrain(idx as i32, 0));
            r.buf[off as usize * 4096..off as usize * 4096 + 4].copy_from_slice(&length.to_be_bytes());
        }
        // 4: unknown scheme over zlib data, 5: gzip data marked as scheme 0 with a bad length.
        let (off, _) = r.add(4, &good_terrain(4, 0));
        r.buf[off as usize * 4096 + 4] = 77;
        let (off, _) = r.add_raw(5, 1, &gzip(&good_terrain(5, 0)));
        r.buf[off as usize * 4096..off as usize * 4096 + 5].copy_from_slice(&[0xFF, 0, 0, 0, 0]);
        // 6: a length that is right but for a damaged stream.
        let mut bad = zlib(&good_terrain(6, 0));
        let mid = bad.len() / 2;
        bad[mid] ^= 0x55;
        r.add_raw(6, 2, &bad);
        // 7: the length is one short, so the stream lacks its last byte.
        let (off, _) = r.add(7, &good_terrain(7, 0));
        let at = off as usize * 4096;
        let length = be32(&r.buf[at..]) - 1;
        r.buf[at..at + 4].copy_from_slice(&length.to_be_bytes());
        r.write(&region(&tmp));

        let report = repaired(
            &tmp,
            &[
                "its length field (-1 ->",
                "its length field (0 ->",
                "its length field (9000000 ->",
                "its compression marker (77 -> zlib)",
                "its compression marker (0 -> gzip)",
                "!damaged",
                "!the game rewrites it",
            ],
            &[(Some(6), Kind::Corrupt), (Some(7), Kind::TruncatedStream)],
        );
        assert_eq!(report.repair_counts(), (5, 2, 0));
        assert_eq!(report.files, 1);
        assert!(report.summary().starts_with("Done: repaired 5 chunks; 2 cannot be repaired"), "{}", report.summary());
        assert_eq!(slots(&region(&tmp)), [0, 1, 2, 3, 4, 5, 6, 7]);
    }

    #[test]
    fn sector_counts_are_corrected() {
        let tmp = TempDir::new("repair-sectors");
        let mut r = Builder::new();
        // Incompressible, so that the chunk needs several sectors.
        let mut noise = 0x2545_F491_4F6C_DD1Du64;
        let text: String = (0..12_000)
            .map(|_| {
                noise ^= noise << 13;
                noise ^= noise >> 7;
                noise ^= noise << 17;
                char::from(b'a' + (noise % 26) as u8)
            })
            .collect();
        let big = |x: i32| {
            let mut n = Nbt::root();
            n.int("DataVersion", 4000).int("xPos", x).int("zPos", 0).string("Status", &text).end();
            n.0
        };
        // 0: three sectors of data, an entry for one; the rest is unclaimed.
        let (off, cnt) = r.add(0, &big(0));
        assert!(cnt >= 2);
        r.set_loc(0, off, 1);
        // 1: an entry for no sectors at all.
        let (off, _) = r.add(1, &good_terrain(1, 0));
        r.set_loc(1, off, 0);
        // 2: too few sectors, but the next ones belong to chunk 3.
        let (off, _) = r.add(2, &big(2));
        r.set_loc(2, off, 1);
        r.set_loc(3, off + 1, 1);
        r.write(&region(&tmp));

        let out = sorted_scan(tmp.path());
        assert_eq!(out.findings[0].kind, Kind::Truncated);
        assert_eq!(out.findings[1].kind, Kind::ZeroSectors);
        assert_eq!(out.findings[2].kind, Kind::Truncated);
        let report = repaired(
            &tmp,
            &[&format!("its sector count (1 -> {cnt})"), "its sector count (0 -> 1)", "!not an intact chunk", "!"],
            &[(Some(2), Kind::Truncated), (Some(3), out.findings[3].kind)],
        );
        assert_eq!(report.repair_counts(), (2, 2, 0));
    }

    #[test]
    fn lost_entries_are_found_again() {
        let tmp = TempDir::new("repair-relink");
        let mut r = Builder::new();
        r.add(0, &good_terrain(0, 0));
        // 1: one copy in unclaimed sectors.
        let lost = r.add(1, &good_terrain(1, 0));
        r.set_loc(1, 1, 1);
        // 2: three copies; the one from tick 300 is the newest.
        r.add(2, &dated(2, 0, 100, "old"));
        let newest = r.add(2, &dated(2, 0, 300, "new"));
        r.add(2, &dated(2, 0, 200, "middle"));
        r.set_loc(2, 5000, 1);
        // 3: two copies that do not say when they were saved.
        r.add(3, &good_terrain(3, 0));
        r.add(3, &good_terrain(3, 0));
        r.set_loc(3, 0, 2);
        // 4: nothing left of it; 5: only a damaged copy.
        r.set_loc(4, 6000, 1);
        let mut bad = zlib(&good_terrain(5, 0));
        bad[10] ^= 0xFF;
        r.add_raw(5, 2, &bad);
        r.set_loc(5, 1, 1);
        r.write(&region(&tmp));

        let report = repaired(
            &tmp,
            &[
                "the intact copy found at sector",
                "the newest of 3 intact copies",
                "!2 intact copies",
                "!no intact copy",
                "!no intact copy",
            ],
            &[(Some(3), Kind::HeaderOverlap), (Some(4), Kind::OutOfBounds), (Some(5), Kind::HeaderOverlap)],
        );
        assert_eq!(report.repair_counts(), (2, 3, 0));
        let buf = fs::read(region(&tmp)).unwrap();
        assert_eq!(be32(&buf[4..]), lost.0 << 8 | lost.1);
        assert_eq!(be32(&buf[8..]), newest.0 << 8 | newest.1);
        // The time stamps were left alone.
        assert_eq!(be32(&buf[4096 + 4..]), 1_700_000_000);
    }

    #[test]
    fn misfiled_chunks_are_filed_where_they_belong() {
        let tmp = TempDir::new("repair-refile");
        let mut r = Builder::new();
        r.add(0, &good_terrain(0, 0));
        // Slot 1 holds chunk 9,0, which has no entry.
        let moved = r.add(1, &good_terrain(9, 0));
        // Slot 2 holds chunk 10,0, whose own data is damaged.
        let replaced = r.add(2, &good_terrain(10, 0));
        r.add_raw(10, 2, b"not zlib at all");
        // Slots 3 and 4 both point at the data of chunk 4,0.
        let (off, cnt) = r.add(4, &good_terrain(4, 0));
        r.set_loc(3, off, cnt);
        // Slot 5 holds a second copy of chunk 0,0; slot 6 one of a chunk of another region.
        r.add(5, &good_terrain(0, 0));
        r.add(6, &good_terrain(40, 0));
        // Slot 7 has its own data, but its entry points at the data of chunk 8,0.
        let own = r.add(7, &good_terrain(7, 0));
        let (off, cnt) = r.add(8, &good_terrain(8, 0));
        r.set_loc(7, off, cnt);
        r.write(&region(&tmp));

        let out = scan(tmp.path(), false, 2);
        assert_eq!(
            kinds(&out),
            [
                (Some(1), Kind::WrongPosition),
                (Some(2), Kind::WrongPosition),
                (Some(3), Kind::WrongPosition),
                (Some(4), Kind::SharedSectors),
                (Some(5), Kind::WrongPosition),
                (Some(6), Kind::WrongPosition),
                (Some(7), Kind::WrongPosition),
                (Some(8), Kind::SharedSectors),
                (Some(10), Kind::Corrupt),
            ]
        );
        let report = repaired(
            &tmp,
            &[
                "filed its data under chunk 9,0",
                "filed its data under chunk 10,0",
                "a second entry for the data of chunk 4,0",
                "!this chunk reads fine",
                "!intact data of its own",
                "!r.1.0.mca",
                "this chunk's own data was still in the file",
                "!this chunk reads fine",
                "found filed under chunk 2,0",
            ],
            &[(Some(5), Kind::WrongPosition), (Some(6), Kind::WrongPosition)],
        );
        assert_eq!(report.repair_counts(), (5, 4, 0));
        assert_eq!(slots(&region(&tmp)), [0, 4, 5, 6, 7, 8, 9, 10]);
        let buf = fs::read(region(&tmp)).unwrap();
        assert_eq!(be32(&buf[9 * 4..]), moved.0 << 8 | moved.1);
        assert_eq!(be32(&buf[10 * 4..]), replaced.0 << 8 | replaced.1);
        assert_eq!(be32(&buf[7 * 4..]), own.0 << 8 | own.1);
        assert_eq!(be32(&buf[4096 + 7 * 4..]), 1_700_000_000);
        assert_eq!(be32(&buf[4096 + 4..]), 0, "an emptied slot keeps no time stamp");
    }

    #[test]
    fn entity_chunks_are_repaired_too() {
        let tmp = TempDir::new("repair-entities");
        let mut r = Builder::new();
        r.add(0, &entities(0, 0));
        r.add(1, &entities(1, 0));
        r.set_loc(1, 1, 1);
        r.add(2, &entities(7, 0));
        r.write(&tmp.path().join("entities/r.0.0.mca"));
        let out = scan(tmp.path(), false, 2);
        let real = run(&out, &REAL);
        assert_eq!(real.repair_counts(), (2, 0, 0), "{:?}", real.outcomes);
        assert!(scan(tmp.path(), false, 2).findings.is_empty());
        assert_eq!(slots(&tmp.path().join("entities/r.0.0.mca")), [0, 1, 7]);
    }

    #[test]
    fn changed_files_are_left_alone() {
        let tmp = TempDir::new("repair-stale");
        let mut r = Builder::new();
        let (off, _) = r.add(1, &good_terrain(1, 0));
        r.buf[off as usize * 4096..off as usize * 4096 + 4].copy_from_slice(&(-5i32).to_be_bytes());
        r.write(&region(&tmp));
        let out = scan(tmp.path(), false, 2);
        // The game saved the chunk again.
        r.buf[4096 + 4..4096 + 8].copy_from_slice(&1_800_000_000u32.to_be_bytes());
        r.write(&region(&tmp));
        let before = fs::read(region(&tmp)).unwrap();
        for opts in [&DRY, &REAL] {
            let report = run(&out, opts);
            assert!(matches!(&report.outcomes[0].status, Status::Skipped(why) if why.contains("changed since the scan")));
            assert_eq!(report.repair_counts(), (0, 0, 1));
        }
        assert_eq!(fs::read(region(&tmp)).unwrap(), before);
        fs::remove_file(region(&tmp)).unwrap();
        assert!(matches!(&run(&out, &REAL).outcomes[0].status, Status::Skipped(why) if why.contains("no longer exists")));
    }

    #[test]
    fn the_dry_run_predicts_the_real_run() {
        let tmp = TempDir::new("repair-sample");
        sample_world(tmp.path());
        let out = scan(tmp.path(), false, 4);
        let dry = run(&out, &DRY);
        assert!(!tmp.path().join(fix::BACKUP_DIR).exists());
        let real = run(&out, &REAL);
        assert_eq!(statuses(&dry), statuses(&real));
        // Whatever was repaired is no longer found; whatever was not, still is.
        let after = scan(tmp.path(), false, 4);
        let still: HashSet<(String, Option<u16>)> = after
            .findings
            .iter()
            .filter(|f| f.severity() == Severity::Error)
            .map(|f| (f.file.rel.clone(), f.slot.map(|s| s.idx)))
            .collect();
        for (o, f) in real.outcomes.iter().zip(&out.findings) {
            let key = (f.file.rel.clone(), f.slot.map(|s| s.idx));
            match &o.status {
                Status::Repaired(how) => assert!(!still.contains(&key), "{} still found after: {how}", f.line()),
                Status::NotRepairable(_) if f.severity() == Severity::Error => assert!(still.contains(&key), "{}", f.line()),
                Status::NotRepairable(_) => {}
                other => panic!("{other:?}"),
            }
        }
        // Repairing again finds nothing more to repair.
        assert_eq!(run(&after, &REAL).repair_counts().0, 0);
    }
}
