//! Chunk decompression: gzip, zlib, uncompressed and LZ4, as the game reads
//! them, but telling a damaged stream apart from one that merely ends early.

use flate2::{Decompress, FlushDecompress, Status};
use std::io::Read;

/// Nothing real comes close; this only stops decompression bombs.
pub const MAX_OUT: usize = 512 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// The stream is damaged. The output so far is not trustworthy.
    Corrupt(String),
    /// The stream is intact as far as it goes but ends before its end marker.
    /// The output so far is what the game would get to read.
    Truncated,
    /// More than [`MAX_OUT`] bytes.
    TooLarge,
}

/// Reusable decompressor: one per worker thread, so inflating a chunk does
/// not allocate zlib state every time.
pub struct Codec {
    z: Decompress,
}

impl Default for Codec {
    fn default() -> Self {
        Codec { z: Decompress::new(true) }
    }
}

/// Compression schemes this build can verify.
pub fn known_scheme(scheme: u8) -> bool {
    (1..=4).contains(&scheme)
}

pub fn scheme_name(scheme: u8) -> &'static str {
    match scheme {
        1 => "gzip",
        2 => "zlib",
        3 => "none",
        4 => "LZ4",
        127 => "custom",
        _ => "unknown",
    }
}

impl Codec {
    /// Decompress a chunk payload into `out`. On an error `out` holds what
    /// could be decompressed up to that point.
    pub fn decompress(&mut self, scheme: u8, data: &[u8], out: &mut Vec<u8>) -> Result<(), DecodeError> {
        out.clear();
        match scheme {
            1 => gunzip(data, out),
            2 => self.zlib(data, out),
            3 => {
                out.extend_from_slice(data);
                Ok(())
            }
            4 => lz4_block_stream(data, out),
            other => Err(DecodeError::Corrupt(format!("unsupported compression scheme {other}"))),
        }
    }

    /// The length of the complete zlib or gzip stream that `data` starts
    /// with, whatever follows it. `out` receives the decompressed data.
    pub fn stream_len(&mut self, scheme: u8, data: &[u8], out: &mut Vec<u8>) -> Option<usize> {
        match scheme {
            1 => {
                out.clear();
                let mut gz = flate2::bufread::GzDecoder::new(data);
                let n = (&mut gz).take(MAX_OUT as u64 + 1).read_to_end(out).ok()?;
                (n <= MAX_OUT).then(|| data.len() - gz.into_inner().len())
            }
            2 => {
                self.zlib(data, out).ok()?;
                Some(self.z.total_in() as usize)
            }
            _ => None,
        }
    }

    fn zlib(&mut self, data: &[u8], out: &mut Vec<u8>) -> Result<(), DecodeError> {
        const STEP: usize = 1 << 17;
        self.z.reset(true);
        if out.capacity() < STEP {
            out.reserve(STEP);
        }
        loop {
            let consumed = self.z.total_in() as usize;
            let produced = self.z.total_out();
            let status = self
                .z
                .decompress_vec(&data[consumed.min(data.len())..], out, FlushDecompress::None)
                .map_err(|e| DecodeError::Corrupt(format!("zlib: {e}")))?;
            if status == Status::StreamEnd {
                return Ok(());
            }
            if out.len() == out.capacity() {
                if out.len() >= MAX_OUT {
                    return Err(DecodeError::TooLarge);
                }
                out.reserve(out.capacity().clamp(STEP, 64 << 20));
                continue;
            }
            if self.z.total_in() as usize == consumed && self.z.total_out() == produced {
                return Err(DecodeError::Truncated);
            }
        }
    }
}

fn gunzip(data: &[u8], out: &mut Vec<u8>) -> Result<(), DecodeError> {
    let mut reader = flate2::read::MultiGzDecoder::new(data).take(MAX_OUT as u64 + 1);
    match reader.read_to_end(out) {
        Ok(_) if out.len() > MAX_OUT => Err(DecodeError::TooLarge),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Err(DecodeError::Truncated),
        Err(e) => Err(DecodeError::Corrupt(format!("gzip: {e}"))),
    }
}

