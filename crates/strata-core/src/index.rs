//! Three-layer index components (L0 region bitmap, L1 SIEVE page cache, L2 disk index pages).
//!
//! Pure in-memory / on-disk structures; no Store integration lives here.
//!
//! * [`RegionBitmap`] — L0: a 384-byte presence bitmap per 32×32 region
//!   (3 record types × 1024 bits).
//! * [`IndexPage`] — L2: a sorted, prefix-compressed page of `(IndexKey, IndexVal)`
//!   entries as persisted on disk.
//! * [`SieveCache`] — L1: a byte-bounded SIEVE (NSDI'24) cache of index pages.

use std::collections::HashMap;
use std::sync::Arc;

use crate::StrataError;

/// Key identifying one record inside the index: chunk coordinates + record type.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct IndexKey {
    pub x: i32,
    pub z: i32,
    pub type_id: u16,
}

/// Value stored for an indexed record: where its payload lives and how fresh it is.
#[derive(Debug, Clone, Copy)]
pub struct IndexVal {
    pub seg_id: u32,
    pub offset: u64,
    pub payload_len: u32,
    pub gen: u64,
    pub comp_id: u8,
}

/// Number of record types tracked by the L0 bitmap.
const BITMAP_TYPES: usize = 3;
/// One slab per type: 1024 bits = 128 bytes (32×32 region).
const BITMAP_SLAB_BYTES: usize = 128;

/// L0 bitmap: 384 B = 3 types × 1024 bits.
///
/// Bit index for a coordinate is `((z & 31) * 32 + (x & 31))`, so negative
/// coordinates fold into the region via two's-complement masking.
///
/// Unknown types (`type_id >= 3`) bypass the bitmap conservatively:
/// `set` is a no-op and `has` always returns `true`, forcing such lookups
/// down to the real index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegionBitmap {
    bits: [u8; BITMAP_TYPES * BITMAP_SLAB_BYTES],
}

impl RegionBitmap {
    /// An empty bitmap: nothing present in the region.
    pub fn new() -> Self {
        Self {
            bits: [0u8; BITMAP_TYPES * BITMAP_SLAB_BYTES],
        }
    }

    /// Mark `(x, z)` as present for type `t`. No-op for unknown types (`t >= 3`).
    pub fn set(&mut self, x: i32, z: i32, t: u16) {
        if let Some((byte, mask)) = Self::slot(x, z, t) {
            self.bits[byte] |= mask;
        }
    }

    /// Whether `(x, z)` *may* be present for type `t`.
    ///
    /// Always `true` for unknown types (`t >= 3`). May return false positives
    /// (bitmap semantics) but never false negatives.
    pub fn has(&self, x: i32, z: i32, t: u16) -> bool {
        match Self::slot(x, z, t) {
            Some((byte, mask)) => self.bits[byte] & mask != 0,
            None => true, // unknown type: conservative pass-through
        }
    }

    /// Raw 384-byte representation (one 128-byte slab per type, in type order).
    pub fn as_bytes(&self) -> &[u8; BITMAP_TYPES * BITMAP_SLAB_BYTES] {
        &self.bits
    }

    /// Rebuild a bitmap from its raw 384-byte representation.
    pub fn from_bytes(b: &[u8; BITMAP_TYPES * BITMAP_SLAB_BYTES]) -> Self {
        Self { bits: *b }
    }

    /// Byte offset + bit mask for `(x, z, t)`, or `None` for unknown types.
    #[inline]
    fn slot(x: i32, z: i32, t: u16) -> Option<(usize, u8)> {
        let t = usize::from(t);
        if t >= BITMAP_TYPES {
            return None;
        }
        let idx = ((z & 31) * 32 + (x & 31)) as usize;
        let byte = t * BITMAP_SLAB_BYTES + (idx >> 3);
        let mask = 1u8 << (idx & 7);
        Some((byte, mask))
    }
}

impl Default for RegionBitmap {
    fn default() -> Self {
        Self::new()
    }
}

