//! World folder discovery and the session lock. Understands the classic
//! layout (`region/`, `DIM-1/region/`), the 26.1+ layout
//! (`dimensions/<namespace>/<name>/region/`), and being pointed straight at a
//! dimension folder, a region folder or a single region file.

use anyhow::{bail, Result};
use serde::Serialize;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Which of a dimension's chunk stores a region file belongs to.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Store {
    /// Terrain: blocks, biomes, block entities.
    Region,
    /// Mobs, item frames, minecarts, ...
    Entities,
    /// Points of interest: beds, job sites, portals, ...
    Poi,
    /// Region files in a folder with another name.
    Other,
}

impl Store {
    pub const ALL: [Store; 4] = [Store::Region, Store::Entities, Store::Poi, Store::Other];

    pub fn name(self) -> &'static str {
        match self {
            Store::Region => "region",
            Store::Entities => "entities",
            Store::Poi => "poi",
            Store::Other => "other",
        }
    }

    fn of_dir(name: &str) -> Option<Store> {
        match name {
            "region" => Some(Store::Region),
            "entities" => Some(Store::Entities),
            "poi" => Some(Store::Poi),
            _ => None,
        }
    }
}

/// A region file (`r.X.Z.mca`) or an oversized-chunk file (`c.X.Z.mcc`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRef {
    pub path: PathBuf,
    /// Path relative to the scanned folder, always with forward slashes.
    pub rel: String,
    pub store: Store,
    /// For example `minecraft:the_nether`.
    pub dim: String,
    /// Region coordinates (for a `.mcc` file, those of the region it belongs to).
    pub rx: i32,
    pub rz: i32,
    /// Size when it was discovered.
    pub size: u64,
}

impl FileRef {
    pub fn dir(&self) -> &Path {
        self.path.parent().unwrap_or(Path::new(""))
    }

    /// The dimension without the `minecraft:` prefix.
    pub fn dim_short(&self) -> &str {
        self.dim.strip_prefix("minecraft:").unwrap_or(&self.dim)
    }

    /// Absolute chunk coordinates of header slot `idx`.
    pub fn chunk_pos(&self, idx: usize) -> (i32, i32) {
        (self.rx * 32 + (idx % 32) as i32, self.rz * 32 + (idx / 32) as i32)
    }
}

/// An oversized chunk stored next to its region file.
#[derive(Debug, Clone)]
pub struct Mcc {
    pub file: Arc<FileRef>,
    pub cx: i32,
    pub cz: i32,
}

#[derive(Debug, Default)]
pub struct World {
    /// The folder that was scanned; backups go below it.
    pub root: PathBuf,
    pub regions: Vec<Arc<FileRef>>,
    pub mccs: Vec<Mcc>,
}

/// Header slot of a chunk inside its region file.
pub fn chunk_index(cx: i32, cz: i32) -> usize {
    (cx.rem_euclid(32) + cz.rem_euclid(32) * 32) as usize
}

pub fn mcc_name(cx: i32, cz: i32) -> String {
    format!("c.{cx}.{cz}.mcc")
}

pub fn region_name(rx: i32, rz: i32) -> String {
    format!("r.{rx}.{rz}.mca")
}

/// The game only ever opens names it builds itself, so only canonical
/// numbers count: no `+`, no leading zeros, no `-0`.
fn coord(s: &str, limit: i32) -> Option<i32> {
    let n: i32 = s.parse().ok()?;
    (n.to_string() == s && n.unsigned_abs() <= limit.unsigned_abs()).then_some(n)
}

fn parse_name(name: &str, prefix: &str, ext: &str, limit: i32) -> Option<(i32, i32)> {
    let mut parts = name.split('.');
    let ok = parts.next()? == prefix;
    let x = coord(parts.next()?, limit)?;
    let z = coord(parts.next()?, limit)?;
    (ok && parts.next()? == ext && parts.next().is_none()).then_some((x, z))
}

/// `r.X.Z.mca` -> (X, Z)
pub fn region_coords(name: &str) -> Option<(i32, i32)> {
    // Far beyond the world border, and chunk coordinates still fit an i32.
    parse_name(name, "r", "mca", 1 << 22)
}

/// `c.X.Z.mcc` -> (X, Z), in chunks.
pub fn mcc_coords(name: &str) -> Option<(i32, i32)> {
    parse_name(name, "c", "mcc", 1 << 27)
}