const LZ4_MAGIC: &[u8; 8] = b"LZ4Block";
const LZ4_HEADER: usize = 21;
const LZ4_RAW: u8 = 0x10;
const LZ4_COMPRESSED: u8 = 0x20;
/// lz4-java's block checksum: XXH32 with this seed, lowest 28 bits.
const LZ4_SEED: u32 = 0x9747_B28C;

/// Minecraft's LZ4 chunks use lz4-java's `LZ4BlockOutputStream` framing:
/// repeated blocks of `"LZ4Block" token clen olen checksum data`, closed by
/// an empty block. The checks are the ones `LZ4BlockInputStream` makes.
fn lz4_block_stream(mut data: &[u8], out: &mut Vec<u8>) -> Result<(), DecodeError> {
    let corrupt = |why: &str| Err(DecodeError::Corrupt(format!("LZ4: {why}")));
    let le = |b: &[u8]| u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    loop {
        if data.is_empty() {
            // No closing block. The game only notices if it wants more data
            // than there is, which the NBT check that follows finds out.
            return Err(DecodeError::Truncated);
        }
        if data.len() < LZ4_HEADER {
            return Err(DecodeError::Truncated);
        }
        if &data[..8] != LZ4_MAGIC {
            return corrupt("bad block magic");
        }
        let token = data[8];
        let method = token & 0xF0;
        let level = 10 + u32::from(token & 0x0F);
        let clen = le(&data[9..13]) as i32;
        let olen = le(&data[13..17]) as i32;
        let check = le(&data[17..21]);
        if method != LZ4_RAW && method != LZ4_COMPRESSED {
            return corrupt("unknown block method");
        }
        if clen < 0 || olen < 0 || olen as u64 > 1u64 << level || (olen == 0 && clen != 0) || (method == LZ4_RAW && olen != clen) {
            return corrupt("impossible block sizes");
        }
        data = &data[LZ4_HEADER..];
        let (clen, olen) = (clen as usize, olen as usize);
        if olen == 0 {
            return if check == 0 { Ok(()) } else { corrupt("bad end-of-stream block") };
        }
        if clen > data.len() {
            return Err(DecodeError::Truncated);
        }
        if out.len() + olen > MAX_OUT {
            return Err(DecodeError::TooLarge);
        }
        let (block, rest) = data.split_at(clen);
        let start = out.len();
        if method == LZ4_RAW {
            out.extend_from_slice(block);
        } else {
            out.resize(start + olen, 0);
            match lz4_flex::block::decompress_into(block, &mut out[start..]) {
                Ok(n) if n == olen => {}
                Ok(_) => {
                    out.truncate(start);
                    return corrupt("block is shorter than declared");
                }
                Err(e) => {
                    out.truncate(start);
                    return corrupt(&e.to_string());
                }
            }
        }
        if twox_hash::XxHash32::oneshot(LZ4_SEED, &out[start..]) & 0x0FFF_FFFF != check {
            out.truncate(start);
            return corrupt("block checksum mismatch");
        }
        data = rest;
    }
}

#[cfg(test)]
pub mod testing {
    use super::*;
    use std::io::Write;

    pub fn zlib(data: &[u8]) -> Vec<u8> {
        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    }

    pub fn gzip(data: &[u8]) -> Vec<u8> {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    }

