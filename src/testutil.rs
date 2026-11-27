//! Builders for the tests: NBT, region files and throwaway folders.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

pub struct Nbt(pub Vec<u8>);

impl Nbt {
    /// An open, unnamed root compound. Close it with [`Nbt::end`].
    pub fn root() -> Nbt {
        Nbt(vec![10, 0, 0])
    }

    fn head(&mut self, tag: u8, name: &str) -> &mut Self {
        self.0.push(tag);
        self.0.extend_from_slice(&(name.len() as u16).to_be_bytes());
        self.0.extend_from_slice(name.as_bytes());
        self
    }

    pub fn raw(&mut self, bytes: &[u8]) -> &mut Self {
        self.0.extend_from_slice(bytes);
        self
    }

    pub fn byte(&mut self, name: &str, v: i8) -> &mut Self {
        self.head(1, name).raw(&[v as u8])
    }

    pub fn int(&mut self, name: &str, v: i32) -> &mut Self {
        self.head(3, name).raw(&v.to_be_bytes())
    }

    pub fn string(&mut self, name: &str, v: &str) -> &mut Self {
        self.head(8, name).raw(&(v.len() as u16).to_be_bytes()).raw(v.as_bytes())
    }

    pub fn int_array(&mut self, name: &str, v: &[i32]) -> &mut Self {
        self.head(11, name).raw(&(v.len() as i32).to_be_bytes());
        for x in v {
            self.raw(&x.to_be_bytes());
        }
        self
    }

    /// `n` longs with some non-zero content.
    pub fn long_array(&mut self, name: &str, n: u32) -> &mut Self {
        self.head(12, name).raw(&(n as i32).to_be_bytes());
        for i in 0..n {
            self.raw(&(u64::from(i) * 0x0101_0101_0101).to_be_bytes());
        }
        self
    }

    /// Header of a list; the items follow.
    pub fn list(&mut self, name: &str, elem: u8, n: u32) -> &mut Self {
        self.head(9, name).raw(&[elem]).raw(&(n as i32).to_be_bytes())
    }

    pub fn compound(&mut self, name: &str) -> &mut Self {
        self.head(10, name)
    }

    /// Close the innermost compound.
    pub fn end(&mut self) -> &mut Self {
        self.raw(&[0])
    }
}

/// A terrain chunk in the 1.18+ layout. Each section is
/// `(Y, palette entries, longs of data)`; `None` leaves that part out.
pub fn terrain(x: i32, z: i32, version: i32, sections: &[(i8, Option<u32>, Option<u32>)]) -> Vec<u8> {
    let mut n = Nbt::root();
    n.int("DataVersion", version).int("xPos", x).int("yPos", -4).int("zPos", z).string("Status", "minecraft:full");
    n.list("sections", 10, sections.len() as u32);
    for &(y, palette, data) in sections {
        n.byte("Y", y).compound("block_states");
        if let Some(p) = palette {
            n.list("palette", 10, p);
            for i in 0..p {
                n.string("Name", &format!("minecraft:block_{i}")).end();
            }
        }
        if let Some(d) = data {
            n.long_array("data", d);
        }
        n.end();
        n.compound("biomes").list("palette", 8, 1).raw(&[0, 6]).raw(b"plains").end();
        n.end();
    }
    n.end();
    n.0
}

/// A healthy terrain chunk with a few sections.
pub fn good_terrain(x: i32, z: i32) -> Vec<u8> {
    terrain(x, z, 4000, &[(-1, Some(1), None), (0, Some(20), Some(342)), (1, Some(2), Some(256))])
}

pub fn entities(x: i32, z: i32) -> Vec<u8> {
    let mut n = Nbt::root();
    n.int("DataVersion", 4000).int_array("Position", &[x, z]).list("Entities", 10, 0).end();
    n.0
}

pub fn poi() -> Vec<u8> {
    let mut n = Nbt::root();
    n.int("DataVersion", 4000).compound("Sections").end().end();
    n.0
}

/// A region file under construction.
pub struct Region {
    pub buf: Vec<u8>,
}

impl Region {
    pub fn new() -> Region {
        Region { buf: vec![0; 8192] }
    }

    pub fn set_loc(&mut self, idx: usize, off: u32, cnt: u32) {
        self.buf[idx * 4..idx * 4 + 4].copy_from_slice(&((off << 8) | cnt).to_be_bytes());
        self.buf[4096 + idx * 4..4096 + idx * 4 + 4].copy_from_slice(&1_700_000_000u32.to_be_bytes());
    }

    /// Append a chunk record (`length`, `scheme`, payload) in fresh sectors.
    pub fn add_raw(&mut self, idx: usize, scheme: u8, payload: &[u8]) -> (u32, u32) {
        let off = (self.buf.len() / 4096) as u32;
        self.buf.extend_from_slice(&(payload.len() as u32 + 1).to_be_bytes());
        self.buf.push(scheme);
        self.buf.extend_from_slice(payload);
        let cnt = (payload.len() + 5).div_ceil(4096) as u32;
        self.buf.resize((off + cnt) as usize * 4096, 0);
        self.set_loc(idx, off, cnt);
        (off, cnt)
    }

    /// Append zlib-compressed NBT.
    pub fn add(&mut self, idx: usize, nbt: &[u8]) -> (u32, u32) {
        self.add_raw(idx, 2, &crate::codec::testing::zlib(nbt))
    }

    pub fn write(&self, path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, &self.buf).unwrap();
    }
}

/// A folder that is removed again when the value goes away.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> TempDir {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("mcfsck-test-{}-{tag}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
