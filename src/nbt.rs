//! A validating NBT walker. It never builds a tree: it checks that the bytes
//! are well-formed NBT the game could read, and picks out the few values the
//! chunk checks need (position, data version, section sizes).

use std::fmt;

/// The game refuses NBT nested deeper than this.
pub const MAX_DEPTH: u32 = 512;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NbtError {
    Empty,
    BadRoot { tag: u8 },
    Truncated { at: usize },
    BadTag { tag: u8, at: usize },
    NegativeLength { at: usize },
    ListWithoutType { at: usize },
    TooDeep,
}

impl fmt::Display for NbtError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NbtError::Empty => write!(f, "there is no data at all"),
            NbtError::BadRoot { tag } => write!(f, "the data does not start with a compound tag (found tag type {tag})"),
            NbtError::Truncated { at } => write!(f, "the data ends in the middle of a tag (after {at} bytes)"),
            NbtError::BadTag { tag, at } => write!(f, "unknown tag type {tag} at byte {at}"),
            NbtError::NegativeLength { at } => write!(f, "negative array or list length at byte {at}"),
            NbtError::ListWithoutType { at } => write!(f, "non-empty list without an element type at byte {at}"),
            NbtError::TooDeep => write!(f, "nested deeper than {MAX_DEPTH} levels"),
        }
    }
}

type Result<T> = std::result::Result<T, NbtError>;

/// What the checks need to know about a chunk.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ChunkInfo {
    /// `xPos` / `zPos` at the top level (1.18 and newer terrain chunks).
    pub pos: Option<(i32, i32)>,
    /// `Level.xPos` / `Level.zPos` (terrain chunks before 1.18).
    pub level_pos: Option<(i32, i32)>,
    /// `Position` (entity chunks).
    pub entity_pos: Option<(i32, i32)>,
    pub data_version: Option<i32>,
    /// First section whose block or biome data cannot be loaded.
    pub section_problem: Option<String>,
    /// How many block and biome containers were measured.
    pub containers: u32,
}

impl ChunkInfo {
    pub fn terrain_pos(&self) -> Option<(i32, i32)> {
        self.pos.or(self.level_pos)
    }
}

/// Number of longs the game expects for a paletted container of `size`
/// entries with `palette` distinct values (the format used since 1.18).
pub fn expected_longs(palette: u64, size: u64, min_bits: u32) -> u64 {
    if palette <= 1 {
        return 0;
    }
    let bits = (64 - (palette - 1).leading_zeros()).max(min_bits);
    size.div_ceil(u64::from(64 / bits))
}

#[derive(Default)]
struct Container {
    palette: Option<u64>,
    data: Option<u64>,
}

impl Container {
    fn problem(&self, what: &str, size: u64, min_bits: u32) -> Option<String> {
        match (self.palette, self.data) {
            (None, _) => Some(format!("{what} has no palette")),
            (Some(0), _) => Some(format!("{what} has an empty palette")),
            (Some(1), _) => None,
            (Some(n), None) => Some(format!("{what} has a palette of {n} entries but no data")),
            (Some(n), Some(d)) => {
                let want = expected_longs(n, size, min_bits);
                (d != want).then(|| format!("{what} has {d} longs of data, but its palette of {n} entries needs exactly {want}"))
            }
        }
    }
}

