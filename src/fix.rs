//! Acting on findings: remove chunks the game cannot load (so it generates
//! them anew) and files nothing uses. The same code runs as a dry run, which
//! does every check but never opens a file for writing.
//!
//! Safety rules:
//! * nothing is written while a game or server has the world open;
//! * a chunk is only touched if its header entry is still exactly what the
//!   scan saw, a file only if it is still what the scan described;
//! * every file is copied to `<world>/mcfsck-backups/<time>/` before its
//!   first change, and is left alone if that copy fails;
//! * a chunk is removed the way the game removes one: by zeroing its 4-byte
//!   header entry. No other byte of a region file is ever rewritten.

use crate::scan::{Finding, Fix, Kind, HEADER, SECTOR};
use crate::util::{human_count, now_secs, s, stamp};
use crate::world::{self, FileRef, LockResult, Store};
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub const BACKUP_DIR: &str = "mcfsck-backups";

#[derive(Clone, Debug)]
pub struct FixOptions {
    /// Work out and report what would happen; write nothing.
    pub dry_run: bool,
    /// Copy files before changing them.
    pub backup: bool,
    /// Go ahead even if the world seems to be in use.
    pub force: bool,
    /// With a terrain chunk, also remove its entity and point-of-interest
    /// data, which would otherwise outlive the terrain they belong to.
    pub linked: bool,
}

#[derive(Default)]
pub struct Progress {
    pub total: AtomicU64,
    pub done: AtomicU64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// Removed (or, in a dry run, would be).
    Removed,
    Skipped(String),
    /// There is nothing this tool can do about the finding.
    NoFix,
}

#[derive(Clone, Debug)]
pub struct Outcome {
    /// The finding's number.
    #[cfg_attr(not(test), allow(dead_code))]
    pub n: u64,
    pub fix: Fix,
    pub status: Status,
    /// What else goes with it.
    pub notes: Vec<String>,
}

impl Outcome {
    pub fn line(&self, finding: &Finding, dry_run: bool) -> String {
        let target = finding.target();
        match &self.status {
            Status::Removed => {
                let verb = if dry_run { "WOULD REMOVE" } else { "REMOVED" };
                let extra = if self.notes.is_empty() { String::new() } else { format!(" (with {})", self.notes.join(", ")) };
                format!("{verb} {target}{extra} - {}", finding.kind.label())
            }
            Status::Skipped(why) => format!("SKIPPED {target} - {why}"),
            Status::NoFix => format!("NO FIX {target} - {}: nothing to remove", finding.kind.label()),
        }
    }
}

#[derive(Debug)]
pub struct Report {
    pub dry_run: bool,
    /// One per finding, in the order given.
    pub outcomes: Vec<Outcome>,
    pub backup_dir: Option<PathBuf>,
    /// Region files that were (or would be) modified, and files removed.
    pub files: u64,
    pub warnings: Vec<String>,
}

impl Report {
    /// (chunks removed, files removed, skipped, without a fix)
    pub fn counts(&self) -> (u64, u64, u64, u64) {
        let mut c = (0, 0, 0, 0);
        for o in &self.outcomes {
            match (&o.status, o.fix) {
                (Status::Removed, Fix::DeleteFile) => c.1 += 1,
                (Status::Removed, _) => c.0 += 1,
                (Status::Skipped(_), _) => c.2 += 1,
                (Status::NoFix, _) => c.3 += 1,
            }
        }
        c
    }

    pub fn summary(&self) -> String {
        let (chunks, files, skipped, nofix) = self.counts();
        let mut text = format!(
            "{} {} chunk{} and {} file{}",
            if self.dry_run { "DRY RUN: would remove" } else { "Done: removed" },
            human_count(chunks),
            s(chunks),
            human_count(files),
            s(files)
        );
        if skipped > 0 {
            text.push_str(&format!("; {} skipped", human_count(skipped)));
        }
        if nofix > 0 {
            text.push_str(&format!("; {} cannot be fixed by removing anything", human_count(nofix)));
        }
        if self.files > 0 {
            text.push_str(&format!(
                " ({} file{} {})",
                human_count(self.files),
                s(self.files),
                if self.dry_run { "would be touched" } else { "touched" }
            ));
        }
        text.push_str(if self.dry_run { ". Nothing was written." } else { "." });
        text
    }
}

// ---------------------------------------------------------------------------
// Backups
// ---------------------------------------------------------------------------