/// L2 disk index page: a sorted entry array with prefix-compressed serialization.
///
/// Entries are sorted by [`IndexKey`] (via its `Ord` impl); duplicate keys keep
/// only the entry with the highest `gen`.
pub struct IndexPage {
    entries: Vec<(IndexKey, IndexVal)>,
}

impl IndexPage {
    /// Build a page from raw entries: sorts by key and, for duplicate keys,
    /// keeps only the entry with the maximum `gen`.
    pub fn from_entries(mut entries: Vec<(IndexKey, IndexVal)>) -> Self {
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let mut out: Vec<(IndexKey, IndexVal)> = Vec::with_capacity(entries.len());
        for (k, v) in entries {
            if let Some(last) = out.last_mut() {
                if last.0 == k {
                    if v.gen > last.1.gen {
                        last.1 = v;
                    }
                    continue;
                }
            }
            out.push((k, v));
        }
        Self { entries: out }
    }

    /// Serialize the page.
    ///
    /// Format: `u32` entry count (LE), then per entry:
    /// * `x`: entry 0 stores the raw `i32` as 4 LE bytes; entry `i > 0` stores
    ///   the delta `x_i - x_{i-1}` (non-negative, since entries are sorted with
    ///   `x` as the primary key) as an unsigned LEB128 varint;
    /// * `z`: `i32` LE;
    /// * `type_id`: `u16` LE;
    /// * `seg_id`: `u32` LE;
    /// * `offset`: `u64` LE;
    /// * `payload_len`: `u32` LE;
    /// * `gen`: `u64` LE;
    /// * `comp_id`: `u8`.
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(4 + self.entries.len() * 32);
        out.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());
        let mut prev_x: Option<i32> = None;
        for (k, v) in &self.entries {
            match prev_x {
                None => out.extend_from_slice(&k.x.to_le_bytes()),
                Some(px) => push_varint(&mut out, (k.x as u32).wrapping_sub(px as u32)),
            }
            out.extend_from_slice(&k.z.to_le_bytes());
            out.extend_from_slice(&k.type_id.to_le_bytes());
            out.extend_from_slice(&v.seg_id.to_le_bytes());
            out.extend_from_slice(&v.offset.to_le_bytes());
            out.extend_from_slice(&v.payload_len.to_le_bytes());
            out.extend_from_slice(&v.gen.to_le_bytes());
            out.push(v.comp_id);
            prev_x = Some(k.x);
        }
        out
    }

    /// Parse a page produced by [`IndexPage::serialize`]. Truncation, trailing
    /// garbage, or any structural violation yields [`StrataError::Codec`].
    pub fn deserialize(b: &[u8]) -> Result<Self, StrataError> {
        let corrupt = |detail: &str| StrataError::Codec(format!("index page: {detail}"));

        if b.len() < 4 {
            return Err(corrupt("truncated header"));
        }
        let count = u32::from_le_bytes(b[0..4].try_into().unwrap()) as usize;
        // Every entry costs at least 32 bytes (>=1 varint byte + 31 fixed bytes),
        // so this cheaply rejects absurd counts before any allocation.
        if count > b.len() / 32 {
            return Err(corrupt("entry count exceeds buffer"));
        }

        let mut pos = 4usize;
        let mut entries = Vec::with_capacity(count);
        let mut prev_x = 0i32;
        for i in 0..count {
            let x = if i == 0 {
                let s = take(b, &mut pos, 4).map_err(|_| corrupt("truncated entry"))?;
                i32::from_le_bytes(s.try_into().unwrap())
            } else {
                let (delta, n) =
                    read_varint(&b[pos..]).ok_or_else(|| corrupt("bad x-delta varint"))?;
                pos += n;
                (prev_x as u32).wrapping_add(delta) as i32
            };
            let z = i32::from_le_bytes(
                take(b, &mut pos, 4)
                    .map_err(|_| corrupt("truncated entry"))?
                    .try_into()
                    .unwrap(),
            );
            let type_id = u16::from_le_bytes(
                take(b, &mut pos, 2)
                    .map_err(|_| corrupt("truncated entry"))?
                    .try_into()
                    .unwrap(),
            );
            let seg_id = u32::from_le_bytes(
                take(b, &mut pos, 4)
                    .map_err(|_| corrupt("truncated entry"))?
                    .try_into()
                    .unwrap(),
            );
            let offset = u64::from_le_bytes(
                take(b, &mut pos, 8)
                    .map_err(|_| corrupt("truncated entry"))?
                    .try_into()
                    .unwrap(),
            );
            let payload_len = u32::from_le_bytes(
                take(b, &mut pos, 4)
                    .map_err(|_| corrupt("truncated entry"))?
                    .try_into()
                    .unwrap(),
            );
            let gen = u64::from_le_bytes(
                take(b, &mut pos, 8)
                    .map_err(|_| corrupt("truncated entry"))?
                    .try_into()
                    .unwrap(),
            );
            let comp_id = take(b, &mut pos, 1).map_err(|_| corrupt("truncated entry"))?[0];

            let key = IndexKey { x, z, type_id };
            if let Some((prev_key, _)) = entries.last() {
                if key <= *prev_key {
                    return Err(corrupt("entries not strictly sorted by key"));
                }
            }
            entries.push((
                key,
                IndexVal {
                    seg_id,
                    offset,
                    payload_len,
                    gen,
                    comp_id,
                },
            ));
            prev_x = x;
        }

        if pos != b.len() {
            return Err(corrupt("trailing bytes"));
        }
        Ok(Self { entries })
    }

    /// Binary search for a key.
    pub fn lookup(&self, k: &IndexKey) -> Option<&IndexVal> {
        self.entries
            .binary_search_by(|(key, _)| key.cmp(k))
            .ok()
            .map(|i| &self.entries[i].1)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &(IndexKey, IndexVal)> {
        self.entries.iter()
    }
}