struct Walker<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Walker<'a> {
    fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        if n > self.b.len() - self.p {
            return Err(NbtError::Truncated { at: self.b.len() });
        }
        let s = &self.b[self.p..self.p + n];
        self.p += n;
        Ok(s)
    }

    fn skip_n(&mut self, n: u64) -> Result<()> {
        if n > (self.b.len() - self.p) as u64 {
            return Err(NbtError::Truncated { at: self.b.len() });
        }
        self.p += n as usize;
        Ok(())
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.bytes(1)?[0])
    }

    fn u16(&mut self) -> Result<u16> {
        let b = self.bytes(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    fn i32(&mut self) -> Result<i32> {
        let b = self.bytes(4)?;
        Ok(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn name(&mut self) -> Result<&'a [u8]> {
        let n = self.u16()? as usize;
        self.bytes(n)
    }

    /// Length prefix of an array or list.
    fn len(&mut self) -> Result<u64> {
        let at = self.p;
        let n = self.i32()?;
        if n < 0 {
            return Err(NbtError::NegativeLength { at });
        }
        Ok(n as u64)
    }

    /// The next entry of a compound: `None` at its end.
    fn entry(&mut self) -> Result<Option<(u8, &'a [u8])>> {
        let at = self.p;
        let tag = self.u8()?;
        if tag == 0 {
            return Ok(None);
        }
        if tag > 12 {
            return Err(NbtError::BadTag { tag, at });
        }
        Ok(Some((tag, self.name()?)))
    }

    fn list_header(&mut self) -> Result<(u8, u64)> {
        let at = self.p;
        let elem = self.u8()?;
        let n = self.len()?;
        if n > 0 {
            if elem == 0 {
                return Err(NbtError::ListWithoutType { at });
            }
            if elem > 12 {
                return Err(NbtError::BadTag { tag: elem, at });
            }
        }
        Ok((elem, n))
    }

    /// Skip the items of a list that itself sits at nesting level `depth`.
    fn skip_items(&mut self, elem: u8, n: u64, depth: u32) -> Result<()> {
        if depth > MAX_DEPTH {
            return Err(NbtError::TooDeep);
        }
        if n == 0 {
            return Ok(());
        }
        match elem {
            1 => self.skip_n(n),
            2 => self.skip_n(n * 2),
            3 | 5 => self.skip_n(n * 4),
            4 | 6 => self.skip_n(n * 8),
            _ => {
                for _ in 0..n {
                    self.skip(elem, depth + 1)?;
                }
                Ok(())
            }
        }
    }

    /// Skip the payload of a tag that sits at nesting level `depth`.
    fn skip(&mut self, tag: u8, depth: u32) -> Result<()> {
        match tag {
            1 => self.skip_n(1),
            2 => self.skip_n(2),
            3 | 5 => self.skip_n(4),
            4 | 6 => self.skip_n(8),
            7 => {
                let n = self.len()?;
                self.skip_n(n)
            }
            8 => {
                let n = self.u16()?;
                self.skip_n(u64::from(n))
            }
            9 => {
                let (elem, n) = self.list_header()?;
                self.skip_items(elem, n, depth)
            }
            10 => {
                if depth > MAX_DEPTH {
                    return Err(NbtError::TooDeep);
                }
                while let Some((tag, _)) = self.entry()? {
                    self.skip(tag, depth + 1)?;
                }
                Ok(())
            }
            11 => {
                let n = self.len()?;
                self.skip_n(n * 4)
            }
            12 => {
                let n = self.len()?;
                self.skip_n(n * 8)
            }
            other => Err(NbtError::BadTag { tag: other, at: self.p }),
        }
    }

    /// `Level` of a pre-1.18 terrain chunk (a compound at level 1).
    fn level(&mut self) -> Result<Option<(i32, i32)>> {
        let (mut x, mut z) = (None, None);
        while let Some((tag, name)) = self.entry()? {
            match (tag, name) {
                (3, b"xPos") => x = Some(self.i32()?),
                (3, b"zPos") => z = Some(self.i32()?),
                _ => self.skip(tag, 2)?,
            }
        }
        Ok(x.zip(z))
    }

    /// `block_states` or `biomes` of a section (a compound at level 3).
    fn container(&mut self) -> Result<Container> {
        let mut c = Container::default();
        while let Some((tag, name)) = self.entry()? {
            match (tag, name) {
                (9, b"palette") => {
                    let (elem, n) = self.list_header()?;
                    self.skip_items(elem, n, 4)?;
                    c.palette = Some(n);
                }
                (12, b"data") => {
                    let n = self.len()?;
                    self.skip_n(n * 8)?;
                    c.data = Some(n);
                }
                _ => self.skip(tag, 4)?,
            }
        }
        Ok(c)
    }

    /// One element of `sections` (a compound at level 2).
    fn section(&mut self, info: &mut ChunkInfo) -> Result<()> {
        let mut y: Option<i32> = None;
        let (mut blocks, mut biomes) = (None, None);
        while let Some((tag, name)) = self.entry()? {
            match (tag, name) {
                (1, b"Y") => y = Some(i32::from(self.u8()? as i8)),
                (3, b"Y") => y = Some(self.i32()?),
                (10, b"block_states") => blocks = Some(self.container()?),
                (10, b"biomes") => biomes = Some(self.container()?),
                _ => self.skip(tag, 3)?,
            }
        }
        info.containers += u32::from(blocks.is_some()) + u32::from(biomes.is_some());
        if info.section_problem.is_none() {
            let problem = blocks
                .and_then(|c| c.problem("block data", 4096, 4))
                .or_else(|| biomes.and_then(|c| c.problem("biome data", 64, 1)));
            if let Some(p) = problem {
                let y = y.map_or("?".to_string(), |y| y.to_string());
                info.section_problem = Some(format!("section Y={y}: {p}"));
            }
        }
        Ok(())
    }
}

/// Validate a chunk's NBT and collect what the checks need. With `sections`
/// the block and biome containers of 1.18+ terrain sections are measured too.
pub fn inspect(buf: &[u8], sections: bool) -> Result<ChunkInfo> {
    if buf.is_empty() {
        return Err(NbtError::Empty);
    }
    let mut w = Walker { b: buf, p: 0 };
    let tag = w.u8()?;
    if tag != 10 {
        return Err(NbtError::BadRoot { tag });
    }
    w.name()?;
    let mut info = ChunkInfo::default();
    let (mut x, mut z) = (None, None);
    while let Some((tag, name)) = w.entry()? {
        match (tag, name) {
            (3, b"xPos") => x = Some(w.i32()?),
            (3, b"zPos") => z = Some(w.i32()?),
            (3, b"DataVersion") => info.data_version = Some(w.i32()?),
            (10, b"Level") => info.level_pos = w.level()?,
            (11, b"Position") => {
                let n = w.len()?;
                if n >= 2 {
                    let px = w.i32()?;
                    let pz = w.i32()?;
                    w.skip_n((n - 2) * 4)?;
                    info.entity_pos = Some((px, pz));
                } else {
                    w.skip_n(n * 4)?;
                }
            }
            (9, b"sections") if sections => {
                let (elem, n) = w.list_header()?;
                if elem == 10 {
                    for _ in 0..n {
                        w.section(&mut info)?;
                    }
                } else {
                    w.skip_items(elem, n, 1)?;
                }
            }
            _ => w.skip(tag, 1)?,
        }
    }
    info.pos = x.zip(z);
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{terrain, Nbt};

    #[test]
    fn long_counts() {
        // Blocks: 4096 entries, at least 4 bits each.
        assert_eq!(expected_longs(1, 4096, 4), 0);
        assert_eq!(expected_longs(2, 4096, 4), 256);
        assert_eq!(expected_longs(16, 4096, 4), 256);
        assert_eq!(expected_longs(17, 4096, 4), 342);
        assert_eq!(expected_longs(32, 4096, 4), 342);
        assert_eq!(expected_longs(33, 4096, 4), 410);
        assert_eq!(expected_longs(64, 4096, 4), 410);
        assert_eq!(expected_longs(65, 4096, 4), 456);
        assert_eq!(expected_longs(128, 4096, 4), 456);
        assert_eq!(expected_longs(129, 4096, 4), 512);
        assert_eq!(expected_longs(256, 4096, 4), 512);
        assert_eq!(expected_longs(257, 4096, 4), 586);
        assert_eq!(expected_longs(1025, 4096, 4), 820);
        // Biomes: 64 entries, no minimum.
        assert_eq!(expected_longs(1, 64, 1), 0);
        assert_eq!(expected_longs(2, 64, 1), 1);
        assert_eq!(expected_longs(3, 64, 1), 2);
        assert_eq!(expected_longs(4, 64, 1), 2);
        assert_eq!(expected_longs(5, 64, 1), 4);
        assert_eq!(expected_longs(9, 64, 1), 4);
        assert_eq!(expected_longs(17, 64, 1), 6);
        assert_eq!(expected_longs(64, 64, 1), 7);
    }

    #[test]
    fn reads_positions() {
        let info = inspect(&terrain(3, -7, 4000, &[]), true).unwrap();
        assert_eq!(info.pos, Some((3, -7)));
        assert_eq!(info.terrain_pos(), Some((3, -7)));
        assert_eq!(info.data_version, Some(4000));
        assert_eq!(info.section_problem, None);

        let mut old = Nbt::root();
        old.int("DataVersion", 1343).compound("Level").int("xPos", 5).int("zPos", 6).end().end();
        let info = inspect(&old.0, true).unwrap();
        assert_eq!((info.pos, info.level_pos, info.terrain_pos()), (None, Some((5, 6)), Some((5, 6))));

        let mut ent = Nbt::root();
        ent.int_array("Position", &[-2, 9]).int("DataVersion", 4000).end();
        assert_eq!(inspect(&ent.0, false).unwrap().entity_pos, Some((-2, 9)));

        // A position of the wrong type is as good as none.
        let mut odd = Nbt::root();
        odd.string("xPos", "1").int("zPos", 2).int_array("Position", &[1]).end();
        let info = inspect(&odd.0, true).unwrap();
        assert_eq!((info.pos, info.entity_pos), (None, None));
    }

    #[test]
    fn section_sizes() {
        let ok = terrain(0, 0, 4000, &[(-4, Some(1), None), (0, Some(20), Some(342)), (1, Some(300), Some(586))]);
        assert_eq!(inspect(&ok, true).unwrap().section_problem, None);
        assert_eq!(inspect(&ok, true).unwrap().containers, 6);
        assert_eq!(inspect(&ok, false).unwrap().containers, 0);

        let bad = terrain(0, 0, 4000, &[(0, Some(1), None), (5, Some(20), Some(256))]);
        let p = inspect(&bad, true).unwrap().section_problem.unwrap();
        assert!(p.starts_with("section Y=5: block data has 256 longs"), "{p}");
        // Not looked at unless asked.
        assert_eq!(inspect(&bad, false).unwrap().section_problem, None);

        let none = terrain(0, 0, 4000, &[(2, Some(3), None)]);
        assert!(inspect(&none, true).unwrap().section_problem.unwrap().contains("no data"));
        let empty = terrain(0, 0, 4000, &[(2, Some(0), None)]);
        assert!(inspect(&empty, true).unwrap().section_problem.unwrap().contains("empty palette"));
        let missing = terrain(0, 0, 4000, &[(2, None, Some(4))]);
        assert!(inspect(&missing, true).unwrap().section_problem.unwrap().contains("no palette"));
    }

    #[test]
    fn rejects_broken_nbt() {
        assert_eq!(inspect(&[], true), Err(NbtError::Empty));
        assert_eq!(inspect(&[0], true), Err(NbtError::BadRoot { tag: 0 }));
        assert_eq!(inspect(&[3, 0, 0, 0, 0, 0, 1], true), Err(NbtError::BadRoot { tag: 3 }));

        let good = terrain(1, 2, 4000, &[(0, Some(20), Some(342))]);
        assert!(inspect(&good, true).is_ok());
        // Every proper prefix is truncated, never a panic and never accepted.
        for cut in 0..good.len() {
            assert!(inspect(&good[..cut], true).is_err(), "prefix of {cut} bytes was accepted");
        }
        // Trailing bytes after the root tag are ignored, like the game does.
        let mut longer = good.clone();
        longer.extend_from_slice(&[0xFF; 9]);
        assert!(inspect(&longer, true).is_ok());

        let mut bad_tag = Nbt::root();
        bad_tag.raw(&[13, 0, 1, b'a']);
        assert!(matches!(inspect(&bad_tag.0, true), Err(NbtError::BadTag { tag: 13, .. })));

        let mut neg = Nbt::root();
        neg.raw(&[7, 0, 1, b'a', 0xFF, 0xFF, 0xFF, 0xFF]);
        assert!(matches!(inspect(&neg.0, true), Err(NbtError::NegativeLength { .. })));

        let mut untyped = Nbt::root();
        untyped.raw(&[9, 0, 1, b'a', 0, 0, 0, 0, 1]);
        assert!(matches!(inspect(&untyped.0, true), Err(NbtError::ListWithoutType { .. })));

        // An empty list may carry any element type.
        let mut empty_list = Nbt::root();
        empty_list.raw(&[9, 0, 1, b'a', 99, 0, 0, 0, 0]).end();
        assert!(inspect(&empty_list.0, true).is_ok());

        // A huge declared length must not allocate or loop.
        let mut huge = Nbt::root();
        huge.raw(&[9, 0, 1, b'a', 10, 0x7F, 0xFF, 0xFF, 0xFF]);
        assert!(matches!(inspect(&huge.0, true), Err(NbtError::Truncated { .. })));
        let mut huge = Nbt::root();
        huge.raw(&[12, 0, 1, b'a', 0x7F, 0xFF, 0xFF, 0xFF]);
        assert!(matches!(inspect(&huge.0, true), Err(NbtError::Truncated { .. })));
    }

    #[test]
    fn depth_limit() {
        let nested = |levels: usize| {
            let mut n = Nbt::root();
            for _ in 0..levels {
                n.compound("c");
            }
            for _ in 0..=levels {
                n.end();
            }
            n.0
        };
        assert!(inspect(&nested(MAX_DEPTH as usize), true).is_ok());
        assert_eq!(inspect(&nested(MAX_DEPTH as usize + 1), true), Err(NbtError::TooDeep));

        // Lists nest too.
        let mut lists = Nbt::root();
        lists.raw(&[9, 0, 1, b'l']);
        for _ in 0..600 {
            lists.raw(&[9, 0, 0, 0, 1]);
        }
        assert_eq!(inspect(&lists.0, true), Err(NbtError::TooDeep));
    }
}