    /// What lz4-java's `LZ4BlockOutputStream` writes, with `block` bytes of
    /// input per block.
    pub fn lz4(data: &[u8], block: usize) -> Vec<u8> {
        let mut out = Vec::new();
        let level = (block.max(1024).next_power_of_two().trailing_zeros() - 10) as u8;
        for part in data.chunks(block) {
            let packed = lz4_flex::block::compress(part);
            let (method, body) = if packed.len() >= part.len() { (LZ4_RAW, part) } else { (LZ4_COMPRESSED, &packed[..]) };
            out.extend_from_slice(LZ4_MAGIC);
            out.push(method | level);
            out.extend_from_slice(&(body.len() as u32).to_le_bytes());
            out.extend_from_slice(&(part.len() as u32).to_le_bytes());
            out.extend_from_slice(&(twox_hash::XxHash32::oneshot(LZ4_SEED, part) & 0x0FFF_FFFF).to_le_bytes());
            out.extend_from_slice(body);
        }
        out.extend_from_slice(LZ4_MAGIC);
        out.push(LZ4_RAW | level);
        out.extend_from_slice(&[0; 12]);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    fn sample(n: usize) -> Vec<u8> {
        (0..n).map(|i| ((i * 7 + i / 300) % 251) as u8).collect()
    }

    #[test]
    fn stream_lengths() {
        let mut codec = Codec::default();
        let mut out = Vec::new();
        let data = sample(50_000);
        for (scheme, packed) in [(1u8, gzip(&data)), (2, zlib(&data))] {
            let mut padded = packed.clone();
            padded.extend_from_slice(&[0; 3000]);
            padded.extend_from_slice(b"garbage");
            assert_eq!(codec.stream_len(scheme, &padded, &mut out), Some(packed.len()), "scheme {scheme}");
            assert_eq!(out, data);
            assert_eq!(codec.stream_len(scheme, &packed, &mut out), Some(packed.len()));
            assert_eq!(codec.stream_len(scheme, &packed[..packed.len() - 1], &mut out), None);
            let mut bad = padded.clone();
            bad[packed.len() / 2] ^= 0x10;
            assert_eq!(codec.stream_len(scheme, &bad, &mut out), None);
            assert_eq!(codec.stream_len(scheme, &[], &mut out), None);
        }
        assert_eq!(codec.stream_len(3, &data, &mut out), None);
        assert_eq!(codec.stream_len(4, &lz4(&data, 4096), &mut out), None);
    }

    #[test]
    fn round_trips_reuse_the_codec() {
        let mut codec = Codec::default();
        let mut out = Vec::new();
        for n in [0usize, 1, 10, 4096, 200_000, 3_000_000] {
            let data = sample(n);
            codec.decompress(2, &zlib(&data), &mut out).unwrap();
            assert_eq!(out, data, "zlib {n}");
            codec.decompress(1, &gzip(&data), &mut out).unwrap();
            assert_eq!(out, data, "gzip {n}");
            codec.decompress(3, &data, &mut out).unwrap();
            assert_eq!(out, data, "raw {n}");
            codec.decompress(4, &lz4(&data, 1 << 16), &mut out).unwrap();
            assert_eq!(out, data, "lz4 {n}");
        }
        assert!(matches!(codec.decompress(9, &[1, 2, 3], &mut out), Err(DecodeError::Corrupt(_))));
    }

    #[test]
    fn zlib_truncation_and_damage() {
        let mut codec = Codec::default();
        let mut out = Vec::new();
        let data = sample(100_000);
        let z = zlib(&data);
        assert_eq!(codec.decompress(2, &[], &mut out), Err(DecodeError::Truncated));
        assert_eq!(codec.decompress(2, &z[..1], &mut out), Err(DecodeError::Truncated));
        // Without its checksum the whole content still comes out.
        assert_eq!(codec.decompress(2, &z[..z.len() - 4], &mut out), Err(DecodeError::Truncated));
        assert_eq!(out, data);
        assert_eq!(codec.decompress(2, &z[..z.len() / 2], &mut out), Err(DecodeError::Truncated));
        assert!(out.len() < data.len());
        assert_eq!(&out[..], &data[..out.len()]);
        // A wrong checksum is damage, not truncation.
        let mut bad = z.clone();
        let last = bad.len() - 1;
        bad[last] ^= 0x55;
        assert!(matches!(codec.decompress(2, &bad, &mut out), Err(DecodeError::Corrupt(_))));
        assert!(matches!(codec.decompress(2, &[0xDE, 0xAD, 0xBE, 0xEF, 0, 0], &mut out), Err(DecodeError::Corrupt(_))));
        // Bytes after the end of the stream do not matter.
        let mut padded = z.clone();
        padded.extend_from_slice(&[0xAB; 100]);
        codec.decompress(2, &padded, &mut out).unwrap();
        assert_eq!(out, data);
        // The codec recovers after every kind of failure.
        codec.decompress(2, &z, &mut out).unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn gzip_truncation_and_damage() {
        let mut codec = Codec::default();
        let mut out = Vec::new();
        let data = sample(50_000);
        let g = gzip(&data);
        assert_eq!(codec.decompress(1, &g[..g.len() - 3], &mut out), Err(DecodeError::Truncated));
        let mut bad = g.clone();
        let n = bad.len();
        bad[n - 6] ^= 1; // CRC
        assert!(matches!(codec.decompress(1, &bad, &mut out), Err(DecodeError::Corrupt(_))));
        assert!(matches!(codec.decompress(1, b"not gzip at all", &mut out), Err(DecodeError::Corrupt(_))));
    }

    #[test]
    fn lz4_truncation_and_damage() {
        let mut codec = Codec::default();
        let mut out = Vec::new();
        let data = sample(150_000);
        let l = lz4(&data, 1 << 16);
        // Three data blocks and the closing block.
        assert_eq!(l.windows(8).filter(|w| w == LZ4_MAGIC).count(), 4);

        // Missing only the closing block: all the content is there.
        assert_eq!(codec.decompress(4, &l[..l.len() - LZ4_HEADER], &mut out), Err(DecodeError::Truncated));
        assert_eq!(out, data);
        assert_eq!(codec.decompress(4, &l[..l.len() / 2], &mut out), Err(DecodeError::Truncated));
        assert!(out.len() < data.len());
        assert_eq!(codec.decompress(4, &[], &mut out), Err(DecodeError::Truncated));

        let corrupt = |codec: &mut Codec, bytes: &[u8]| {
            let mut out = Vec::new();
            matches!(codec.decompress(4, bytes, &mut out), Err(DecodeError::Corrupt(_)))
        };
        let mut bad = l.clone();
        bad[0] = b'X'; // magic
        assert!(corrupt(&mut codec, &bad));
        let mut bad = l.clone();
        bad[17] ^= 1; // checksum
        assert!(corrupt(&mut codec, &bad));
        let mut bad = l.clone();
        bad[8] = 0x30 | (bad[8] & 0x0F); // method
        assert!(corrupt(&mut codec, &bad));
        let mut bad = l.clone();
        bad[16] = 0x7F; // original length far beyond the block size
        assert!(corrupt(&mut codec, &bad));
        let mut bad = l.clone();
        bad[LZ4_HEADER + 40] ^= 0xFF; // payload
        assert!(corrupt(&mut codec, &bad));

        // Incompressible data is stored raw.
        let mut x = 0x2545_F491u32;
        let noise: Vec<u8> = (0..5000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                (x >> 11) as u8
            })
            .collect();
        let l = lz4(&noise, 1 << 16);
        assert_eq!(l[8] & 0xF0, LZ4_RAW);
        codec.decompress(4, &l, &mut out).unwrap();
        assert_eq!(out, noise);
    }

    /// A stream written by the real lz4-java 1.10.1 `LZ4BlockOutputStream`
    /// (the library the game uses), with the default 64 KiB block size.
    #[test]
    fn lz4_java_reference_stream() {
        let stream = include_bytes!("../tests/data/lz4-java.bin");
        let expected: Vec<u8> = (0..100_000u32).map(|i| ((i * 31 + i / 1000) % 251) as u8).collect();
        let mut out = Vec::new();
        Codec::default().decompress(4, stream, &mut out).unwrap();
        assert_eq!(out, expected);
    }
}