/// The dimension a store folder belongs to, from the folders leading to it
/// (the last one being the store folder itself).
fn dimension_of(dirs: &[String]) -> String {
    let n = dirs.len();
    if n >= 4 && dirs[n - 4] == "dimensions" {
        return format!("{}:{}", dirs[n - 3], dirs[n - 2]);
    }
    match if n >= 2 { dirs[n - 2].as_str() } else { "" } {
        "DIM-1" => "minecraft:the_nether".to_string(),
        "DIM1" => "minecraft:the_end".to_string(),
        _ => "minecraft:overworld".to_string(),
    }
}

const SKIP_DIRS: [&str; 12] = [
    "mcfsck-backups",
    "mcscan-backups",
    "datapacks",
    "playerdata",
    "players",
    "advancements",
    "stats",
    "generated",
    "data",
    "logs",
    "crash-reports",
    "resources",
];

/// Symbolic links are followed (server worlds are often linked onto other
/// disks), so the depth is what stops a loop.
const MAX_DEPTH: usize = 8;

struct Walk {
    out: World,
    /// Folders above the scanned one, to name the dimension when the scan
    /// starts inside a dimension.
    above: Vec<String>,
}

impl Walk {
    fn dir(&mut self, dir: &Path, rel: &[String]) {
        let Ok(entries) = fs::read_dir(dir) else { return };
        let own_name = rel.last().or(self.above.last()).cloned().unwrap_or_default();
        let store = Store::of_dir(&own_name).or(rel.is_empty().then_some(Store::Other));
        let mut dim: Option<String> = None;
        let mut subdirs = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Ok(meta) = fs::metadata(entry.path()) else { continue };
            if meta.is_dir() {
                if !name.starts_with('.') && !SKIP_DIRS.contains(&name) && rel.len() < MAX_DEPTH {
                    subdirs.push(name.to_string());
                }
                continue;
            }
            let (Some(store), true) = (store, meta.is_file()) else { continue };
            let region = region_coords(name);
            let mcc = if region.is_none() { mcc_coords(name) } else { None };
            if region.is_none() && mcc.is_none() {
                continue;
            }
            let dim = dim.get_or_insert_with(|| {
                let mut dirs = self.above.clone();
                dirs.extend(rel.iter().cloned());
                dimension_of(&dirs)
            });
            let child_rel = rel.iter().map(String::as_str).chain([name]).collect::<Vec<_>>().join("/");
            let (rx, rz) = region.unwrap_or_else(|| mcc.map(|(cx, cz)| (cx >> 5, cz >> 5)).unwrap_or_default());
            let file =
                Arc::new(FileRef { path: entry.path(), rel: child_rel, store, dim: dim.clone(), rx, rz, size: meta.len() });
            match mcc {
                Some((cx, cz)) => self.out.mccs.push(Mcc { file, cx, cz }),
                None => self.out.regions.push(file),
            }
        }
        // A store folder has no store folders inside it.
        if store.is_some() && !rel.is_empty() {
            return;
        }
        subdirs.sort();
        for name in subdirs {
            let mut child = rel.to_vec();
            child.push(name.clone());
            self.dir(&dir.join(&name), &child);
        }
    }
}

/// Names of the last few folders of `path`, outermost first.
fn tail_dirs(path: &Path, n: usize) -> Vec<String> {
    let abs = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut names: Vec<String> = abs
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => s.to_str().map(str::to_string),
            _ => None,
        })
        .collect();
    let cut = names.len().saturating_sub(n);
    names.drain(..cut);
    names
}

