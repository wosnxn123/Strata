//! Anvil `.mca` region file reader/writer.
//!
//! Layout (Minecraft Anvil region format):
//! - 4 KiB location table: 1024 big-endian u32, index `(x & 31) + (z & 31) * 32`.
//!   Entry = `(offset_sectors << 8) | count_sectors`.
//! - 4 KiB timestamp table: 1024 big-endian u32.
//! - Chunk data: 4096-byte sectors starting at sector 2. Each record is
//!   `u32 BE n` + 1 version byte + `n - 1` bytes of compressed NBT,
//!   padded to whole sectors.

use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

use anyhow::{bail, Context};
use flate2::read::{GzDecoder, ZlibDecoder};
use flate2::write::ZlibEncoder;
use flate2::Compression;

const SECTOR: usize = 4096;
const HEADER_SIZE: usize = 2 * SECTOR;
const ENTRIES: usize = 1024;

/// Hard cap on decompressed bytes for a single chunk (256 MiB). Vanilla chunks
/// decompress to far less than this (typically a few MiB at most); the cap only
/// guards against decompression bombs from corrupt or malicious region files.
pub const MAX_CHUNK_DECOMPRESSED: usize = 256 * 1024 * 1024;

/// Version bytes for chunk compression schemes.
const VER_GZIP: u8 = 1;
const VER_DEFLATE: u8 = 2;
const VER_NONE: u8 = 3;
const VER_LZ4: u8 = 4;
/// Chunks stored in external `.mcc` files use version | 128.
const VER_EXTERNAL_MASK: u8 = 0x80;

const COMPRESSION_LEVEL: u32 = 6;

/// A single chunk payload stored inside a region file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkLoc {
    pub x: u8,
    pub z: u8,
    /// Raw (decompressed) NBT bytes.
    pub nbt: Vec<u8>,
    pub timestamp: u32,
}

fn chunk_index(x: u8, z: u8) -> usize {
    (x & 31) as usize + (z & 31) as usize * 32
}

/// Read an Anvil `.mca` region file, returning all present chunks.
pub fn read_region(path: &Path) -> anyhow::Result<Vec<ChunkLoc>> {
    let mut data = Vec::new();
    File::open(path)
        .with_context(|| format!("opening {}", path.display()))?
        .read_to_end(&mut data)?;

    if data.len() < HEADER_SIZE {
        bail!("region file too small: {} bytes", data.len());
    }

    let mut chunks = Vec::new();
    for index in 0..ENTRIES {
        let base = index * 4;
        let loc = u32::from_be_bytes([
            data[base],
            data[base + 1],
            data[base + 2],
            data[base + 3],
        ]);
        if loc == 0 {
            continue; // empty slot
        }
        let offset = (loc >> 8) as usize;
        let count = (loc & 0xFF) as usize;
        if count == 0 {
            continue;
        }
        if offset < 2 {
            bail!(
                "chunk at index {} points to sector {offset} inside the file header",
                index
            );
        }

        let start = offset * SECTOR;
        let end = start + count * SECTOR;
        if start + 5 > data.len() || end > data.len() {
            bail!("chunk at index {} references bytes outside file", index);
        }

        let n = u32::from_be_bytes([
            data[start],
            data[start + 1],
            data[start + 2],
            data[start + 3],
        ]) as usize;
        // The record (4-byte length + `n` bytes) must stay inside the slot's
        // allocated sectors; otherwise a crafted `n` could run past them.
        if n == 0 || n > count * SECTOR - 5 {
            bail!(
                "chunk at index {} has bad record length {n} for a {count}-sector slot",
                index
            );
        }
        if start + 4 + n > data.len() {
            bail!("chunk at index {} record runs past end of file", index);
        }
        let version = data[start + 4];
        let payload = &data[start + 5..start + 4 + n];

        if version & VER_EXTERNAL_MASK != 0 {
            bail!("unsupported external chunk at index {}", index);
        }

        let nbt = match version {
            VER_GZIP => {
                let mut out = Vec::new();
                let n_read = GzDecoder::new(payload)
                    .take(MAX_CHUNK_DECOMPRESSED as u64 + 1)
                    .read_to_end(&mut out)
                    .context("gzip chunk")?;
                if n_read > MAX_CHUNK_DECOMPRESSED {
                    bail!(
                        "gzip chunk at index {} decompresses to more than {MAX_CHUNK_DECOMPRESSED} bytes",
                        index
                    );
                }
                out
            }
            VER_DEFLATE => {
                let mut out = Vec::new();
                let n_read = ZlibDecoder::new(payload)
                    .take(MAX_CHUNK_DECOMPRESSED as u64 + 1)
                    .read_to_end(&mut out)
                    .context("deflate chunk")?;
                if n_read > MAX_CHUNK_DECOMPRESSED {
                    bail!(
                        "deflate chunk at index {} decompresses to more than {MAX_CHUNK_DECOMPRESSED} bytes",
                        index
                    );
                }
                out
            }
            VER_NONE => payload.to_vec(),
            VER_LZ4 => {
                if payload.len() < 4 {
                    bail!("lz4 chunk at index {} too short", index);
                }
                let orig_len = u32::from_be_bytes([
                    payload[0],
                    payload[1],
                    payload[2],
                    payload[3],
                ]) as usize;
                if orig_len > MAX_CHUNK_DECOMPRESSED {
                    bail!(
                        "lz4 chunk at index {} claims {orig_len} decompressed bytes (cap {MAX_CHUNK_DECOMPRESSED})",
                        index
                    );
                }
                lz4_flex::block::decompress(&payload[4..], orig_len)
                    .context("lz4 chunk")?
            }
            v => bail!("unknown chunk compression version {}", v),
        };

        let ts_base = SECTOR + base;
        let timestamp = u32::from_be_bytes([
            data[ts_base],
            data[ts_base + 1],
            data[ts_base + 2],
            data[ts_base + 3],
        ]);

        chunks.push(ChunkLoc {
            x: (index % 32) as u8,
            z: (index / 32) as u8,
            nbt,
            timestamp,
        });
    }

    Ok(chunks)
}

