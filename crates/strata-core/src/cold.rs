//! Cold-tier chunked solid archive (`.varc`, read-only).
//!
//! An archive packs a region's records into zstd-compressed blocks of at most
//! [`COLD_BLOCK_CHUNKS`] records each. Before chunking, records are ordered by
//! *superfeatures* (min/max `xxh64` over 32-byte windows of the NBT) so
//! similar payloads land in the same block and compress together.
//!
//! File layout (all little-endian):
//!
//! | section       | contents |
//! |---------------|----------|
//! | header (20 B) | magic "VARC" \| region_x i32 \| region_z i32 \| block_count u32 \| slot_count u32 |
//! | block table   | `block_count × (file_offset u64 \| comp_len u32 \| plain_len u32)` |
//! | slot table    | `slot_count × (x_rel u16 \| z_rel u16 \| type_id u16 \| block u16 \| offset_in_block u32 \| plain_len u32)`, sorted by `(z_rel, x_rel, type_id)` |
//! | block data    | one zstd frame per block; a block is the concatenation of its entries, each `[40-byte envelope | NBT]` |
//!
//! Envelopes stored inside blocks carry `payload_len` = uncompressed NBT
//! length and `payload_hash` = `xxh64(NBT, 0)`; `comp_id` keeps the caller's
//! value unchanged.
//!
//! **Phase 1 limitation:** the dictionary is *not* stored in the archive.
//! Blocks compressed with `dict = Some(_)` cannot be decoded by
//! [`ArchiveReader`]; pass `None` until dictionary plumbing lands.

use std::fs::{self, File};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use xxhash_rust::xxh64::xxh64;

use crate::envelope::{Envelope, ENVELOPE_SIZE};
use crate::StrataError;

/// Maximum number of records per compressed block.
pub const COLD_BLOCK_CHUNKS: usize = 64;

const MAGIC: [u8; 4] = *b"VARC";
const HEADER_LEN: u64 = 20;

/// Build statistics returned by [`ArchiveBuilder::finish`].
#[derive(Debug, Clone, Copy)]
pub struct ArchiveSummary {
    /// Number of compressed blocks written.
    pub blocks: u32,
    /// Total size of the compressed block payloads, in bytes.
    pub compressed_bytes: u64,
    /// Total decompressed block size (envelopes + NBT), in bytes.
    pub plain_bytes: u64,
}

/// `(min_h, max_h)` superfeature of one NBT blob.
///
/// The blob is walked in 32-byte windows; a short tail window is included as
/// well, and a blob shorter than 32 bytes hashes as a single window.
fn superfeatures(nbt: &[u8]) -> (u64, u64) {
    let mut min_h = u64::MAX;
    let mut max_h = 0u64;
    let mut start = 0;
    while start < nbt.len() {
        let end = (start + 32).min(nbt.len());
        let h = xxh64(&nbt[start..end], 0);
        min_h = min_h.min(h);
        max_h = max_h.max(h);
        if end == nbt.len() {
            break;
        }
        start = end;
    }
    (min_h, max_h)
}

struct BuildEntry {
    min_h: u64,
    max_h: u64,
    x_rel: u16,
    z_rel: u16,
    env: Envelope,
    nbt: Vec<u8>,
}

/// Builds a read-only `.varc` archive.
pub struct ArchiveBuilder {
    region_x: i32,
    region_z: i32,
    level: i32,
    dict: Option<Vec<u8>>,
    items: Vec<Option<(Envelope, Vec<u8>)>>,
    by_key: std::collections::HashMap<(u16, u16, u16), usize>,
}

impl ArchiveBuilder {
    /// Start a builder for the region at `(region_x, region_z)`.
    ///
    /// `level` is the zstd compression level; `dict` is an optional trained
    /// zstd dictionary (see the module-level Phase 1 limitation).
    pub fn new(region_x: i32, region_z: i32, level: i32, dict: Option<Vec<u8>>) -> Self {
        Self {
            region_x,
            region_z,
            level,
            dict,
            items: Vec::new(),
            by_key: std::collections::HashMap::new(),
        }
    }

