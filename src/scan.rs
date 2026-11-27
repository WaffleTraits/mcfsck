//! The scan: every region file is checked the way the game reads it, in
//! parallel, and everything the game could not load is reported.
//!
//! The scan only ever reads.

use crate::codec::{self, Codec, DecodeError};
use crate::nbt::{self, ChunkInfo};
use crate::util::{human_bytes, human_count, s};
use crate::world::{self, FileRef, Store, World};
use anyhow::{anyhow, bail, Result};
use rayon::prelude::*;
use serde::Serialize;
use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const SECTOR: u64 = 4096;
pub const HEADER: u64 = 8192;
pub const SLOTS: usize = 1024;
/// Region files up to this size are read in one go; larger ones chunk by chunk.
const WHOLE_LIMIT: u64 = 64 << 20;
/// First data version (1.18) whose terrain sections use the container
/// layout that [`nbt::expected_longs`] describes.
const SECTIONS_SINCE: i32 = 2860;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// The game cannot load this; whatever was stored is lost or unreachable.
    Error,
    /// Odd, but the game copes.
    Warning,
}

impl Severity {
    pub fn name(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            Severity::Error => "ERROR",
            Severity::Warning => "warn",
        }
    }
}

/// What `fix` does about a finding.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fix {
    /// Remove the chunk from its region file; the game generates it anew.
    DeleteChunk,
    /// Remove the file.
    DeleteFile,
    /// Nothing this tool can do.
    None,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Kind {
    // Whole files.
    Unreadable,
    NoHeader,
    OrphanMcc,
    // Header entries.
    HeaderOverlap,
    ZeroSectors,
    OutOfBounds,
    // Chunk records.
    NoChunkHeader,
    MissingStream,
    NegativeLength,
    Truncated,
    BadCompression,
    ExternalMissing,
    // Chunk content.
    Corrupt,
    BadNbt,
    NoPosition,
    WrongPosition,
    BadSection,
    // Warnings.
    TruncatedStream,
    SharedSectors,
    Unverified,
}

impl Kind {
    pub const ALL: [Kind; 20] = [
        Kind::Unreadable,
        Kind::NoHeader,
        Kind::OrphanMcc,
        Kind::HeaderOverlap,
        Kind::ZeroSectors,
        Kind::OutOfBounds,
        Kind::NoChunkHeader,
        Kind::MissingStream,
        Kind::NegativeLength,
        Kind::Truncated,
        Kind::BadCompression,
        Kind::ExternalMissing,
        Kind::Corrupt,
        Kind::BadNbt,
        Kind::NoPosition,
        Kind::WrongPosition,
        Kind::BadSection,
        Kind::TruncatedStream,
        Kind::SharedSectors,
        Kind::Unverified,
    ];

    /// Stable name for reports and filters.
    pub fn code(self) -> &'static str {
        match self {
            Kind::Unreadable => "unreadable",
            Kind::NoHeader => "no_header",
            Kind::OrphanMcc => "orphan_mcc",
            Kind::HeaderOverlap => "header_overlap",
            Kind::ZeroSectors => "zero_sectors",
            Kind::OutOfBounds => "out_of_bounds",
            Kind::NoChunkHeader => "cut_off",
            Kind::MissingStream => "missing_stream",
            Kind::NegativeLength => "negative_length",
            Kind::Truncated => "truncated",
            Kind::BadCompression => "bad_compression",
            Kind::ExternalMissing => "external_missing",
            Kind::Corrupt => "corrupt",
            Kind::BadNbt => "bad_nbt",
            Kind::NoPosition => "no_position",
            Kind::WrongPosition => "wrong_position",
            Kind::BadSection => "bad_section",
            Kind::TruncatedStream => "truncated_stream",
            Kind::SharedSectors => "shared_sectors",
            Kind::Unverified => "unverified",
        }
    }

    /// A few words for the list.
    pub fn label(self) -> &'static str {
        match self {
            Kind::Unreadable => "cannot be read",
            Kind::NoHeader => "region file without header",
            Kind::OrphanMcc => "unused oversized-chunk file",
            Kind::HeaderOverlap => "points into the file header",
            Kind::ZeroSectors => "has no sectors",
            Kind::OutOfBounds => "beyond the end of the file",
            Kind::NoChunkHeader => "cut off by the end of the file",
            Kind::MissingStream => "allocated but empty",
            Kind::NegativeLength => "impossible length",
            Kind::Truncated => "longer than its sectors",
            Kind::BadCompression => "unknown compression",
            Kind::ExternalMissing => "oversized-chunk file missing",
            Kind::Corrupt => "damaged compressed data",
            Kind::BadNbt => "damaged chunk data",
            Kind::NoPosition => "no chunk position",
            Kind::WrongPosition => "wrong chunk position",
            Kind::BadSection => "unloadable section",
            Kind::TruncatedStream => "compressed data ends early",
            Kind::SharedSectors => "shares sectors",
            Kind::Unverified => "could not be checked",
        }
    }

    pub fn severity(self) -> Severity {
        match self {
            Kind::OrphanMcc | Kind::TruncatedStream | Kind::SharedSectors | Kind::Unverified => Severity::Warning,
            _ => Severity::Error,
        }
    }

    /// What the game does when it meets this, for the details view.
    pub fn explain(self) -> &'static str {
        match self {
            Kind::Unreadable => "The operating system refused to read this. Check the disk and the file permissions.",
            Kind::NoHeader => {
                "A region file starts with 8 KiB of header, and chunk data can only follow after that, so this file cannot hold a single readable chunk."
            }
            Kind::OrphanMcc => {
                "Chunks too large for a region file are stored in their own .mcc file. Nothing refers to this one any more, so the game never reads it."
            }
            Kind::HeaderOverlap | Kind::ZeroSectors | Kind::OutOfBounds => {
                "The game drops such an entry when it opens the region file, logs a warning, and generates the chunk anew."
            }
            Kind::NoChunkHeader | Kind::MissingStream | Kind::NegativeLength | Kind::Truncated | Kind::BadCompression => {
                "The game cannot read this chunk; it logs an error and generates the chunk anew, or keeps failing on every load."
            }
            Kind::ExternalMissing => "The chunk is too large for the region file and its separate .mcc file is gone, so the chunk is lost.",
            Kind::Corrupt | Kind::BadNbt => {
                "The stored data is damaged and the game cannot load it. Depending on the version it regenerates the chunk, logs errors on every load, or crashes."
            }
            Kind::NoPosition | Kind::WrongPosition => {
                "The data stored here belongs to another chunk (or to none). The game refuses it or loads terrain in the wrong place."
            }
            Kind::BadSection => {
                "A section's block or biome data does not have the size its palette requires. The game fails to load the chunk."
            }
            Kind::TruncatedStream => {
                "The end of the compressed stream is missing, but everything the game reads is there. It probably still loads; saving the chunk in game repairs it."
            }
            Kind::SharedSectors => {
                "Two header entries claim the same part of the file. This chunk reads fine right now, but when either chunk is saved again it can overwrite the other."
            }
            Kind::Unverified => "This tool could not look inside. It is not known to be damaged.",
        }
    }
}