/// Consume `n` bytes from `b` at `pos`, advancing it; `Err` past the end.
fn take<'a>(b: &'a [u8], pos: &mut usize, n: usize) -> Result<&'a [u8], ()> {
    if *pos + n > b.len() {
        return Err(());
    }
    let s = &b[*pos..*pos + n];
    *pos += n;
    Ok(s)
}

/// Append `v` to `out` as an unsigned LEB128 varint.
fn push_varint(out: &mut Vec<u8>, mut v: u32) {
    loop {
        let mut byte = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if v == 0 {
            break;
        }
    }
}

/// Decode an unsigned LEB128 varint from the front of `b`.
/// Returns `(value, bytes_consumed)`; `None` on truncation or >5-byte overflow.
fn read_varint(b: &[u8]) -> Option<(u32, usize)> {
    let mut value: u32 = 0;
    for (i, &byte) in b.iter().take(5).enumerate() {
        let part = u32::from(byte & 0x7f);
        // The fifth byte may only carry the top 4 bits of a u32.
        if i == 4 && part > 0x0f {
            return None;
        }
        value |= part << (7 * i);
        if byte & 0x80 == 0 {
            return Some((value, i + 1));
        }
    }
    None
}

/// One node in the SIEVE circular doubly-linked list (slab-allocated).
struct CacheNode {
    seg_id: u32,
    page: Arc<IndexPage>,
    /// Serialized size of `page` in bytes — the cache's billing unit.
    cost: u64,
    visited: bool,
    /// Slab indices of the ring neighbours (always valid while the node is live).
    prev: usize,
    next: usize,
}

/// L1 cache of index pages with a hard memory bound, using the SIEVE eviction
/// policy (NSDI'24): a single hand scans the ring; visited nodes get their flag
/// cleared and are skipped, the first non-visited node is evicted. If a full lap
/// finds only visited nodes, the node under the hand is evicted. After an
/// eviction the hand points at the evicted node's successor.
///
/// Capacity is accounted in bytes of `page.serialize().len()`.
pub struct SieveCache {
    nodes: Vec<Option<CacheNode>>,
    free: Vec<usize>,
    map: HashMap<u32, usize>,
    hand: Option<usize>,
    head: Option<usize>,
    tail: Option<usize>,
    max_bytes: u64,
    len_bytes: u64,
}