    /// Queue one record: `env` is the raw envelope, `nbt` the uncompressed
    /// payload. `payload_len`/`payload_hash` are recomputed in
    /// [`finish`](Self::finish) from the uncompressed NBT; `comp_id` is kept
    /// as given.
    ///
    /// Re-adding the same `(chunk_x & 31, chunk_z & 31, type_id)` key replaces
    /// the earlier record (last write wins), keeping the slot table unique.
    pub fn add(&mut self, env: Envelope, nbt: Vec<u8>) {
        let key = ((env.chunk_x & 31) as u16, (env.chunk_z & 31) as u16, env.type_id);
        match self.by_key.get(&key) {
            Some(&idx) => self.items[idx] = Some((env, nbt)),
            None => {
                self.by_key.insert(key, self.items.len());
                self.items.push(Some((env, nbt)));
            }
        }
    }

    /// Sort by superfeatures, chunk, compress, and write the archive to `path`.
    pub fn finish(self, path: &Path) -> Result<ArchiveSummary, StrataError> {
        let Self {
            region_x,
            region_z,
            level,
            dict,
            items,
            by_key: _,
        } = self;
        let mut keyed: Vec<BuildEntry> = Vec::with_capacity(items.len());
        for item in items.into_iter().flatten() {
            let (env, nbt) = item;
            let (min_h, max_h) = superfeatures(&nbt);
            keyed.push(BuildEntry {
                min_h,
                max_h,
                x_rel: (env.chunk_x & 31) as u16,
                z_rel: (env.chunk_z & 31) as u16,
                env,
                nbt,
            });
        }
        // Superfeature ordering: similar payloads become neighbours so zstd
        // finds matches inside each block.
        keyed.sort_unstable_by_key(|e| (e.min_h, e.max_h, e.x_rel, e.z_rel, e.env.type_id));

        struct BlockOut {
            comp: Vec<u8>,
            plain_len: u32,
        }
        struct SlotRec {
            x_rel: u16,
            z_rel: u16,
            type_id: u16,
            block: u16,
            offset_in_block: u32,
            plain_len: u32,
        }

        // Compress every block up front so file offsets are known before the
        // header and tables are written.
        let mut block_outs: Vec<BlockOut> = Vec::new();
        let mut slots: Vec<SlotRec> = Vec::new();
        for chunk in keyed.chunks(COLD_BLOCK_CHUNKS) {
            let mut plain = Vec::new();
            for e in chunk {
                let mut env = e.env.clone();
                env.payload_len = u32::try_from(e.nbt.len()).map_err(|_| {
                    StrataError::Codec(format!("nbt size {} exceeds u32", e.nbt.len()))
                })?;
                env.payload_hash = xxh64(&e.nbt, 0);
                let mut buf = [0u8; ENVELOPE_SIZE];
                env.encode(&mut buf);
                plain.extend_from_slice(&buf);
                plain.extend_from_slice(&e.nbt);
            }
            let comp = Self::compress_block(&dict, level, &plain)?;
            let block_id = u16::try_from(block_outs.len())
                .map_err(|_| StrataError::Codec("cold archive exceeds 65536 blocks".into()))?;
            let mut offset_in_block = ENVELOPE_SIZE as u32;
            for e in chunk {
                slots.push(SlotRec {
                    x_rel: e.x_rel,
                    z_rel: e.z_rel,
                    type_id: e.env.type_id,
                    block: block_id,
                    offset_in_block,
                    plain_len: e.nbt.len() as u32,
                });
                offset_in_block += ENVELOPE_SIZE as u32 + e.nbt.len() as u32;
            }
            let plain_len = u32::try_from(plain.len()).map_err(|_| {
                StrataError::Codec(format!("cold block plain size {} exceeds u32", plain.len()))
            })?;
            block_outs.push(BlockOut {
                comp,
                plain_len,
            });
        }

        // Slot table is serialized in (z_rel, x_rel, type_id) order so the
        // reader can binary-search it; block payloads stay in superfeature order.
        slots.sort_by_key(|s| (s.z_rel, s.x_rel, s.type_id));

        let tables_len = (block_outs.len() as u64 + slots.len() as u64) * 16;
        let mut data_start = HEADER_LEN + tables_len;
        let mut offsets = Vec::with_capacity(block_outs.len());
        for b in &block_outs {
            offsets.push(data_start);
            data_start += b.comp.len() as u64;
        }

        let mut out = Vec::with_capacity((data_start) as usize);
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&region_x.to_le_bytes());
        out.extend_from_slice(&region_z.to_le_bytes());
        out.extend_from_slice(&(block_outs.len() as u32).to_le_bytes());
        out.extend_from_slice(&(slots.len() as u32).to_le_bytes());
        for (b, offset) in block_outs.iter().zip(&offsets) {
            out.extend_from_slice(&offset.to_le_bytes());
            out.extend_from_slice(&(b.comp.len() as u32).to_le_bytes());
            out.extend_from_slice(&b.plain_len.to_le_bytes());
        }
        for s in &slots {
            out.extend_from_slice(&s.x_rel.to_le_bytes());
            out.extend_from_slice(&s.z_rel.to_le_bytes());
            out.extend_from_slice(&s.type_id.to_le_bytes());
            out.extend_from_slice(&s.block.to_le_bytes());
            out.extend_from_slice(&s.offset_in_block.to_le_bytes());
            out.extend_from_slice(&s.plain_len.to_le_bytes());
        }
        for b in &block_outs {
            out.extend_from_slice(&b.comp);
        }