/// A chunk's entry in the region header, as it was when scanned. `fix` only
/// touches a chunk whose entry is still exactly this.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Slot {
    pub idx: u16,
    /// Sector offset (upper 24 bits) and sector count (lowest 8).
    pub loc: u32,
    /// When the game last saved the chunk, in seconds since 1970.
    pub ts: u32,
}

#[derive(Clone, Debug)]
pub struct Finding {
    /// Position in the report, from 1.
    pub n: u64,
    pub file: Arc<FileRef>,
    /// Chunk coordinates, if this is about one chunk.
    pub pos: Option<(i32, i32)>,
    pub slot: Option<Slot>,
    pub kind: Kind,
    pub detail: String,
}

impl Finding {
    pub fn severity(&self) -> Severity {
        self.kind.severity()
    }

    pub fn fix(&self) -> Fix {
        match self.kind {
            Kind::NoHeader | Kind::OrphanMcc => Fix::DeleteFile,
            Kind::Unreadable | Kind::Unverified => Fix::None,
            _ if self.slot.is_some() => Fix::DeleteChunk,
            _ => Fix::None,
        }
    }

    /// "overworld region"
    pub fn place(&self) -> String {
        format!("{} {}", self.file.dim_short(), self.file.store.name())
    }

    /// "12,-5", or "-" for a whole file.
    pub fn chunk_text(&self) -> String {
        self.pos.map_or("-".to_string(), |(x, z)| format!("{x},{z}"))
    }

    /// What the finding is about, for sentences.
    pub fn target(&self) -> String {
        match (self.slot, self.pos) {
            (Some(_), Some((x, z))) => format!("chunk {x},{z} in {}", self.file.rel),
            _ => format!("file {}", self.file.rel),
        }
    }

    pub fn problem(&self) -> String {
        format!("{}: {}", self.kind.label(), self.detail)
    }

    /// One line for plain-text listings.
    pub fn line(&self) -> String {
        format!("#{} {:<5} {} - {}", self.n, self.severity().short(), self.target(), self.problem())
    }
}

#[derive(Clone, Debug, Default)]
pub struct Options {
    /// Only check headers; do not decompress chunk data.
    pub quick: bool,
    pub threads: Option<usize>,
}

/// Live counters, readable from the UI thread while the scan runs.
#[derive(Default)]
pub struct Stats {
    pub total_files: AtomicU64,
    pub files_done: AtomicU64,
    pub total_bytes: AtomicU64,
    pub bytes_done: AtomicU64,
    pub chunks: AtomicU64,
    pub errors: AtomicU64,
    pub warnings: AtomicU64,
    pub cancel: AtomicBool,
}

#[derive(Debug)]
pub struct ScanOutput {
    pub root: PathBuf,
    pub findings: Vec<Finding>,
    /// Region files per store, in the order of [`Store::ALL`].
    pub files: [u64; 4],
    /// Region files of 0 bytes. The game creates those; they are not a problem.
    pub empty_files: u64,
    /// Block and biome containers whose size was checked.
    pub containers: u64,
    pub mcc_files: u64,
    pub bytes: u64,
    /// Chunks that have a header entry.
    pub chunks: u64,
    pub dims: Vec<String>,
    pub elapsed: Duration,
    pub quick: bool,
    pub threads: usize,
}

impl ScanOutput {
    pub fn count(&self, severity: Severity) -> u64 {
        self.findings.iter().filter(|f| f.severity() == severity).count() as u64
    }

    pub fn total_files(&self) -> u64 {
        self.files.iter().sum()
    }