/// Write chunks into an Anvil `.mca` region file using DEFLATE (version 2).
pub fn write_region(path: &Path, chunks: &[ChunkLoc]) -> anyhow::Result<()> {
    // Compress every payload first so sector allocation is exact.
    let mut records: Vec<Vec<u8>> = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        let mut enc = ZlibEncoder::new(Vec::new(), Compression::new(COMPRESSION_LEVEL));
        enc.write_all(&chunk.nbt)?;
        let compressed = enc.finish()?;
        // record: u32 BE length + version byte + compressed data
        let record_len = 4 + 1 + compressed.len();
        let sectors = record_len.div_ceil(SECTOR);
        if sectors > 255 {
            bail!(
                "chunk ({}, {}) record is {record_len} bytes = {sectors} sectors; \
                 the Anvil location table can only address up to 255 sectors per chunk",
                chunk.x,
                chunk.z
            );
        }
        let len = (1 + compressed.len()) as u32;
        let mut record = Vec::with_capacity(record_len);
        record.extend_from_slice(&len.to_be_bytes());
        record.push(VER_DEFLATE);
        record.extend_from_slice(&compressed);
        records.push(record);
    }

    let mut locations = [0u32; ENTRIES];
    let mut timestamps = [0u32; ENTRIES];
    let mut body = Vec::new();
    let mut sector = 2usize; // sectors 0 and 1 are the header

    for (chunk, record) in chunks.iter().zip(&records) {
        let index = chunk_index(chunk.x, chunk.z);
        let sectors = record.len().div_ceil(SECTOR);
        locations[index] = ((sector as u32) << 8) | (sectors as u32);
        timestamps[index] = chunk.timestamp;
        body.extend_from_slice(record);
        body.resize(body.len() + sectors * SECTOR - record.len(), 0);
        sector += sectors;
    }

    let mut out = vec![0u8; HEADER_SIZE + body.len()];
    for i in 0..ENTRIES {
        out[i * 4..i * 4 + 4].copy_from_slice(&locations[i].to_be_bytes());
        out[SECTOR + i * 4..SECTOR + i * 4 + 4]
            .copy_from_slice(&timestamps[i].to_be_bytes());
    }
    out[HEADER_SIZE..].copy_from_slice(&body);

    File::create(path)
        .with_context(|| format!("creating {}", path.display()))?
        .write_all(&out)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anvil_write_read_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("r.0.0.mca");
        let chunks = vec![
            ChunkLoc { x: 0, z: 0, nbt: vec![1, 2, 3], timestamp: 100 },
            ChunkLoc { x: 31, z: 31, nbt: vec![9; 5000], timestamp: 200 }, // 跨扇区
        ];
        write_region(&path, &chunks).unwrap();
        let back = read_region(&path).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].nbt, vec![1, 2, 3]);
        assert_eq!(back[0].timestamp, 100);
        assert_eq!(back[1].nbt, vec![9; 5000]);
    }

    #[test]
    fn empty_slots_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("r.0.0.mca");
        write_region(&path, &[ChunkLoc { x: 3, z: 7, nbt: b"hi".to_vec(), timestamp: 1 }]).unwrap();
        assert_eq!(read_region(&path).unwrap().len(), 1);
    }
}