struct Backup {
    root: PathBuf,
    enabled: bool,
    dir: Option<PathBuf>,
    saved: HashSet<PathBuf>,
}

impl Backup {
    fn new_dir(&self) -> Result<PathBuf> {
        let base = self.root.join(BACKUP_DIR);
        fs::create_dir_all(&base).with_context(|| format!("creating {}", base.display()))?;
        let name = stamp(now_secs());
        for n in 1..1000 {
            let dir = base.join(if n == 1 { name.clone() } else { format!("{name}-{n}") });
            match fs::create_dir(&dir) {
                Ok(()) => return Ok(dir),
                Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e).with_context(|| format!("creating {}", dir.display())),
            }
        }
        bail!("no free backup folder name in {}", base.display())
    }

    /// Copy `path` into the backup folder, once.
    fn save(&mut self, path: &Path) -> Result<()> {
        if !self.enabled || self.saved.contains(path) {
            return Ok(());
        }
        let dir = match &self.dir {
            Some(d) => d.clone(),
            None => {
                let d = self.new_dir()?;
                self.dir = Some(d.clone());
                d
            }
        };
        let rel = match path.strip_prefix(&self.root) {
            Ok(rel) if !rel.as_os_str().is_empty() => rel.to_path_buf(),
            // A neighbour of the scanned folder (its entities or poi sibling).
            _ => {
                let parent = path.parent().and_then(Path::file_name).unwrap_or_default();
                Path::new("_outside").join(parent).join(path.file_name().unwrap_or_default())
            }
        };
        let dest = dir.join(rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        if dest.exists() {
            bail!("{} already exists", dest.display());
        }
        fs::copy(path, &dest).with_context(|| format!("copying {} to {}", path.display(), dest.display()))?;
        self.saved.insert(path.to_path_buf());
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Region files
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
enum Cleared {
    Yes,
    /// There was no such chunk.
    Absent,
    No(String),
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

fn write_at(file: &mut File, at: u64, bytes: &[u8]) -> std::io::Result<()> {
    file.seek(SeekFrom::Start(at))?;
    file.write_all(bytes)
}

/// Zero the header entries of the given slots. With `Some((loc, ts))` a slot
/// is only cleared if its entry still has exactly these values; with `None`
/// whatever is there goes. In a dry run the answers are the same but the
/// file is opened read-only.
fn clear_slots(
    path: &Path,
    wanted: &[(usize, Option<(u32, u32)>)],
    dry_run: bool,
    backup: &mut Backup,
    warnings: &mut Vec<String>,
) -> Vec<Cleared> {
    let all = |why: String| vec![Cleared::No(why); wanted.len()];
    let mut file = match File::options().read(true).write(!dry_run).open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == ErrorKind::NotFound => return all("its region file no longer exists".to_string()),
        Err(e) => return all(format!("its region file cannot be opened: {e}")),
    };
    let mut header = [0u8; HEADER as usize];
    match file.read_exact(&mut header) {
        Ok(()) => {}
        // Shorter than a header (often 0 bytes, which is normal): it holds no chunks.
        Err(e) if e.kind() == ErrorKind::UnexpectedEof => {
            return wanted
                .iter()
                .map(|(_, expect)| match expect {
                    Some(_) => Cleared::No("it changed since the scan; scan again".to_string()),
                    None => Cleared::Absent,
                })
                .collect();
        }
        Err(e) => return all(format!("the header of its region file cannot be read: {e}")),
    }
    let mut results: Vec<Cleared> = wanted
        .iter()
        .map(|&(idx, expect)| {
            let now = (be32(&header[idx * 4..]), be32(&header[SECTOR as usize + idx * 4..]));
            match expect {
                Some(seen) if seen != now => Cleared::No("it changed since the scan; scan again".to_string()),
                None if now.0 == 0 => Cleared::Absent,
                _ => Cleared::Yes,
            }
        })
        .collect();
    if dry_run || !results.contains(&Cleared::Yes) {
        return results;
    }
    if let Err(e) = backup.save(path) {
        let why = format!("its region file could not be backed up ({e:#}), so it was left alone");
        for r in results.iter_mut().filter(|r| **r == Cleared::Yes) {
            *r = Cleared::No(why.clone());
        }
        return results;
    }
    for (r, &(idx, _)) in results.iter_mut().zip(wanted) {
        if *r != Cleared::Yes {
            continue;
        }
        // The location entry is the chunk's existence; the timestamp is tidiness.
        match write_at(&mut file, idx as u64 * 4, &[0; 4]) {
            Ok(()) => {
                let _ = write_at(&mut file, SECTOR + idx as u64 * 4, &[0; 4]);
            }
            Err(e) => *r = Cleared::No(format!("its region file could not be written: {e}")),
        }
    }
    if let Err(e) = file.sync_all() {
        warnings.push(format!("{}: could not be flushed to disk: {e}", path.display()));
    }
    results
}

/// Remove the oversized-data file of a chunk that is being removed, if there
/// is one (whether the chunk used it or not: nothing can use it afterwards).
/// Returns the file's path if there was one.
fn remove_mcc(dir: &Path, (cx, cz): (i32, i32), dry_run: bool, backup: &mut Backup) -> Result<Option<PathBuf>> {
    let path = dir.join(world::mcc_name(cx, cz));
    if !path.is_file() {
        return Ok(None);
    }
    if !dry_run {
        backup.save(&path)?;
        fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    }
    Ok(Some(path))
}

/// Whether the region file next to an `.mcc` file stores that chunk externally.
fn is_referenced(mcc: &FileRef, (cx, cz): (i32, i32)) -> std::result::Result<bool, String> {
    let owner = mcc.dir().join(world::region_name(mcc.rx, mcc.rz));
    let unknown = |e: std::io::Error| format!("its region file cannot be checked: {e}");
    let mut file = match File::open(&owner) {
        Ok(f) => f,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(unknown(e)),
    };
    let mut header = [0u8; HEADER as usize];
    match file.read_exact(&mut header) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok(false),
        Err(e) => return Err(unknown(e)),
    }
    let off = u64::from(be32(&header[world::chunk_index(cx, cz) * 4..]) >> 8);
    if off < 2 {
        return Ok(false);
    }
    let mut version = [0u8; 1];
    match file.seek(SeekFrom::Start(off * SECTOR + 4)).and_then(|_| file.read_exact(&mut version)) {
        Ok(()) => Ok(version[0] & 0x80 != 0),
        Err(e) if e.kind() == ErrorKind::UnexpectedEof => Ok(false),
        Err(e) => Err(unknown(e)),
    }
}

fn remove_file(f: &Finding, dry_run: bool, backup: &mut Backup) -> std::result::Result<(), String> {
    let path = &f.file.path;
    let meta = match fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == ErrorKind::NotFound => return Err("it no longer exists".to_string()),
        Err(e) => return Err(format!("it cannot be examined: {e}")),
    };
    let changed = || Err("it changed since the scan; scan again".to_string());
    match f.kind {
        Kind::NoHeader if meta.len() >= HEADER || meta.len() == 0 => return changed(),
        Kind::OrphanMcc => {
            if let Some(pos) = f.pos {
                if is_referenced(&f.file, pos)? {
                    return Err("its region file uses it now; scan again".to_string());
                }
            }
        }
        _ => {}
    }
    if dry_run {
        return Ok(());
    }
    backup.save(path).map_err(|e| format!("it could not be backed up ({e:#}), so it was left alone"))?;
    fs::remove_file(path).map_err(|e| format!("it could not be removed: {e}"))
}