    pub fn speed_line(&self) -> String {
        let secs = self.elapsed.as_secs_f64().max(1e-3);
        format!(
            "Checked {} chunk{} in {} region file{} ({}) in {:.1}s - {}/s on {} thread{}",
            human_count(self.chunks),
            s(self.chunks),
            human_count(self.total_files()),
            s(self.total_files()),
            human_bytes(self.bytes),
            self.elapsed.as_secs_f64(),
            human_bytes((self.bytes as f64 / secs) as u64),
            self.threads,
            s(self.threads as u64),
        )
    }

    pub fn verdict(&self) -> String {
        let (e, w) = (self.count(Severity::Error), self.count(Severity::Warning));
        if e + w == 0 {
            "No problems found.".to_string()
        } else {
            format!("Found {} error{} and {} warning{}.", human_count(e), s(e), human_count(w), s(w))
        }
    }

    /// What the scan looked into.
    pub fn coverage_lines(&self) -> Vec<String> {
        let mut lines = vec![self.speed_line()];
        let per_store: Vec<String> = Store::ALL
            .iter()
            .zip(self.files)
            .filter(|(_, n)| *n > 0)
            .map(|(store, n)| format!("{} {}", human_count(n), store.name()))
            .collect();
        let empty = if self.empty_files > 0 {
            format!(" ({} of them empty, which is normal)", human_count(self.empty_files))
        } else {
            String::new()
        };
        lines.push(format!(
            "Region files: {}{empty}; oversized-chunk files: {}",
            if per_store.is_empty() { "none".to_string() } else { per_store.join(", ") },
            human_count(self.mcc_files)
        ));
        if !self.dims.is_empty() {
            lines.push(format!("Dimensions: {}", self.dims.join(", ")));
        }
        lines.push(if self.quick {
            "Quick scan: region headers and chunk headers only. Chunk data was NOT decompressed or validated.".to_string()
        } else {
            format!(
                "Full scan: headers, decompression (with checksums), NBT structure, chunk positions, and the sizes of {} block and biome containers.",
                human_count(self.containers)
            )
        });
        lines
    }
}

// ---------------------------------------------------------------------------
// One region file
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Scratch {
    whole: Vec<u8>,
    raw: Vec<u8>,
    ext: Vec<u8>,
    out: Vec<u8>,
    codec: Codec,
}

impl Scratch {
    /// Do not let one giant chunk pin memory in every worker for the rest of the scan.
    fn trim(&mut self) {
        for buf in [&mut self.raw, &mut self.ext, &mut self.out] {
            if buf.capacity() > 32 << 20 {
                *buf = Vec::new();
            }
        }
    }
}

#[derive(Default)]
struct FileScan {
    findings: Vec<Finding>,
    /// Chunks whose data lives in a `.mcc` file.
    externals: Vec<(i32, i32)>,
    unreadable: bool,
    empty: bool,
    chunks: u64,
    /// Block and biome containers whose size was checked.
    containers: u64,
}

struct Cand {
    off: u64,
    cnt: u64,
    slot: Slot,
}

type Problem = (Kind, String);

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

fn content_problem(store: Store, expected: (i32, i32), info: &ChunkInfo) -> Option<Problem> {
    let found = match store {
        Store::Region => match info.terrain_pos() {
            None => return Some((Kind::NoPosition, "its data has no xPos/zPos; this is not a terrain chunk".to_string())),
            p => p,
        },
        Store::Entities => match info.entity_pos {
            None => return Some((Kind::NoPosition, "its data has no Position; this is not an entity chunk".to_string())),
            p => p,
        },
        Store::Poi => None,
        Store::Other => info.terrain_pos().or(info.entity_pos),
    };
    if let Some((x, z)) = found {
        if (x, z) != expected {
            return Some((Kind::WrongPosition, format!("the data stored here says it is chunk {x},{z}")));
        }
    }
    if matches!(store, Store::Region | Store::Other) && info.data_version.is_some_and(|v| v >= SECTIONS_SINCE) {
        if let Some(p) = &info.section_problem {
            return Some((Kind::BadSection, p.clone()));
        }
    }
    None
}