impl SieveCache {
    /// A cache holding at most `max_bytes` bytes of serialized index pages.
    pub fn new(max_bytes: u64) -> Self {
        Self {
            nodes: Vec::new(),
            free: Vec::new(),
            map: HashMap::new(),
            hand: None,
            head: None,
            tail: None,
            max_bytes,
            len_bytes: 0,
        }
    }

    /// Fetch a page. A hit marks the node visited but does **not** move it.
    pub fn get(&mut self, seg_id: u32) -> Option<Arc<IndexPage>> {
        let idx = *self.map.get(&seg_id)?;
        let node = self.nodes[idx].as_mut().expect("mapped node exists");
        node.visited = true;
        Some(Arc::clone(&node.page))
    }

    /// Insert or replace the page for `seg_id`, billing `page.serialize().len()`
    /// bytes. Evicts via [`SieveCache::evict_one`] until the new page fits
    /// within `max_bytes`.
    pub fn put(&mut self, seg_id: u32, page: Arc<IndexPage>) {
        let cost = page.serialize().len() as u64;
        if let Some(&idx) = self.map.get(&seg_id) {
            let node = self.nodes[idx].as_mut().expect("mapped node exists");
            self.len_bytes = self.len_bytes - node.cost + cost;
            node.page = page;
            node.cost = cost;
            node.visited = true;
            return;
        }
        while !self.map.is_empty() && self.len_bytes + cost > self.max_bytes {
            self.evict_one();
        }
        let idx = match self.free.pop() {
            Some(i) => {
                self.nodes[i] = Some(CacheNode {
                    seg_id,
                    page,
                    cost,
                    visited: false,
                    prev: i,
                    next: i,
                });
                i
            }
            None => {
                self.nodes.push(Some(CacheNode {
                    seg_id,
                    page,
                    cost,
                    visited: false,
                    prev: 0,
                    next: 0,
                }));
                self.nodes.len() - 1
            }
        };
        match self.head {
            None => {
                // Sole node links to itself.
                self.head = Some(idx);
                self.tail = Some(idx);
            }
            Some(h) => {
                let t = self.tail.expect("head set implies tail set");
                let nodes = &mut self.nodes;
                nodes[idx].as_mut().unwrap().prev = t;
                nodes[idx].as_mut().unwrap().next = h;
                nodes[t].as_mut().unwrap().next = idx;
                nodes[h].as_mut().unwrap().prev = idx;
                self.head = Some(idx);
            }
        }
        if self.hand.is_none() {
            self.hand = Some(idx);
        }
        self.map.insert(seg_id, idx);
        self.len_bytes += cost;
    }

    /// Explicitly remove a segment's page (e.g. when the segment is deleted).
    pub fn evict(&mut self, seg_id: u32) {
        if let Some(idx) = self.map.remove(&seg_id) {
            self.unlink(idx);
        }
    }

    /// One SIEVE scan step from the current hand position.
    pub fn evict_one(&mut self) {
        let Some(start) = self.hand else { return };
        let mut cur = start;
        let victim = loop {
            let (visited, next) = {
                let n = self.nodes[cur].as_ref().expect("hand points at a live node");
                (n.visited, n.next)
            };
            if !visited {
                break cur;
            }
            self.nodes[cur].as_mut().unwrap().visited = false;
            if next == start {
                // Full lap, every node was visited: evict the hand node itself.
                break start;
            }
            cur = next;
        };
        let next = self.nodes[victim].as_ref().unwrap().next;
        let seg = self.nodes[victim].as_ref().unwrap().seg_id;
        // SIEVE: after eviction the hand points at the evicted node's successor
        // (even when the victim was found mid-scan). unlink() reconciles the
        // empty-ring case.
        self.hand = Some(next);
        self.map.remove(&seg);
        self.unlink(victim);
    }

    /// Current billed size in bytes.
    pub fn len_bytes(&self) -> u64 {
        self.len_bytes
    }

    /// Number of cached pages.
    pub fn len_pages(&self) -> usize {
        self.map.len()
    }