        let file = File::create(path)?;
        let mut w = BufWriter::new(file);
        w.write_all(&out)?;
        w.flush()?;

        let compressed_bytes = block_outs.iter().map(|b| b.comp.len() as u64).sum();
        let plain_bytes = block_outs.iter().map(|b| b.plain_len as u64).sum();
        Ok(ArchiveSummary {
            blocks: block_outs.len() as u32,
            compressed_bytes,
            plain_bytes,
        })
    }

    fn compress_block(dict: &Option<Vec<u8>>, level: i32, data: &[u8]) -> Result<Vec<u8>, StrataError> {
        let mut out = Vec::new();
        let res = match dict {
            None => zstd::stream::copy_encode(data, &mut out, level),
            // Phase 1: the dictionary is not stored in the archive, so blocks
            // written through this path cannot be read back by ArchiveReader
            // until caller-supplied dictionary plumbing lands.
            Some(dict) => {
                let prepared = zstd::dict::EncoderDictionary::copy(dict, level);
                let mut enc =
                    zstd::stream::write::Encoder::with_prepared_dictionary(&mut out, &prepared)?;
                enc.write_all(data)?;
                enc.finish().map(|_| ())
            }
        };
        res.map_err(|e| StrataError::Codec(format!("cold block zstd encode: {e}")))?;
        Ok(out)
    }
}

struct BlockMeta {
    file_offset: u64,
    comp_len: u32,
    plain_len: u32,
}

struct SlotMeta {
    key: (u16, u16, u16), // (z_rel, x_rel, type_id) — binary-search order
    block: u16,
    offset_in_block: u32,
    plain_len: u32,
}

/// Read-side handle for a `.varc` archive.
///
/// Holds the file handle, the parsed block/slot tables, a one-block cache of
/// the most recently decompressed block, and the in-memory invalidation
/// bitmap (persisted to `<archive>.varc.inv`).
pub struct ArchiveReader {
    path: PathBuf,
    file: File,
    region_x: i32,
    region_z: i32,
    blocks: Vec<BlockMeta>,
    slots: Vec<SlotMeta>,
    invalid: Vec<u8>,
    cache: Option<(u16, Vec<u8>)>,
}