/// Check one chunk record. `data` holds its sectors (in a quick scan only
/// the first bytes); `avail` is how much of them lies inside the file.
#[allow(clippy::too_many_arguments)]
fn examine(
    f: &FileRef,
    pos: (i32, i32),
    data: &[u8],
    avail: usize,
    quick: bool,
    ext: &mut Vec<u8>,
    out: &mut Vec<u8>,
    dec: &mut Codec,
    externals: &mut Vec<(i32, i32)>,
    containers: &mut u64,
) -> Option<Problem> {
    if avail < 5 || data.len() < 5 {
        return Some((Kind::NoChunkHeader, format!("only {avail} byte{} of it lie inside the file", s(avail as u64))));
    }
    let length = i32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    let version = data[4];
    if length == 0 {
        return Some((
            Kind::MissingStream,
            "its sectors are reserved but hold no data (typically left behind by a crash)".to_string(),
        ));
    }
    if length < 0 {
        return Some((Kind::NegativeLength, format!("it declares a length of {length} bytes")));
    }
    let scheme = version & 0x7F;
    let bad_scheme = || match scheme {
        s if codec::known_scheme(s) => None,
        127 => Some((Kind::Unverified, "it uses a custom compression scheme this tool cannot read".to_string())),
        s => Some((Kind::BadCompression, format!("it uses compression scheme {s}, which the game does not know"))),
    };
    let payload: &[u8] = if version & 0x80 != 0 {
        externals.push(pos);
        let name = world::mcc_name(pos.0, pos.1);
        let path = f.dir().join(&name);
        match std::fs::metadata(&path) {
            Ok(m) if m.is_file() => {
                if m.len() > codec::MAX_OUT as u64 {
                    return Some((Kind::Unverified, format!("{name} is larger than 512 MiB; not checked")));
                }
            }
            _ => return Some((Kind::ExternalMissing, format!("its data should be in {name}, which does not exist"))),
        }
        if let Some(p) = bad_scheme() {
            return Some(p);
        }
        if quick {
            return None;
        }
        ext.clear();
        if let Err(e) = File::open(&path).and_then(|mut m| m.read_to_end(ext)) {
            return Some((Kind::Unreadable, format!("{name} cannot be read: {e}")));
        }
        ext
    } else {
        let n = (length - 1) as usize;
        if n > avail - 5 {
            return Some((
                Kind::Truncated,
                format!("it declares {n} bytes of data, but only {} fit in its sectors inside the file", avail - 5),
            ));
        }
        if let Some(p) = bad_scheme() {
            return Some(p);
        }
        if quick {
            return None;
        }
        &data[5..5 + n]
    };

    let sections = matches!(f.store, Store::Region | Store::Other);
    match dec.decompress(scheme, payload, out) {
        Ok(()) => {}
        Err(DecodeError::Corrupt(why)) => {
            return Some((Kind::Corrupt, format!("its {} data cannot be decompressed ({why})", codec::scheme_name(scheme))));
        }
        Err(DecodeError::TooLarge) => {
            return Some((Kind::Unverified, "it decompresses to more than 512 MiB; not checked".to_string()));
        }
        Err(DecodeError::Truncated) => {
            return Some(match nbt::inspect(out, sections) {
                Ok(info) if content_problem(f.store, pos, &info).is_none() => (
                    Kind::TruncatedStream,
                    format!(
                        "its {} stream stops before its end marker, but the chunk data in it is complete",
                        codec::scheme_name(scheme)
                    ),
                ),
                _ => (
                    Kind::Corrupt,
                    format!(
                        "its {} data stops early; only {} of the chunk can be recovered",
                        codec::scheme_name(scheme),
                        human_bytes(out.len() as u64)
                    ),
                ),
            });
        }
    }
    match nbt::inspect(out, sections) {
        Err(e) => Some((Kind::BadNbt, format!("its data is not valid NBT: {e}"))),
        Ok(info) => {
            if info.data_version.is_some_and(|v| v >= SECTIONS_SINCE) {
                *containers += u64::from(info.containers);
            }
            content_problem(f.store, pos, &info)
        }
    }
}

fn scan_file(f: &Arc<FileRef>, quick: bool, whole_limit: u64, cancel: &AtomicBool, sc: &mut Scratch) -> FileScan {
    let mut res = FileScan::default();
    let whole_file = |kind: Kind, detail: String| Finding { n: 0, file: f.clone(), pos: None, slot: None, kind, detail };
    macro_rules! unreadable {
        ($what:expr, $e:expr) => {{
            res.unreadable = true;
            res.findings.push(whole_file(Kind::Unreadable, format!("{}: {}", $what, $e)));
            return res;
        }};
    }

    let mut file = match File::open(&f.path) {
        Ok(file) => file,
        Err(e) => unreadable!("the file cannot be opened", e),
    };
    let mut size = match file.metadata() {
        Ok(m) => m.len(),
        Err(e) => unreadable!("the file cannot be examined", e),
    };
    let whole = !quick && size <= whole_limit;
    if whole {
        sc.whole.clear();
        if let Err(e) = file.read_to_end(&mut sc.whole) {
            unreadable!("the file cannot be read", e);
        }
        size = sc.whole.len() as u64;
    }
    if size == 0 {
        // Normal: the game creates a region file before it has a chunk to put in it.
        res.empty = true;
        return res;
    }
    if size < HEADER {
        res.findings.push(whole_file(
            Kind::NoHeader,
            format!("the file is only {size} bytes long, less than its 8192-byte header, so no chunk in it can be read"),
        ));
        return res;
    }
    let mut header = [0u8; HEADER as usize];
    if whole {
        header.copy_from_slice(&sc.whole[..HEADER as usize]);
    } else if let Err(e) = file.read_exact(&mut header) {
        unreadable!("the header cannot be read", e);
    }

    let chunk = |slot: Slot, (kind, detail): Problem| Finding {
        n: 0,
        file: f.clone(),
        pos: Some(f.chunk_pos(slot.idx as usize)),
        slot: Some(slot),
        kind,
        detail,
    };

    // The header table: where each chunk is supposed to be.
    let mut cands: Vec<Cand> = Vec::new();
    for idx in 0..SLOTS {
        let loc = be32(&header[idx * 4..]);
        if loc == 0 {
            continue;
        }
        res.chunks += 1;
        let slot = Slot { idx: idx as u16, loc, ts: be32(&header[SECTOR as usize + idx * 4..]) };
        let (off, cnt) = (u64::from(loc >> 8), u64::from(loc & 0xFF));
        if off < 2 {
            res.findings.push(chunk(slot, (Kind::HeaderOverlap, format!("its header entry points at sector {off}, inside the file header"))));
        } else if cnt == 0 {
            res.findings.push(chunk(slot, (Kind::ZeroSectors, format!("its header entry reserves 0 sectors (at sector {off})"))));
        } else if off * SECTOR >= size {
            res.findings.push(chunk(
                slot,
                (
                    Kind::OutOfBounds,
                    format!("it should start at sector {off}, but the file is only {} sectors long", size.div_ceil(SECTOR)),
                ),
            ));
        } else {
            cands.push(Cand { off, cnt, slot });
        }
    }

    // Chunks whose sectors overlap. Sorted by offset, an entry overlaps an
    // earlier one exactly if it starts before the furthest end seen so far.
    cands.sort_by_key(|c| (c.off, c.slot.idx));
    let mut shared: Vec<Option<u16>> = vec![None; cands.len()];
    let mut top: Option<usize> = None;
    for i in 0..cands.len() {
        if let Some(t) = top {
            if cands[i].off < cands[t].off + cands[t].cnt {
                shared[i].get_or_insert(cands[t].slot.idx);
                shared[t].get_or_insert(cands[i].slot.idx);
            }
        }
        if top.is_none_or(|t| cands[i].off + cands[i].cnt > cands[t].off + cands[t].cnt) {
            top = Some(i);
        }
    }

    for (c, shared) in cands.iter().zip(shared) {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let pos = f.chunk_pos(c.slot.idx as usize);
        let start = c.off * SECTOR;
        let avail = (c.cnt * SECTOR).min(size - start) as usize;
        let data: &[u8] = if whole {
            &sc.whole[start as usize..start as usize + avail]
        } else {
            sc.raw.resize(if quick { avail.min(5) } else { avail }, 0);
            if let Err(e) = file.seek(SeekFrom::Start(start)).and_then(|_| file.read_exact(&mut sc.raw)) {
                res.findings.push(chunk(c.slot, (Kind::Unreadable, format!("its sectors cannot be read: {e}"))));
                continue;
            }
            &sc.raw
        };
        let problem =
            examine(f, pos, data, avail, quick, &mut sc.ext, &mut sc.out, &mut sc.codec, &mut res.externals, &mut res.containers);
        let other = shared.map(|idx| {
            let (x, z) = f.chunk_pos(idx as usize);
            format!("chunk {x},{z}")
        });
        match (problem, other) {
            (Some((kind, detail)), Some(other)) => {
                res.findings.push(chunk(c.slot, (kind, format!("{detail} (its sectors also overlap those of {other})"))));
            }
            (Some(problem), None) => res.findings.push(chunk(c.slot, problem)),
            (None, Some(other)) => res.findings.push(chunk(
                c.slot,
                (Kind::SharedSectors, format!("its sectors overlap those of {other}; this chunk itself reads fine")),
            )),
            (None, None) => {}
        }
    }
    sc.trim();
    res
}