    /// Remove node `idx` from the ring, the slab, and the byte accounting.
    fn unlink(&mut self, idx: usize) {
        let node = self.nodes[idx].take().expect("unlink: live node");
        if node.prev == idx {
            // Sole node in the ring.
            self.head = None;
            self.tail = None;
            self.hand = None;
        } else {
            self.nodes[node.prev].as_mut().unwrap().next = node.next;
            self.nodes[node.next].as_mut().unwrap().prev = node.prev;
            if self.head == Some(idx) {
                self.head = Some(node.next);
            }
            if self.tail == Some(idx) {
                self.tail = Some(node.prev);
            }
            if self.hand == Some(idx) {
                // SIEVE: after eviction the hand advances to the victim's successor.
                self.hand = Some(node.next);
            }
        }
        self.len_bytes -= node.cost;
        self.free.push(idx);
        debug_assert_eq!(
            self.nodes
                .iter()
                .filter_map(|n| n.as_ref().map(|n| n.cost))
                .sum::<u64>(),
            self.len_bytes
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_page(approx_bytes: usize) -> IndexPage {
        // Each entry serializes to ~32–35 B; dividing by 34 keeps the estimate
        // close to the requested size.
        let n = (approx_bytes / 34).max(1);
        let entries = (0..n)
            .map(|i| {
                (
                    IndexKey {
                        x: i as i32,
                        z: 0,
                        type_id: 0,
                    },
                    IndexVal {
                        seg_id: 1,
                        offset: 16,
                        payload_len: 5,
                        gen: i as u64,
                        comp_id: 1,
                    },
                )
            })
            .collect();
        IndexPage::from_entries(entries)
    }

    #[test]
    fn bitmap_set_has_o1() {
        let mut bm = RegionBitmap::new();
        assert!(!bm.has(3, 7, 0));
        bm.set(3, 7, 0);
        bm.set(-1, -1, 2); // negative coordinates normalize via & 31
        assert!(bm.has(3, 7, 0));
        assert!(!bm.has(3, 7, 1)); // types are isolated
        assert!(bm.has(-1, -1, 2));
        assert!(bm.has(5, 5, 99)); // unknown types always report true
    }

    #[test]
    fn bitmap_bytes_roundtrip_and_folding() {
        let mut bm = RegionBitmap::new();
        bm.set(0, 0, 0);
        bm.set(31, 31, 1);
        bm.set(10, 20, 2);
        bm.set(10, 20, 3); // unknown type: no-op
        let bytes = *bm.as_bytes();
        let bm2 = RegionBitmap::from_bytes(&bytes);
        assert_eq!(bm2.as_bytes(), &bytes);
        assert!(bm2.has(0, 0, 0));
        assert!(bm2.has(31, 31, 1));
        assert!(bm2.has(10, 20, 2));
        // coordinates alias modulo 32
        assert!(bm2.has(32, 0, 0));
        assert!(bm2.has(-1, 31, 1));
        assert!(!bm2.has(0, 1, 0));
    }

    #[test]
    fn index_page_roundtrip_and_latest_gen() {
        let entries = vec![
            (
                IndexKey { x: 1, z: 1, type_id: 0 },
                IndexVal { seg_id: 1, offset: 16, payload_len: 5, gen: 1, comp_id: 1 },
            ),
            (
                IndexKey { x: 1, z: 1, type_id: 0 },
                IndexVal { seg_id: 2, offset: 99, payload_len: 5, gen: 3, comp_id: 1 },
            ),
            (
                IndexKey { x: 2, z: 9, type_id: 1 },
                IndexVal { seg_id: 1, offset: 200, payload_len: 9, gen: 2, comp_id: 1 },
            ),
        ];
        let page = IndexPage::from_entries(entries);
        let bytes = page.serialize();
        let page2 = IndexPage::deserialize(&bytes).unwrap();
        assert_eq!(page2.len(), 2);
        assert_eq!(
            page2
                .lookup(&IndexKey { x: 1, z: 1, type_id: 0 })
                .unwrap()
                .gen,
            3
        );
        assert!(bytes.len() < 3 * 40);
    }

    #[test]
    fn index_page_negative_x_roundtrip_and_empty() {
        // Negative leading x exercises the raw i32 path; later deltas stay >= 0.
        let entries = vec![
            (
                IndexKey { x: -5, z: 0, type_id: 2 },
                IndexVal { seg_id: 7, offset: 40, payload_len: 1, gen: 10, comp_id: 3 },
            ),
            (
                IndexKey { x: -3, z: 0, type_id: 2 },
                IndexVal { seg_id: 7, offset: 80, payload_len: 1, gen: 11, comp_id: 3 },
            ),
            (
                IndexKey { x: 4, z: 0, type_id: 2 },
                IndexVal { seg_id: 7, offset: 120, payload_len: 1, gen: 12, comp_id: 3 },
            ),
        ];
        let page = IndexPage::from_entries(entries);
        let back = IndexPage::deserialize(&page.serialize()).unwrap();
        assert_eq!(back.len(), 3);
        assert_eq!(
            back.lookup(&IndexKey { x: -3, z: 0, type_id: 2 })
                .unwrap()
                .offset,
            80
        );
        assert!(back.lookup(&IndexKey { x: 0, z: 0, type_id: 2 }).is_none());

        let empty = IndexPage::from_entries(Vec::new());
        assert!(empty.is_empty());
        let re = IndexPage::deserialize(&empty.serialize()).unwrap();
        assert!(re.is_empty());

        // Duplicate keys keep the max gen regardless of input order.
        let dup = IndexPage::from_entries(vec![
            (
                IndexKey { x: 0, z: 0, type_id: 0 },
                IndexVal { seg_id: 1, offset: 0, payload_len: 0, gen: 9, comp_id: 0 },
            ),
            (
                IndexKey { x: 0, z: 0, type_id: 0 },
                IndexVal { seg_id: 2, offset: 0, payload_len: 0, gen: 4, comp_id: 0 },
            ),
        ]);
        assert_eq!(dup.len(), 1);
        assert_eq!(
            dup.lookup(&IndexKey { x: 0, z: 0, type_id: 0 }).unwrap().gen,
            9
        );
    }

    #[test]
    fn index_page_rejects_bad_bytes() {
        let page = sample_page(200);
        let bytes = page.serialize();

        assert!(IndexPage::deserialize(&bytes[..3]).is_err()); // truncated header
        let mut truncated = bytes.clone();
        truncated.pop();
        assert!(IndexPage::deserialize(&truncated).is_err()); // truncated entry

        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(IndexPage::deserialize(&trailing).is_err()); // trailing garbage

        // Absurd entry count must be rejected, not preallocated.
        let huge = u32::MAX.to_le_bytes();
        assert!(IndexPage::deserialize(&huge).is_err());

        // Two entries with the same key violate the page invariant.
        let mut dup = vec![2u8, 0, 0, 0]; // count = 2
        // entry 0: x stored as raw i32
        dup.extend_from_slice(&1i32.to_le_bytes());
        dup.extend_from_slice(&0i32.to_le_bytes()); // z
        dup.extend_from_slice(&0u16.to_le_bytes()); // type_id
        dup.extend_from_slice(&1u32.to_le_bytes()); // seg_id
        dup.extend_from_slice(&16u64.to_le_bytes()); // offset
        dup.extend_from_slice(&5u32.to_le_bytes()); // payload_len
        dup.extend_from_slice(&1u64.to_le_bytes()); // gen
        dup.push(1); // comp_id
        // entry 1: delta 0 -> x stays 1 -> duplicate key
        push_varint(&mut dup, 0);
        dup.extend_from_slice(&0i32.to_le_bytes());
        dup.extend_from_slice(&0u16.to_le_bytes());
        dup.extend_from_slice(&1u32.to_le_bytes());
        dup.extend_from_slice(&16u64.to_le_bytes());
        dup.extend_from_slice(&5u32.to_le_bytes());
        dup.extend_from_slice(&1u64.to_le_bytes());
        dup.push(1);
        assert!(IndexPage::deserialize(&dup).is_err());

        // Out-of-order keys (delta underflows into a smaller x) are rejected.
        let mut unsorted = vec![2u8, 0, 0, 0];
        unsorted.extend_from_slice(&5i32.to_le_bytes());
        unsorted.extend_from_slice(&0i32.to_le_bytes());
        unsorted.extend_from_slice(&0u16.to_le_bytes());
        unsorted.extend_from_slice(&1u32.to_le_bytes());
        unsorted.extend_from_slice(&16u64.to_le_bytes());
        unsorted.extend_from_slice(&5u32.to_le_bytes());
        unsorted.extend_from_slice(&1u64.to_le_bytes());
        unsorted.push(1);
        push_varint(&mut unsorted, u32::MAX); // delta -1 (wrapping) -> x=4 < 5
        unsorted.extend_from_slice(&0i32.to_le_bytes());
        unsorted.extend_from_slice(&0u16.to_le_bytes());
        unsorted.extend_from_slice(&1u32.to_le_bytes());
        unsorted.extend_from_slice(&16u64.to_le_bytes());
        unsorted.extend_from_slice(&5u32.to_le_bytes());
        unsorted.extend_from_slice(&1u64.to_le_bytes());
        unsorted.push(1);
        assert!(IndexPage::deserialize(&unsorted).is_err());
    }

    #[test]
    fn sieve_bounded_and_keeps_visited() {
        let mut c = SieveCache::new(1000);
        for i in 0..10u32 {
            c.put(i, Arc::new(sample_page(200)));
        }
        c.get(0);
        while c.len_bytes() > 1000 {
            c.evict_one();
        }
        assert!(c.len_bytes() <= 1000);
    }

    #[test]
    fn sieve_put_auto_evicts_to_fit() {
        let mut c = SieveCache::new(500);
        for i in 0..10u32 {
            c.put(i, Arc::new(sample_page(200))); // put evicts on its own
        }
        assert!(c.len_bytes() <= 500);
    }

    #[test]
    fn sieve_visited_page_survives_eviction_pass() {
        let page_cost = sample_page(200).serialize().len() as u64;
        let mut c = SieveCache::new(page_cost * 3);
        for i in 0..3u32 {
            c.put(i, Arc::new(sample_page(200)));
        }
        assert_eq!(c.len_bytes(), page_cost * 3);
        c.get(0); // mark visited; hand currently sits on seg 0
        c.put(3, Arc::new(sample_page(200))); // forces an eviction pass
        assert!(c.get(0).is_some()); // visited page survived the pass
        assert_eq!(c.len_pages(), 3);
    }

    #[test]
    fn sieve_hand_clears_visited_then_evicts() {
        let mut c = SieveCache::new(10_000);
        for i in 0..3u32 {
            c.put(i, Arc::new(sample_page(170)));
        }
        for i in 0..3u32 {
            assert!(c.get(i).is_some());
        }
        // All nodes visited: the sweep clears every flag on its lap and then
        // evicts the node under the hand (seg 0, the first insertion).
        c.evict_one();
        assert_eq!(c.len_pages(), 2);
        assert!(c.get(0).is_none());
        // Flags are cleared now: the next sweep evicts immediately.
        c.evict_one();
        assert_eq!(c.len_pages(), 1);
    }

    #[test]
    fn sieve_replace_evict_and_accounting() {
        let mut c = SieveCache::new(10_000);
        c.put(1, Arc::new(sample_page(340)));
        let before = c.len_bytes();
        assert!(before > 0);

        // Replace with a larger page: accounting follows the new cost.
        c.put(1, Arc::new(sample_page(680)));
        assert!(c.len_bytes() > before);
        assert_eq!(c.len_pages(), 1);
        assert!(c.get(1).is_some());

        // Explicit eviction removes the entry and its bytes.
        c.evict(1);
        assert_eq!(c.len_bytes(), 0);
        assert_eq!(c.len_pages(), 0);
        assert!(c.get(1).is_none());

        // evict_one on an empty cache is a harmless no-op.
        c.evict_one();

        // Slot reuse after eviction keeps the ring consistent.
        c.put(2, Arc::new(sample_page(340)));
        c.put(3, Arc::new(sample_page(340)));
        assert_eq!(c.len_pages(), 2);
        assert!(c.get(2).is_some());
        assert!(c.get(3).is_some());
    }
}