impl ArchiveReader {
    /// Open and parse an archive, validating header bounds and slot ordering.
    pub fn open(path: &Path) -> Result<Self, StrataError> {
        let corrupt = |detail: String| StrataError::Corrupt {
            path: path.display().to_string(),
            detail,
        };
        let mut file = File::open(path)?;
        let len = file.metadata()?.len();
        if len < HEADER_LEN {
            return Err(corrupt("file shorter than header".into()));
        }
        let mut hdr = [0u8; HEADER_LEN as usize];
        file.read_exact(&mut hdr)?;
        if hdr[0..4] != MAGIC {
            return Err(corrupt("bad magic".into()));
        }
        let region_x = i32::from_le_bytes(hdr[4..8].try_into().unwrap());
        let region_z = i32::from_le_bytes(hdr[8..12].try_into().unwrap());
        let block_count = u32::from_le_bytes(hdr[12..16].try_into().unwrap()) as usize;
        let slot_count = u32::from_le_bytes(hdr[16..20].try_into().unwrap()) as usize;

        let block_bytes = (block_count as u64)
            .checked_mul(16)
            .ok_or_else(|| corrupt("table size overflow".into()))?;
        let slot_bytes = (slot_count as u64)
            .checked_mul(16)
            .ok_or_else(|| corrupt("table size overflow".into()))?;
        let tables_end = HEADER_LEN
            .checked_add(block_bytes + slot_bytes)
            .ok_or_else(|| corrupt("table size overflow".into()))?;
        if len < tables_end {
            return Err(corrupt("file truncated inside tables".into()));
        }
        let mut tables = vec![0u8; (tables_end - HEADER_LEN) as usize];
        file.read_exact(&mut tables)?;

        let mut blocks = Vec::with_capacity(block_count);
        for i in 0..block_count {
            let b = &tables[i * 16..(i + 1) * 16];
            let file_offset = u64::from_le_bytes(b[0..8].try_into().unwrap());
            let comp_len = u32::from_le_bytes(b[8..12].try_into().unwrap());
            let plain_len = u32::from_le_bytes(b[12..16].try_into().unwrap());
            file_offset
                .checked_add(comp_len as u64)
                .filter(|&e| e <= len)
                .ok_or_else(|| corrupt(format!("block {i} range out of bounds")))?;
            if file_offset < tables_end {
                return Err(corrupt(format!("block {i} overlaps tables")));
            }
            blocks.push(BlockMeta {
                file_offset,
                comp_len,
                plain_len,
            });
        }

        let slot_base = block_count * 16;
        let mut slots = Vec::with_capacity(slot_count);
        let mut prev: Option<(u16, u16, u16)> = None;
        for i in 0..slot_count {
            let s = &tables[slot_base + i * 16..slot_base + (i + 1) * 16];
            let x_rel = u16::from_le_bytes(s[0..2].try_into().unwrap());
            let z_rel = u16::from_le_bytes(s[2..4].try_into().unwrap());
            let type_id = u16::from_le_bytes(s[4..6].try_into().unwrap());
            let block = u16::from_le_bytes(s[6..8].try_into().unwrap());
            let offset_in_block = u32::from_le_bytes(s[8..12].try_into().unwrap());
            let plain_len = u32::from_le_bytes(s[12..16].try_into().unwrap());
            let bm = blocks
                .get(block as usize)
                .ok_or_else(|| corrupt(format!("slot {i} references missing block {block}")))?;
            (offset_in_block as u64)
                .checked_add(plain_len as u64)
                .filter(|&e| e <= bm.plain_len as u64)
                .ok_or_else(|| corrupt(format!("slot {i} range out of block bounds")))?;
            let key = (z_rel, x_rel, type_id);
            if x_rel > 31 || z_rel > 31 {
                return Err(corrupt(format!("slot {i} relative coordinate out of range")));
            }
            if prev.is_some_and(|p| key <= p) {
                return Err(corrupt(format!(
                    "slot table not strictly sorted at index {i}"
                )));
            }
            prev = Some(key);
            slots.push(SlotMeta {
                key,
                block,
                offset_in_block,
                plain_len,
            });
        }

        let inv_len = slot_count.div_ceil(8);
        let inv_path = path.with_extension("varc.inv");
        let invalid = if inv_path.exists() {
            let b = fs::read(&inv_path)?;
            if b.len() != inv_len {
                return Err(corrupt(format!(
                    "invalidation bitmap size {} does not match slot count {}",
                    b.len(),
                    slot_count
                )));
            }
            b
        } else {
            vec![0u8; inv_len]
        };

        Ok(Self {
            path: path.to_path_buf(),
            file,
            region_x,
            region_z,
            blocks,
            slots,
            invalid,
            cache: None,
        })
    }

    fn corrupt(&self, detail: String) -> StrataError {
        StrataError::Corrupt {
            path: self.path.display().to_string(),
            detail,
        }
    }

    /// Map an absolute chunk coordinate to a region-relative one (0..32).
    fn rel_coord(v: i32, region: i32) -> Option<u16> {
        let rel = (v as i64) - (region as i64) * 32;
        (0..32).contains(&rel).then_some(rel as u16)
    }

    fn find_slot(&self, x_rel: u16, z_rel: u16, type_id: u16) -> Option<usize> {
        let key = (z_rel, x_rel, type_id);
        self.slots.binary_search_by(|s| s.key.cmp(&key)).ok()
    }

    fn is_invalid(&self, idx: usize) -> bool {
        self.invalid
            .get(idx / 8)
            .is_some_and(|byte| byte & (1 << (idx % 8)) != 0)
    }