/// Find every region file below `path`: a world, a dimension folder, a
/// region folder, or one region file.
pub fn discover(path: &Path) -> Result<World> {
    let meta = match fs::metadata(path) {
        Ok(m) => m,
        Err(e) => bail!("cannot open {}: {e}", path.display()),
    };
    if meta.is_file() {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        let Some((rx, rz)) = region_coords(name) else {
            bail!("{} is not a region file (r.X.Z.mca) or a folder", path.display());
        };
        let dir = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        };
        let dirs = tail_dirs(&dir, 4);
        let store = dirs.last().and_then(|d| Store::of_dir(d)).unwrap_or(Store::Other);
        let file = FileRef {
            path: path.to_path_buf(),
            rel: name.to_string(),
            store,
            dim: dimension_of(&dirs),
            rx,
            rz,
            size: meta.len(),
        };
        return Ok(World { root: dir, regions: vec![Arc::new(file)], mccs: Vec::new() });
    }
    let mut walk = Walk { out: World { root: path.to_path_buf(), ..World::default() }, above: tail_dirs(path, 4) };
    walk.dir(path, &[]);
    let mut world = walk.out;
    if world.regions.is_empty() && world.mccs.is_empty() {
        bail!("{} does not look like a Minecraft world (no region files found)", path.display());
    }
    world.regions.sort_by(|a, b| a.rel.cmp(&b.rel));
    world.mccs.sort_by(|a, b| a.file.rel.cmp(&b.file.rel));
    Ok(world)
}

// ---------------------------------------------------------------------------
// Session lock
// ---------------------------------------------------------------------------

/// Holds the world's `session.lock` while files are changed, like the game
/// does while a world is open.
#[derive(Debug)]
pub struct SessionLock {
    _file: Option<File>,
}

#[derive(Debug)]
pub enum LockResult {
    /// The world is not in use (and now locked), or it has no lock file.
    Acquired(SessionLock),
    /// Another process, most likely the game or a server, has the world open.
    InUse(PathBuf),
    /// The lock could not be checked.
    Unknown(PathBuf, std::io::Error),
}

/// Find `session.lock` in the folder itself or one of its parents (a region
/// folder is up to four levels below the world folder).
fn find_session_lock(start: &Path) -> Option<PathBuf> {
    let start = fs::canonicalize(start).ok()?;
    start.ancestors().take(5).map(|dir| dir.join("session.lock")).find(|lock| lock.is_file())
}

/// Try to take the session lock of the world that `path` belongs to.
pub fn lock_world(path: &Path) -> LockResult {
    let Some(lock_path) = find_session_lock(path) else {
        return LockResult::Acquired(SessionLock { _file: None });
    };
    let file = match File::options().read(true).write(true).open(&lock_path) {
        Ok(file) => file,
        Err(e) => return LockResult::Unknown(lock_path, e),
    };
    match try_lock(&file) {
        Ok(true) => LockResult::Acquired(SessionLock { _file: Some(file) }),
        Ok(false) => LockResult::InUse(lock_path),
        Err(e) => LockResult::Unknown(lock_path, e),
    }
}

/// Whether a game or server has the world open right now.
pub fn in_use(path: &Path) -> bool {
    matches!(lock_world(path), LockResult::InUse(_))
}