// ---------------------------------------------------------------------------
// The whole job
// ---------------------------------------------------------------------------

/// Fix `findings` (from a scan of `root`), or with `dry_run` report what
/// fixing them would do.
pub fn execute(root: &Path, findings: &[&Finding], opts: &FixOptions, progress: &Progress) -> Result<Report> {
    let mut warnings = Vec::new();
    // Held until the end, so the game cannot open the world half-way through.
    let _lock = match world::lock_world(root) {
        LockResult::Acquired(lock) => (!opts.dry_run).then_some(lock),
        LockResult::InUse(lock) => {
            if opts.dry_run {
                warnings.push("The world is open in a running game or server. Stop it before applying anything.".to_string());
            } else if opts.force {
                warnings.push("The world is open in a running game or server; went ahead because of --force.".to_string());
            } else {
                bail!(
                    "this world is open in a running game or server ({} is locked). Stop it first: it would overwrite the changes and can corrupt chunks it has loaded. Nothing was written.",
                    lock.display()
                );
            }
            None
        }
        LockResult::Unknown(lock, e) => {
            if !opts.dry_run && !opts.force {
                bail!(
                    "cannot tell whether the world is in use ({}: {e}). Nothing was written; use --force if you are sure it is not.",
                    lock.display()
                );
            }
            warnings.push(format!("Could not check whether the world is in use ({}: {e}).", lock.display()));
            None
        }
    };

    let mut outcomes: Vec<Outcome> =
        findings.iter().map(|f| Outcome { n: f.n, fix: f.fix(), status: Status::NoFix, notes: Vec::new() }).collect();
    let mut by_file: BTreeMap<&Path, Vec<usize>> = BTreeMap::new();
    let mut whole_files: Vec<usize> = Vec::new();
    for (i, f) in findings.iter().enumerate() {
        match (f.fix(), f.slot) {
            (Fix::DeleteChunk, Some(_)) => by_file.entry(f.file.path.as_path()).or_default().push(i),
            (Fix::DeleteFile, _) => whole_files.push(i),
            _ => {}
        }
    }
    progress.total.store((by_file.len() + whole_files.len()) as u64, Ordering::Relaxed);
    progress.done.store(0, Ordering::Relaxed);

    let mut backup = Backup { root: root.to_path_buf(), enabled: opts.backup, dir: None, saved: HashSet::new() };
    // Header entries cleared so far (in a dry run: that would be).
    let mut gone: HashSet<(PathBuf, usize)> = HashSet::new();
    // Region files changed and files removed so far (in a dry run: that would be).
    let mut touched: HashSet<PathBuf> = HashSet::new();

    for (path, members) in by_file {
        let file = findings[members[0]].file.clone();
        let mut asked: Vec<usize> = Vec::new();
        let mut wanted = Vec::new();
        for &i in &members {
            let Some(slot) = findings[i].slot else { continue };
            if gone.contains(&(path.to_path_buf(), slot.idx as usize)) {
                outcomes[i].status = Status::Removed;
                outcomes[i].notes.push("its terrain chunk".to_string());
            } else {
                asked.push(i);
                wanted.push((slot.idx as usize, Some((slot.loc, slot.ts))));
            }
        }
        let results = clear_slots(path, &wanted, opts.dry_run, &mut backup, &mut warnings);
        let mut cleared: Vec<(usize, usize)> = Vec::new();
        for ((&i, &(idx, _)), result) in asked.iter().zip(&wanted).zip(results) {
            match result {
                Cleared::Yes => {
                    outcomes[i].status = Status::Removed;
                    gone.insert((path.to_path_buf(), idx));
                    touched.insert(path.to_path_buf());
                    cleared.push((i, idx));
                }
                Cleared::Absent => outcomes[i].status = Status::Skipped("it no longer exists".to_string()),
                Cleared::No(why) => outcomes[i].status = Status::Skipped(why),
            }
        }
        for &(i, idx) in &cleared {
            let pos = file.chunk_pos(idx);
            match remove_mcc(file.dir(), pos, opts.dry_run, &mut backup) {
                Ok(Some(path)) => {
                    outcomes[i].notes.push(world::mcc_name(pos.0, pos.1));
                    touched.insert(path);
                }
                Ok(None) => {}
                Err(e) => warnings.push(format!("{} was left behind: {e:#}", world::mcc_name(pos.0, pos.1))),
            }
        }

        // What belongs to a terrain chunk goes with it.
        if !(opts.linked && file.store == Store::Region) || cleared.is_empty() {
            progress.done.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        for sibling in ["entities", "poi"] {
            let (Some(dimension), Some(name)) = (file.dir().parent(), file.path.file_name()) else { continue };
            let sib_dir = dimension.join(sibling);
            let sib_path = sib_dir.join(name);
            if !sib_path.is_file() {
                continue;
            }
            let todo: Vec<(usize, usize)> =
                cleared.iter().copied().filter(|&(_, idx)| !gone.contains(&(sib_path.clone(), idx))).collect();
            let wanted: Vec<(usize, Option<(u32, u32)>)> = todo.iter().map(|&(_, idx)| (idx, None)).collect();
            let results = clear_slots(&sib_path, &wanted, opts.dry_run, &mut backup, &mut warnings);
            for (&(i, idx), result) in todo.iter().zip(results) {
                let pos = file.chunk_pos(idx);
                match result {
                    Cleared::Yes => {
                        gone.insert((sib_path.clone(), idx));
                        touched.insert(sib_path.clone());
                        outcomes[i].notes.push(format!("its {sibling} data"));
                        match remove_mcc(&sib_dir, pos, opts.dry_run, &mut backup) {
                            Ok(Some(path)) => {
                                touched.insert(path);
                            }
                            Ok(None) => {}
                            Err(e) => {
                                warnings.push(format!("{sibling}/{} was left behind: {e:#}", world::mcc_name(pos.0, pos.1)))
                            }
                        }
                    }
                    Cleared::Absent => {}
                    Cleared::No(why) => {
                        warnings.push(format!("The {sibling} data of chunk {},{} was not removed: {why}.", pos.0, pos.1));
                    }
                }
            }
        }
        progress.done.fetch_add(1, Ordering::Relaxed);
    }

    for i in whole_files {
        // Already gone (in a dry run: would be) together with its chunk.
        if touched.contains(&findings[i].file.path) {
            outcomes[i].status = Status::Removed;
            progress.done.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        match remove_file(findings[i], opts.dry_run, &mut backup) {
            Ok(()) => {
                outcomes[i].status = Status::Removed;
                touched.insert(findings[i].file.path.clone());
            }
            Err(why) => outcomes[i].status = Status::Skipped(why),
        }
        progress.done.fetch_add(1, Ordering::Relaxed);
    }

    Ok(Report { dry_run: opts.dry_run, outcomes, backup_dir: backup.dir, files: touched.len() as u64, warnings })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::testing::zlib;
    use crate::scan::tests::{sample_world, scan};
    use crate::scan::{ScanOutput, Severity};
    use crate::testutil::{entities, good_terrain, poi, Region, TempDir};
    use std::collections::BTreeMap;

    const DRY: FixOptions = FixOptions { dry_run: true, backup: true, force: false, linked: true };
    const REAL: FixOptions = FixOptions { dry_run: false, ..DRY };

    /// Every file below `dir`, with its content.
    fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        fn walk(dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
            for e in fs::read_dir(dir).unwrap().flatten() {
                if e.path().is_dir() {
                    walk(&e.path(), out);
                } else {
                    out.insert(e.path(), fs::read(e.path()).unwrap());
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(dir, &mut out);
        out
    }

    fn run(root: &Path, out: &ScanOutput, pick: impl Fn(&Finding) -> bool, opts: &FixOptions) -> Report {
        let picked: Vec<&Finding> = out.findings.iter().filter(|f| pick(f)).collect();
        execute(root, &picked, opts, &Progress::default()).unwrap()
    }

    fn left(root: &Path) -> Vec<(String, Option<u16>, Kind)> {
        let mut v: Vec<_> =
            scan(root, false, 4).findings.iter().map(|f| (f.file.rel.clone(), f.slot.map(|s| s.idx), f.kind)).collect();
        v.sort();
        v
    }

    fn slot_set(path: &Path) -> Vec<usize> {
        let buf = fs::read(path).unwrap();
        (0..1024).filter(|i| be32(&buf[i * 4..]) != 0).collect()
    }

    #[test]
    fn dry_run_writes_nothing_and_predicts_the_real_run() {
        let tmp = TempDir::new("dry");
        let w = tmp.path();
        sample_world(w);
        let out = scan(w, false, 4);
        let before = snapshot(w);
        let dry = run(w, &out, |_| true, &DRY);
        assert_eq!(snapshot(w), before, "a dry run changed the world");
        assert!(dry.dry_run && dry.backup_dir.is_none());
        assert!(dry.summary().starts_with("DRY RUN: would remove 20 chunks and 3 files; 1 cannot be fixed"), "{}", dry.summary());
        assert!(dry.summary().ends_with("(6 files would be touched). Nothing was written."), "{}", dry.summary());

        let real = run(w, &out, |_| true, &REAL);
        let status = |r: &Report| r.outcomes.iter().map(|o| (o.n, o.status.clone(), o.notes.clone())).collect::<Vec<_>>();
        assert_eq!(status(&dry), status(&real));
        assert_eq!(dry.files, real.files);
        assert!(real.summary().starts_with("Done: removed 20 chunks and 3 files"), "{}", real.summary());
        for (o, f) in real.outcomes.iter().zip(&out.findings) {
            assert_eq!(o.n, f.n);
            assert!(!o.line(f, false).is_empty());
        }
    }

    #[test]
    fn removes_what_was_selected_and_nothing_else() {
        let tmp = TempDir::new("apply");
        let w = tmp.path();
        sample_world(w);
        let out = scan(w, false, 4);
        let before = snapshot(w);
        let report = run(w, &out, |f| f.severity() == Severity::Error, &REAL);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert_eq!(report.counts(), (18, 1, 0, 0));

        // Only the warnings are left; chunk 15 no longer shares its sectors.
        // (The unused c.5.0.mcc went with chunk 5,0.)
        let want: Vec<(String, Option<u16>, Kind)> = [
            ("region/c.100.100.mcc", None, Kind::OrphanMcc),
            ("region/r.0.0.mca", Some(14), Kind::TruncatedStream),
            ("region/r.0.0.mca", Some(17), Kind::Unverified),
        ]
        .map(|(a, b, c)| (a.to_string(), b, c))
        .into();
        assert_eq!(left(w), want);

        // Healthy chunks are all still there, damaged ones are not.
        assert_eq!(slot_set(&w.join("region/r.0.0.mca")), [0, 14, 15, 17, 18, 20, 21, 22, 23, 24]);
        assert_eq!(slot_set(&w.join("DIM-1/region/r.-1.-1.mca")), [0, 1023]);
        // Entity and POI data of removed terrain went with it; the rest stayed.
        assert_eq!(slot_set(&w.join("entities/r.0.0.mca")), [0]);
        assert_eq!(slot_set(&w.join("poi/r.0.0.mca")), [0]);
        assert!(!w.join("region/r.2.0.mca").exists());
        assert!(w.join("region/c.18.0.mcc").exists());

        // Apart from header entries, not a byte of any region file changed.
        let after = snapshot(w);
        for (path, old) in &before {
            let Some(new) = after.get(path) else { continue };
            assert_eq!(old.len(), new.len(), "{}", path.display());
            assert_eq!(old[8192.min(old.len())..], new[8192.min(new.len())..], "{}", path.display());
        }

        // Every changed or removed file was backed up as it was.
        let dir = report.backup_dir.clone().unwrap();
        assert!(dir.starts_with(w.join(BACKUP_DIR)));
        let saved = snapshot(&dir);
        let rels: Vec<String> =
            saved.keys().map(|p| p.strip_prefix(&dir).unwrap().to_string_lossy().replace('\\', "/")).collect();
        assert_eq!(rels, ["entities/r.0.0.mca", "poi/r.0.0.mca", "region/c.5.0.mcc", "region/r.0.0.mca", "region/r.2.0.mca"]);
        for (path, content) in &saved {
            assert_eq!(Some(content), before.get(&w.join(path.strip_prefix(&dir).unwrap())));
        }
        assert_eq!(report.files, 5);

        // Restoring the backup brings every finding back.
        for (path, content) in &saved {
            fs::write(w.join(path.strip_prefix(&dir).unwrap()), content).unwrap();
        }
        let again = scan(w, false, 4);
        assert_eq!(again.findings.len(), out.findings.len());
    }

    #[test]
    fn everything_selected_leaves_only_what_cannot_be_fixed() {
        let tmp = TempDir::new("all");
        let w = tmp.path();
        sample_world(w);
        let out = scan(w, false, 4);
        let report = run(w, &out, |_| true, &REAL);
        assert_eq!(report.counts(), (20, 3, 0, 1));
        assert_eq!(left(w), [("region/r.0.0.mca".to_string(), Some(17), Kind::Unverified)]);
        // The entity chunks that had findings of their own are reported once each.
        let by_rel = |rel: &str, idx: u16| {
            let f = out.findings.iter().find(|f| f.file.rel == rel && f.slot.map(|s| s.idx) == Some(idx)).unwrap();
            report.outcomes.iter().find(|o| o.n == f.n).unwrap()
        };
        assert_eq!(by_rel("entities/r.0.0.mca", 1).status, Status::Removed);
        assert_eq!(by_rel("region/r.0.0.mca", 9).notes, ["its entities data", "its poi data"]);
        assert_eq!(by_rel("region/r.0.0.mca", 12).notes, Vec::<String>::new());
        // A second run has nothing left to do and creates no backup.
        let out = scan(w, false, 4);
        let report = run(w, &out, |_| true, &REAL);
        assert_eq!(report.counts(), (0, 0, 0, 1));
        assert!(report.backup_dir.is_none());
    }

    #[test]
    fn chunks_saved_since_the_scan_are_left_alone() {
        let tmp = TempDir::new("stale");
        let w = tmp.path();
        sample_world(w);
        let out = scan(w, false, 4);
        // The "game" saves chunk 9 again (new timestamp) and deletes a file.
        let path = w.join("region/r.0.0.mca");
        let mut buf = fs::read(&path).unwrap();
        buf[4096 + 9 * 4..4096 + 9 * 4 + 4].copy_from_slice(&1_800_000_000u32.to_be_bytes());
        fs::write(&path, &buf).unwrap();
        fs::remove_file(w.join("region/c.100.100.mcc")).unwrap();
        fs::write(w.join("region/r.2.0.mca"), [0u8; 9000]).unwrap();
        fs::remove_file(w.join("poi/r.0.0.mca")).unwrap();

        for opts in [&DRY, &REAL] {
            let report = run(w, &out, |_| true, opts);
            let status = |rel: &str, idx: Option<u16>| {
                let f = out.findings.iter().find(|f| f.file.rel == rel && f.slot.map(|s| s.idx) == idx).unwrap();
                report.outcomes.iter().find(|o| o.n == f.n).unwrap().status.clone()
            };
            assert!(matches!(status("region/r.0.0.mca", Some(9)), Status::Skipped(why) if why.contains("changed since the scan")));
            assert!(matches!(status("region/c.100.100.mcc", None), Status::Skipped(why) if why.contains("no longer exists")));
            assert!(matches!(status("region/r.2.0.mca", None), Status::Skipped(why) if why.contains("changed since the scan")));
            assert!(matches!(status("poi/r.0.0.mca", Some(1)), Status::Skipped(why) if why.contains("no longer exists")));
            assert_eq!(status("region/r.0.0.mca", Some(10)), Status::Removed);
        }
        // Chunk 9 and its entity data survived.
        assert!(slot_set(&path).contains(&9));
        assert!(slot_set(&w.join("entities/r.0.0.mca")).contains(&9));
    }

    #[test]
    fn options_backup_and_linked() {
        let tmp = TempDir::new("opts");
        let w = tmp.path();
        sample_world(w);
        let out = scan(w, false, 4);
        let opts = FixOptions { backup: false, linked: false, ..REAL };
        let report = run(w, &out, |f| f.file.rel == "region/r.0.0.mca" && f.severity() == Severity::Error, &opts);
        assert!(report.backup_dir.is_none());
        assert!(!w.join(BACKUP_DIR).exists());
        // Entity data of the removed terrain chunks 9 and 13 is still there.
        assert_eq!(slot_set(&w.join("entities/r.0.0.mca")), [0, 1, 2, 9, 13]);
        assert!(!slot_set(&w.join("region/r.0.0.mca")).contains(&9));
    }

    #[test]
    fn oversized_chunks_lose_their_mcc_file() {
        let tmp = TempDir::new("mcc");
        let w = tmp.path();
        let mut r = Region::new();
        r.add_raw(3, 0x82, &[]);
        r.add_raw(4, 0x82, &[]);
        r.write(&w.join("region/r.0.0.mca"));
        fs::write(w.join("region/c.3.0.mcc"), b"garbage").unwrap();
        fs::write(w.join("region/c.4.0.mcc"), zlib(&good_terrain(4, 0))).unwrap();
        let mut e = Region::new();
        e.add_raw(3, 0x82, &[]);
        e.add(4, &entities(4, 0));
        e.write(&w.join("entities/r.0.0.mca"));
        fs::write(w.join("entities/c.3.0.mcc"), zlib(&entities(3, 0))).unwrap();
        let mut p = Region::new();
        p.add(4, &poi());
        p.write(&w.join("poi/r.0.0.mca"));

        let out = scan(w, false, 2);
        assert_eq!(out.findings.len(), 1, "{:?}", out.findings);
        assert_eq!(out.findings[0].kind, Kind::Corrupt);
        let dry = run(w, &out, |_| true, &DRY);
        assert_eq!(dry.outcomes[0].notes, ["c.3.0.mcc", "its entities data"]);
        assert!(w.join("region/c.3.0.mcc").exists());

        let report = run(w, &out, |_| true, &REAL);
        assert_eq!(report.outcomes[0].notes, ["c.3.0.mcc", "its entities data"]);
        assert!(report.warnings.is_empty());
        assert!(!w.join("region/c.3.0.mcc").exists());
        assert!(!w.join("entities/c.3.0.mcc").exists());
        assert!(w.join("region/c.4.0.mcc").exists());
        let dir = report.backup_dir.unwrap();
        assert_eq!(fs::read(dir.join("region/c.3.0.mcc")).unwrap(), b"garbage");
        assert!(dir.join("entities/c.3.0.mcc").exists() && dir.join("entities/r.0.0.mca").exists());
        assert!(left(w).is_empty());
        assert_eq!(slot_set(&w.join("region/r.0.0.mca")), [4]);
        assert_eq!(slot_set(&w.join("entities/r.0.0.mca")), [4]);
        assert_eq!(slot_set(&w.join("poi/r.0.0.mca")), [4]);
    }

    #[test]
    fn empty_neighbours_are_not_a_problem() {
        // The game leaves 0-byte entities and poi files around; they hold nothing to remove.
        let tmp = TempDir::new("empty");
        let w = tmp.path();
        let mut r = Region::new();
        r.add(0, &good_terrain(0, 0));
        r.add_raw(1, 2, b"not zlib");
        r.write(&w.join("region/r.0.0.mca"));
        fs::create_dir_all(w.join("entities")).unwrap();
        fs::create_dir_all(w.join("poi")).unwrap();
        fs::write(w.join("entities/r.0.0.mca"), b"").unwrap();
        fs::write(w.join("poi/r.0.0.mca"), [0u8; 4096]).unwrap();
        let out = scan(w, false, 2);
        assert_eq!(out.findings.iter().map(|f| f.kind).collect::<Vec<_>>(), [Kind::Corrupt, Kind::NoHeader]);
        for opts in [&DRY, &REAL] {
            let report = run(w, &out, |f| f.kind == Kind::Corrupt, opts);
            assert!(report.warnings.is_empty(), "{:?}", report.warnings);
            assert_eq!(report.outcomes[0].status, Status::Removed);
            assert!(report.outcomes[0].notes.is_empty());
            assert_eq!(report.files, 1);
        }
        assert_eq!(slot_set(&w.join("region/r.0.0.mca")), [0]);
        assert_eq!(fs::read(w.join("poi/r.0.0.mca")).unwrap().len(), 4096);
        // A file that changed since the scan is left alone.
        let out = scan(w, false, 2);
        fs::write(w.join("poi/r.0.0.mca"), b"").unwrap();
        let report = run(w, &out, |_| true, &REAL);
        assert!(matches!(&report.outcomes[0].status, Status::Skipped(why) if why.contains("changed since the scan")));
    }

    #[test]
    fn an_orphan_that_is_used_again_is_kept() {
        let tmp = TempDir::new("orphan");
        let w = tmp.path();
        let mut r = Region::new();
        r.add(0, &good_terrain(0, 0));
        r.write(&w.join("region/r.0.0.mca"));
        fs::write(w.join("region/c.7.0.mcc"), zlib(&good_terrain(7, 0))).unwrap();
        let out = scan(w, false, 2);
        assert_eq!(out.findings[0].kind, Kind::OrphanMcc);
        // The game saves chunk 7 as an oversized chunk.
        r.add_raw(7, 0x82, &[]);
        r.write(&w.join("region/r.0.0.mca"));
        let report = run(w, &out, |_| true, &REAL);
        assert!(matches!(&report.outcomes[0].status, Status::Skipped(why) if why.contains("uses it now")));
        assert!(w.join("region/c.7.0.mcc").exists());
        assert!(left(w).is_empty());
    }

    #[test]
    fn scanning_a_region_folder_directly() {
        let tmp = TempDir::new("direct");
        let w = tmp.path();
        sample_world(w);
        let root = w.join("region");
        let out = scan(&root, false, 2);
        assert!(out.findings.iter().all(|f| !f.file.rel.contains('/')));
        let report = run(&root, &out, |f| f.slot.map(|s| s.idx) == Some(9), &REAL);
        assert_eq!(report.outcomes[0].notes, ["its entities data", "its poi data"]);
        let dir = report.backup_dir.unwrap();
        assert!(dir.starts_with(root.join(BACKUP_DIR)));
        assert!(dir.join("r.0.0.mca").is_file());
        assert!(dir.join("_outside/entities/r.0.0.mca").is_file());
        assert!(dir.join("_outside/poi/r.0.0.mca").is_file());
        // The backup folder inside the region folder is not scanned.
        assert!(scan(&root, false, 2).findings.iter().all(|f| !f.file.rel.contains(BACKUP_DIR)));
    }
}