    /// Decompress `block_id`, reusing the one-block cache.
    fn block_data(&mut self, block_id: u16) -> Result<Vec<u8>, StrataError> {
        if let Some((id, data)) = self.cache.as_ref() {
            if *id == block_id {
                // Clone keeps the cache warm for the next lookup.
                return Ok(data.clone());
            }
        }
        let bm = &self.blocks[block_id as usize];
        let mut buf = vec![0u8; bm.comp_len as usize];
        self.file.seek(SeekFrom::Start(bm.file_offset))?;
        self.file.read_exact(&mut buf)?;
        let mut plain = Vec::new();
        zstd::stream::copy_decode(&buf[..], &mut plain).map_err(|e| {
            StrataError::Codec(format!(
                "cold block {block_id} zstd decode (dictionary-compressed blocks \
                 are not readable in Phase 1): {e}"
            ))
        })?;
        if plain.len() != bm.plain_len as usize {
            return Err(self.corrupt(format!(
                "block {block_id} decompressed to {} bytes, header says {}",
                plain.len(),
                bm.plain_len
            )));
        }
        self.cache = Some((block_id, plain.clone()));
        Ok(plain)
    }

    /// Look up the record at chunk `(x, z)` with `type_id` and return its NBT.
    ///
    /// Coordinates are absolute chunk coordinates; the archive's region origin
    /// is applied internally. Coordinates outside the region, missing slots,
    /// and invalidated slots all yield `Ok(None)`.
    pub fn get(&mut self, x: i32, z: i32, type_id: u16) -> Result<Option<Vec<u8>>, StrataError> {
        let (x_rel, z_rel) =
            match (Self::rel_coord(x, self.region_x), Self::rel_coord(z, self.region_z)) {
                (Some(a), Some(b)) => (a, b),
                _ => return Ok(None),
            };
        let idx = match self.find_slot(x_rel, z_rel, type_id) {
            Some(i) => i,
            None => return Ok(None),
        };
        if self.is_invalid(idx) {
            return Ok(None);
        }
        let (block, off, len) = {
            let s = &self.slots[idx];
            (s.block, s.offset_in_block as usize, s.plain_len as usize)
        };
        let plain = self.block_data(block)?;
        Ok(Some(plain[off..off + len].to_vec()))
    }

    /// Mark a slot invalid and persist the bitmap to `<archive>.varc.inv`.
    ///
    /// Returns `true` only the first time a slot is invalidated; repeated
    /// calls for the same (or missing) slot return `false` and do not bump
    /// [`invalid_count`](Self::invalid_count).
    pub fn invalidate(&mut self, x: i32, z: i32, type_id: u16) -> Result<bool, StrataError> {
        let (x_rel, z_rel) =
            match (Self::rel_coord(x, self.region_x), Self::rel_coord(z, self.region_z)) {
                (Some(a), Some(b)) => (a, b),
                _ => return Ok(false),
            };
        let idx = match self.find_slot(x_rel, z_rel, type_id) {
            Some(i) => i,
            None => return Ok(false),
        };
        if self.is_invalid(idx) {
            return Ok(false);
        }
        self.invalid[idx / 8] |= 1 << (idx % 8);
        fs::write(self.path.with_extension("varc.inv"), &self.invalid)?;
        Ok(true)
    }

    /// Number of invalidated slots (bits set in the bitmap).
    pub fn invalid_count(&self) -> u32 {
        self.invalid.iter().map(|b| b.count_ones()).sum()
    }

    /// Total number of slots in the archive.
    pub fn total_slots(&self) -> u32 {
        self.slots.len() as u32
    }

    /// Extract every entry (including invalidated ones) in stored order, i.e.
    /// superfeature order.
    pub fn extract_all(&mut self) -> Result<Vec<(Envelope, Vec<u8>)>, StrataError> {
        let mut out = Vec::with_capacity(self.slots.len());
        for block_id in 0..self.blocks.len() as u16 {
            let plain = self.block_data(block_id)?;
            let mut pos = 0usize;
            while pos + ENVELOPE_SIZE <= plain.len() {
                let env = Envelope::decode(&plain[pos..pos + ENVELOPE_SIZE].try_into().unwrap())?;
                pos += ENVELOPE_SIZE;
                let nbt_len = env.payload_len as usize;
                if pos + nbt_len > plain.len() {
                    return Err(self.corrupt(format!("block {block_id} entry overruns payload")));
                }
                out.push((env, plain[pos..pos + nbt_len].to_vec()));
                pos += nbt_len;
            }
            if pos != plain.len() {
                return Err(self.corrupt(format!(
                    "block {block_id} has {} trailing bytes",
                    plain.len() - pos
                )));
            }
        }
        Ok(out)
    }

    /// Largest decompressed block size, in bytes (0 for an empty archive).
    pub fn max_block_plain_bytes(&self) -> u64 {
        self.blocks
            .iter()
            .map(|b| b.plain_len as u64)
            .max()
            .unwrap_or(0)
    }
}