/// Java locks with `fcntl` on Unix, which `flock` (what the standard library
/// uses) does not see, so take the same kind of lock here.
#[cfg(unix)]
fn try_lock(file: &File) -> std::io::Result<bool> {
    use rustix::fs::{fcntl_lock, FlockOperation};
    use rustix::io::Errno;
    match fcntl_lock(file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(true),
        Err(Errno::AGAIN) | Err(Errno::ACCESS) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// On Windows both Java and the standard library use `LockFileEx`.
#[cfg(not(unix))]
fn try_lock(file: &File) -> std::io::Result<bool> {
    match file.try_lock() {
        Ok(()) => Ok(true),
        Err(std::fs::TryLockError::WouldBlock) => Ok(false),
        Err(std::fs::TryLockError::Error(e)) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn touch(root: &Path, rel: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, b"").unwrap();
    }

    #[test]
    fn names() {
        assert_eq!(region_coords("r.0.0.mca"), Some((0, 0)));
        assert_eq!(region_coords("r.-12.7.mca"), Some((-12, 7)));
        for bad in ["r.0.0.mcr", "r.0.mca", "r.0.0.0.mca", "r.+1.0.mca", "r.01.0.mca", "r.-0.0.mca", "x.0.0.mca", "r.0.0.mca.bak", "r.99999999.0.mca", "r.a.b.mca"] {
            assert_eq!(region_coords(bad), None, "{bad}");
        }
        assert_eq!(mcc_coords("c.-33.64.mcc"), Some((-33, 64)));
        assert_eq!(mcc_coords("r.0.0.mca"), None);
        assert_eq!(chunk_index(-1, -1), 31 + 31 * 32);
        assert_eq!(chunk_index(33, 64), 1);
        assert_eq!((-33i32 >> 5, 64i32 >> 5), (-2, 2));
    }

    #[test]
    fn layouts() {
        let tmp = TempDir::new("layouts");
        let w = tmp.path().join("world");
        for rel in [
            "region/r.0.0.mca",
            "region/c.1.2.mcc",
            "region/r.0.0.mca.tmp",
            "entities/r.0.0.mca",
            "poi/r.-1.0.mca",
            "DIM-1/region/r.0.0.mca",
            "DIM1/entities/r.1.1.mca",
            "dimensions/minecraft/the_nether/region/r.2.2.mca",
            "dimensions/mypack/mine/poi/r.3.3.mca",
            "mcfsck-backups/20260101-000000/region/r.0.0.mca",
            "data/region/r.9.9.mca",
            "stuff/r.5.5.mca",
            "level.dat",
        ] {
            touch(&w, rel);
        }
        let world = discover(&w).unwrap();
        let found: Vec<(String, Store, String)> =
            world.regions.iter().map(|f| (f.rel.clone(), f.store, f.dim.clone())).collect();
        let want = [
            ("DIM-1/region/r.0.0.mca", Store::Region, "minecraft:the_nether"),
            ("DIM1/entities/r.1.1.mca", Store::Entities, "minecraft:the_end"),
            ("dimensions/minecraft/the_nether/region/r.2.2.mca", Store::Region, "minecraft:the_nether"),
            ("dimensions/mypack/mine/poi/r.3.3.mca", Store::Poi, "mypack:mine"),
            ("entities/r.0.0.mca", Store::Entities, "minecraft:overworld"),
            ("poi/r.-1.0.mca", Store::Poi, "minecraft:overworld"),
            ("region/r.0.0.mca", Store::Region, "minecraft:overworld"),
        ];
        assert_eq!(found, want.map(|(a, b, c)| (a.to_string(), b, c.to_string())));
        assert_eq!(world.mccs.len(), 1);
        assert_eq!((world.mccs[0].cx, world.mccs[0].cz, world.mccs[0].file.rx), (1, 2, 0));
        assert_eq!(world.regions[0].dim_short(), "the_nether");
        assert_eq!(world.regions[5].chunk_pos(33), (-31, 1));

        // Pointed at a dimension, a store folder, or one file.
        let nether = discover(&w.join("DIM-1")).unwrap();
        assert_eq!((nether.regions[0].rel.as_str(), nether.regions[0].dim.as_str()), ("region/r.0.0.mca", "minecraft:the_nether"));
        let dir = discover(&w.join("dimensions/mypack/mine/poi")).unwrap();
        assert_eq!((dir.regions[0].rel.as_str(), dir.regions[0].store, dir.regions[0].dim.as_str()), ("r.3.3.mca", Store::Poi, "mypack:mine"));
        let loose = discover(&w.join("stuff")).unwrap();
        assert_eq!((loose.regions[0].store, loose.regions.len()), (Store::Other, 1));
        let one = discover(&w.join("DIM1/entities/r.1.1.mca")).unwrap();
        assert_eq!((one.regions[0].store, one.regions[0].dim.as_str(), one.root.clone()), (Store::Entities, "minecraft:the_end", w.join("DIM1/entities")));

        assert!(discover(&w.join("level.dat")).is_err());
        assert!(discover(&w.join("nope")).is_err());
        assert!(discover(&w.join("data/region")).is_ok());
        let empty = tmp.path().join("empty");
        fs::create_dir_all(&empty).unwrap();
        assert!(discover(&empty).is_err());
    }

    #[test]
    fn session_lock() {
        let tmp = TempDir::new("lock");
        let w = tmp.path();
        fs::create_dir_all(w.join("dimensions/minecraft/overworld/region")).unwrap();
        assert!(matches!(lock_world(w), LockResult::Acquired(SessionLock { _file: None })));
        fs::write(w.join("session.lock"), "\u{2603}").unwrap();
        let deep = w.join("dimensions/minecraft/overworld/region");
        let lock = lock_world(&deep);
        assert!(matches!(lock, LockResult::Acquired(SessionLock { _file: Some(_) })));
        assert!(!in_use(w) || cfg!(not(unix)));
    }
}