// ---------------------------------------------------------------------------
// The whole world
// ---------------------------------------------------------------------------

pub fn default_threads() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4)
}

/// Scan everything below `path`. `stats` is updated while it runs; setting
/// `stats.cancel` stops it.
pub fn run(path: &Path, opts: &Options, stats: &Stats) -> Result<ScanOutput> {
    let started = Instant::now();
    let world = world::discover(path)?;
    execute(&world, opts, stats, started)
}

fn execute(world: &World, opts: &Options, stats: &Stats, started: Instant) -> Result<ScanOutput> {
    let threads = opts.threads.unwrap_or_else(default_threads).clamp(1, 256);
    let bytes: u64 = world.regions.iter().map(|f| f.size).sum();
    stats.total_bytes.store(bytes, Ordering::Relaxed);
    stats.total_files.store(world.regions.len() as u64, Ordering::Relaxed);

    // Largest first, so the big files do not all end up at the tail.
    let mut order: Vec<usize> = (0..world.regions.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(world.regions[i].size));

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .stack_size(4 << 20)
        .thread_name(|i| format!("mcfsck-{i}"))
        .build()
        .map_err(|e| anyhow!("cannot start worker threads: {e}"))?;
    let scans: Vec<(usize, FileScan)> = pool.install(|| {
        order
            .par_iter()
            .with_max_len(1)
            .map_init(Scratch::default, |scratch, &i| {
                let f = &world.regions[i];
                let scan = if stats.cancel.load(Ordering::Relaxed) {
                    FileScan::default()
                } else {
                    scan_file(f, opts.quick, WHOLE_LIMIT, &stats.cancel, scratch)
                };
                let count = |sev: Severity| scan.findings.iter().filter(|x| x.severity() == sev).count() as u64;
                stats.errors.fetch_add(count(Severity::Error), Ordering::Relaxed);
                stats.warnings.fetch_add(count(Severity::Warning), Ordering::Relaxed);
                stats.chunks.fetch_add(scan.chunks, Ordering::Relaxed);
                stats.bytes_done.fetch_add(f.size, Ordering::Relaxed);
                stats.files_done.fetch_add(1, Ordering::Relaxed);
                (i, scan)
            })
            .collect()
    });
    if stats.cancel.load(Ordering::Relaxed) {
        bail!("the scan was cancelled");
    }

    // Oversized-chunk files that no region file refers to.
    let mut referenced: HashSet<(&Path, i32, i32)> = HashSet::new();
    let mut unreadable: HashSet<(&Path, i32, i32)> = HashSet::new();
    let mut present: HashSet<(&Path, i32, i32)> = HashSet::new();
    let mut findings: Vec<Finding> = Vec::new();
    let (mut chunks, mut empty_files, mut containers) = (0, 0, 0);
    for (i, scan) in scans {
        let f = &world.regions[i];
        present.insert((f.dir(), f.rx, f.rz));
        if scan.unreadable {
            unreadable.insert((f.dir(), f.rx, f.rz));
        }
        referenced.extend(scan.externals.iter().map(|&(x, z)| (f.dir(), x, z)));
        chunks += scan.chunks;
        empty_files += u64::from(scan.empty);
        containers += scan.containers;
        findings.extend(scan.findings);
    }
    for mcc in &world.mccs {
        let f = &mcc.file;
        if unreadable.contains(&(f.dir(), f.rx, f.rz)) || referenced.contains(&(f.dir(), mcc.cx, mcc.cz)) {
            continue;
        }
        let owner = world::region_name(f.rx, f.rz);
        let detail = if present.contains(&(f.dir(), f.rx, f.rz)) {
            format!("{owner} does not store chunk {},{} in a separate file", mcc.cx, mcc.cz)
        } else {
            format!("its region file {owner} does not exist")
        };
        stats.warnings.fetch_add(1, Ordering::Relaxed);
        findings.push(Finding { n: 0, file: f.clone(), pos: Some((mcc.cx, mcc.cz)), slot: None, kind: Kind::OrphanMcc, detail });
    }

    findings.sort_by_cached_key(|x| {
        (x.file.dim.clone(), x.file.store, x.file.rz, x.file.rx, x.slot.map_or(0, |s| s.idx), x.file.rel.clone())
    });
    for (i, f) in findings.iter_mut().enumerate() {
        f.n = i as u64 + 1;
    }

    let mut files = [0u64; 4];
    let mut dims: Vec<String> = Vec::new();
    for f in &world.regions {
        files[Store::ALL.iter().position(|s| *s == f.store).unwrap_or(3)] += 1;
        if !dims.contains(&f.dim) {
            dims.push(f.dim.clone());
        }
    }
    dims.sort();
    Ok(ScanOutput {
        root: world.root.clone(),
        findings,
        files,
        empty_files,
        containers,
        mcc_files: world.mccs.len() as u64,
        bytes,
        chunks,
        dims,
        elapsed: started.elapsed(),
        quick: opts.quick,
        threads,
    })
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::codec::testing::{gzip, lz4, zlib};
    use crate::testutil::{entities, good_terrain, poi, terrain, Nbt, Region, TempDir};
    use std::fs;

    /// A world with one of everything. Returns what a full scan must report:
    /// `(file, header slot, kind)`.
    pub fn sample_world(w: &Path) -> Vec<(&'static str, Option<u16>, Kind)> {
        let mut r = Region::new();
        r.add(0, &good_terrain(0, 0));
        r.set_loc(1, 1, 1);
        r.set_loc(2, 5, 0);
        r.set_loc(3, 5000, 1);
        // 4: reserved, all zero.
        let off = (r.buf.len() / 4096) as u32;
        r.buf.resize(r.buf.len() + 4096, 0);
        r.set_loc(4, off, 1);
        // 5: negative length.
        let (off, _) = r.add_raw(5, 2, b"abc");
        r.buf[off as usize * 4096..off as usize * 4096 + 4].copy_from_slice(&[0xFF; 4]);
        // 6: two sectors of data, one in the header.
        let (off, cnt) = r.add_raw(6, 2, &vec![7u8; 6000]);
        assert_eq!(cnt, 2);
        r.set_loc(6, off, 1);
        r.add_raw(7, 9, b"xxxx");
        r.add_raw(8, 0x82, &[]);
        // 9: a flipped bit in the compressed data.
        let mut z = zlib(&good_terrain(9, 0));
        let mid = z.len() / 2;
        z[mid] ^= 0x10;
        r.add_raw(9, 2, &z);
        r.add(10, &[10, 0, 0, 3, 0, 1, b'a']);
        r.add(11, &poi());
        r.add(12, &good_terrain(5, 5));
        r.add(13, &terrain(13, 0, 4000, &[(0, Some(20), Some(256))]));
        let z = zlib(&good_terrain(14, 0));
        r.add_raw(14, 2, &z[..z.len() - 4]);
        let (off, cnt) = r.add(15, &good_terrain(15, 0));
        r.set_loc(16, off, cnt);
        r.add_raw(17, 127, b"abc");
        r.add_raw(18, 0x82, &[]);
        // 20-24: healthy chunks in every flavour.
        let mut old = Nbt::root();
        old.int("DataVersion", 1343).compound("Level").int("xPos", 20).int("zPos", 0).end().end();
        r.add(20, &old.0);
        r.add_raw(21, 1, &gzip(&good_terrain(21, 0)));
        r.add_raw(22, 3, &good_terrain(22, 0));
        r.add_raw(23, 4, &lz4(&good_terrain(23, 0), 1 << 16));
        r.add(24, &terrain(24, 0, 2800, &[(0, Some(20), Some(256))]));
        // 19: starts three bytes before the end of the file.
        let off = (r.buf.len() / 4096) as u32;
        r.buf.extend_from_slice(&[0, 0, 0]);
        r.set_loc(19, off, 1);
        r.write(&w.join("region/r.0.0.mca"));
        fs::write(w.join("region/c.18.0.mcc"), zlib(&good_terrain(18, 0))).unwrap();
        fs::write(w.join("region/c.5.0.mcc"), b"left over").unwrap();
        fs::write(w.join("region/c.100.100.mcc"), b"left over").unwrap();
        fs::write(w.join("region/r.1.0.mca"), b"").unwrap();
        fs::write(w.join("region/r.2.0.mca"), [0u8; 100]).unwrap();

        let mut e = Region::new();
        e.add(0, &entities(0, 0));
        e.add(1, &entities(9, 9));
        e.add(2, &poi());
        // Entity data for chunks whose terrain is damaged.
        e.add(9, &entities(9, 0));
        e.add(13, &entities(13, 0));
        e.write(&w.join("entities/r.0.0.mca"));

        let mut p = Region::new();
        p.add(0, &poi());
        p.add_raw(1, 2, b"this is not zlib");
        p.add(9, &poi());
        p.write(&w.join("poi/r.0.0.mca"));

        let mut n = Region::new();
        n.add(0, &good_terrain(-32, -32));
        n.add(1023, &good_terrain(-1, -1));
        n.write(&w.join("DIM-1/region/r.-1.-1.mca"));

        vec![
            ("entities/r.0.0.mca", Some(1), Kind::WrongPosition),
            ("entities/r.0.0.mca", Some(2), Kind::NoPosition),
            ("poi/r.0.0.mca", Some(1), Kind::Corrupt),
            ("region/r.0.0.mca", Some(1), Kind::HeaderOverlap),
            ("region/r.0.0.mca", Some(2), Kind::ZeroSectors),
            ("region/r.0.0.mca", Some(3), Kind::OutOfBounds),
            ("region/r.0.0.mca", Some(4), Kind::MissingStream),
            ("region/r.0.0.mca", Some(5), Kind::NegativeLength),
            ("region/r.0.0.mca", Some(6), Kind::Truncated),
            ("region/r.0.0.mca", Some(7), Kind::BadCompression),
            ("region/r.0.0.mca", Some(8), Kind::ExternalMissing),
            ("region/r.0.0.mca", Some(9), Kind::Corrupt),
            ("region/r.0.0.mca", Some(10), Kind::BadNbt),
            ("region/r.0.0.mca", Some(11), Kind::NoPosition),
            ("region/r.0.0.mca", Some(12), Kind::WrongPosition),
            ("region/r.0.0.mca", Some(13), Kind::BadSection),
            ("region/r.0.0.mca", Some(14), Kind::TruncatedStream),
            ("region/r.0.0.mca", Some(15), Kind::SharedSectors),
            ("region/r.0.0.mca", Some(16), Kind::WrongPosition),
            ("region/r.0.0.mca", Some(17), Kind::Unverified),
            ("region/r.0.0.mca", Some(19), Kind::NoChunkHeader),
            ("region/c.100.100.mcc", None, Kind::OrphanMcc),
            ("region/c.5.0.mcc", None, Kind::OrphanMcc),
            ("region/r.2.0.mca", None, Kind::NoHeader),
        ]
    }

    pub fn scan(path: &Path, quick: bool, threads: usize) -> ScanOutput {
        run(path, &Options { quick, threads: Some(threads) }, &Stats::default()).unwrap()
    }

    fn got(out: &ScanOutput) -> Vec<(String, Option<u16>, Kind)> {
        let mut v: Vec<_> = out.findings.iter().map(|f| (f.file.rel.clone(), f.slot.map(|s| s.idx), f.kind)).collect();
        v.sort();
        v
    }

    fn sorted(want: Vec<(&'static str, Option<u16>, Kind)>) -> Vec<(String, Option<u16>, Kind)> {
        let mut v: Vec<_> = want.into_iter().map(|(a, b, c)| (a.to_string(), b, c)).collect();
        v.sort();
        v
    }

    #[test]
    fn finds_exactly_the_damage() {
        let tmp = TempDir::new("scan");
        let want = sorted(sample_world(tmp.path()));
        for threads in [1, 8] {
            let out = scan(tmp.path(), false, threads);
            assert_eq!(got(&out), want, "{threads} thread(s)");
            assert_eq!(out.files, [4, 1, 1, 0]);
            // The 0-byte r.1.0.mca is counted, not reported.
            assert_eq!(out.empty_files, 1);
            assert!(out.containers >= 40, "{}", out.containers);
            assert_eq!(out.mcc_files, 3);
            assert_eq!(out.chunks, 25 + 5 + 3 + 2);
            assert_eq!(out.count(Severity::Error) + out.count(Severity::Warning), want.len() as u64);
            assert_eq!(out.dims, ["minecraft:overworld", "minecraft:the_nether"]);
            // Numbered in order, and every line renders.
            for (i, f) in out.findings.iter().enumerate() {
                assert_eq!(f.n, i as u64 + 1);
                assert!(!f.line().is_empty() && !f.kind.explain().is_empty());
            }
        }
    }

    #[test]
    fn details_name_the_cause() {
        let tmp = TempDir::new("details");
        sample_world(tmp.path());
        let out = scan(tmp.path(), false, 4);
        let find = |rel: &str, idx: u16| out.findings.iter().find(|f| f.file.rel == rel && f.slot.map(|s| s.idx) == Some(idx)).unwrap();
        let wrong = find("region/r.0.0.mca", 12);
        assert_eq!(wrong.pos, Some((12, 0)));
        assert!(wrong.detail.contains("chunk 5,5"), "{}", wrong.detail);
        assert_eq!(wrong.target(), "chunk 12,0 in region/r.0.0.mca");
        assert!(find("region/r.0.0.mca", 13).detail.contains("section Y=0"));
        assert!(find("region/r.0.0.mca", 15).detail.contains("chunk 16,0"));
        let dup = find("region/r.0.0.mca", 16);
        assert!(dup.detail.contains("chunk 15,0") && dup.detail.contains("overlap"), "{}", dup.detail);
        assert!(find("region/r.0.0.mca", 8).detail.contains("c.8.0.mcc"));
        assert!(find("region/r.0.0.mca", 6).detail.contains("6000 bytes"));
        assert_eq!(find("region/r.0.0.mca", 17).fix(), Fix::None);
        assert_eq!(find("region/r.0.0.mca", 9).fix(), Fix::DeleteChunk);
        let orphan = out.findings.iter().find(|f| f.file.rel == "region/c.5.0.mcc").unwrap();
        assert_eq!((orphan.fix(), orphan.pos, orphan.severity()), (Fix::DeleteFile, Some((5, 0)), Severity::Warning));
        assert!(orphan.detail.contains("r.0.0.mca does not store"));
    }

    #[test]
    fn quick_scan_checks_headers_only() {
        let tmp = TempDir::new("quick");
        let content =
            [Kind::Corrupt, Kind::BadNbt, Kind::NoPosition, Kind::WrongPosition, Kind::BadSection, Kind::TruncatedStream];
        let mut want: Vec<_> = sample_world(tmp.path()).into_iter().filter(|w| !content.contains(&w.2)).collect();
        // The duplicate header entry is still visible, from both sides.
        want.push(("region/r.0.0.mca", Some(16), Kind::SharedSectors));
        let out = scan(tmp.path(), true, 4);
        assert_eq!(got(&out), sorted(want));
        assert!(out.quick);
        assert_eq!(out.containers, 0);
    }

    #[test]
    fn large_files_are_read_chunk_by_chunk() {
        // Same answers whether a file is read in one go or seeked through.
        let tmp = TempDir::new("seek");
        let want: Vec<_> = sorted(sample_world(tmp.path())).into_iter().filter(|w| w.2 != Kind::OrphanMcc).collect();
        let world = world::discover(tmp.path()).unwrap();
        let mut sc = Scratch::default();
        let (mut found, mut externals) = (Vec::new(), 0);
        for f in &world.regions {
            let scan = scan_file(f, false, 0, &AtomicBool::new(false), &mut sc);
            externals += scan.externals.len();
            found.extend(scan.findings.iter().map(|x| (x.file.rel.clone(), x.slot.map(|s| s.idx), x.kind)));
        }
        found.sort();
        assert_eq!(found, want);
        assert_eq!(externals, 2);
    }

    #[test]
    fn clean_world_and_cancel() {
        let tmp = TempDir::new("clean");
        let mut r = Region::new();
        for idx in 0..SLOTS {
            r.add(idx, &good_terrain(idx as i32 % 32 + 64, idx as i32 / 32 - 32));
        }
        r.write(&tmp.path().join("region/r.2.-1.mca"));
        let out = scan(tmp.path(), false, 4);
        assert!(out.findings.is_empty(), "{:?}", out.findings.first());
        assert_eq!(out.chunks, 1024);
        assert_eq!(out.verdict(), "No problems found.");
        assert_eq!(out.coverage_lines().len(), 4);

        let stats = Stats::default();
        stats.cancel.store(true, Ordering::Relaxed);
        assert!(run(tmp.path(), &Options::default(), &stats).is_err());
    }

    #[test]
    fn unpadded_and_grown_files() {
        let tmp = TempDir::new("pad");
        // A last chunk whose final sector is not padded is still complete.
        let mut r = Region::new();
        r.add(0, &good_terrain(0, 0));
        let (off, cnt) = r.add(1, &good_terrain(1, 0));
        let used = 5 + zlib(&good_terrain(1, 0)).len();
        r.buf.truncate(off as usize * 4096 + used);
        assert_eq!(cnt, 1);
        r.write(&tmp.path().join("region/r.0.0.mca"));
        assert!(scan(tmp.path(), false, 2).findings.is_empty());
        // One byte less and it is not.
        r.buf.pop();
        r.write(&tmp.path().join("region/r.0.0.mca"));
        let out = scan(tmp.path(), false, 2);
        assert_eq!(got(&out), [("region/r.0.0.mca".to_string(), Some(1), Kind::Truncated)]);
    }

    #[test]
    fn three_way_overlaps_are_all_reported() {
        let tmp = TempDir::new("overlap");
        let mut r = Region::new();
        let (off, _) = r.add_raw(0, 2, &zlib(&good_terrain(0, 0)));
        r.buf.resize(r.buf.len() + 3 * 4096, 0);
        // 0 spans sectors off..off+4; 1 and 2 sit inside it without touching each other.
        r.set_loc(0, off, 4);
        r.set_loc(1, off + 1, 1);
        r.set_loc(2, off + 3, 1);
        r.add(3, &good_terrain(3, 0));
        r.write(&tmp.path().join("region/r.0.0.mca"));
        let out = scan(tmp.path(), true, 1);
        assert_eq!(
            got(&out),
            [0u16, 1, 2].map(|i| ("region/r.0.0.mca".to_string(), Some(i), if i == 0 { Kind::SharedSectors } else { Kind::MissingStream }))
        );
        assert!(out.findings[1].detail.contains("overlap those of chunk 0,0"));
    }
}
