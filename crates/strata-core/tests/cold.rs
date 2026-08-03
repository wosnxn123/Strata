//! Cold archive: block bound, lookup, invalidation, and superfeature ordering.

use strata_core::cold::{ArchiveBuilder, ArchiveReader, COLD_BLOCK_CHUNKS};
use strata_core::envelope::{Envelope, ENVELOPE_SIZE};

fn env(x: i32, z: i32, t: u16, len: u32) -> Envelope {
    Envelope {
        record_ver: 1,
        type_id: t,
        comp_id: 0,
        chunk_x: x,
        chunk_z: z,
        gen: (x as u64) * 1000 + z as u64,
        epoch_ts: 0,
        payload_len: len,
        payload_hash: 0,
    }
}

#[test]
fn archive_block_read_bound_and_invalidate() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("r.0.0.varc");
    let mut b = ArchiveBuilder::new(0, 0, 9, None);
    for x in 0..32i32 {
        for z in 0..32i32 {
            b.add(env(x, z, 0, 200), vec![(x + z * 32) as u8; 200]);
        }
    }
    let s = b.finish(&path).unwrap();
    // 1024 records, 64 per block.
    assert_eq!(
        s.blocks,
        (1024 + COLD_BLOCK_CHUNKS as u32 - 1) / COLD_BLOCK_CHUNKS as u32
    );
    assert_eq!(s.blocks, 16);
    assert_eq!(s.plain_bytes, 1024 * (ENVELOPE_SIZE as u64 + 200));

    let mut r = ArchiveReader::open(&path).unwrap();
    assert_eq!(r.total_slots(), 1024);
    assert_eq!(r.get(10, 0, 0).unwrap().unwrap(), vec![10u8; 200]);
    assert_eq!(r.get(0, 1, 0).unwrap().unwrap(), vec![32u8; 200]);
    assert!(r.get(10, 0, 1).unwrap().is_none()); // absent type_id
    assert!(r.get(99, 0, 0).unwrap().is_none()); // out of region -> no slot
    // Envelope bound: at most 64 entries of (40 envelope + 240 payload) bytes.
    assert!(
        r.max_block_plain_bytes()
            <= COLD_BLOCK_CHUNKS as u64 * 240 + COLD_BLOCK_CHUNKS as u64 * 40
    );

    assert!(r.invalidate(10, 0, 0).unwrap());
    assert_eq!(r.invalid_count(), 1);
    assert!(!r.invalidate(10, 0, 0).unwrap()); // repeat -> no double count
    assert_eq!(r.invalid_count(), 1);
    assert!(r.get(10, 0, 0).unwrap().is_none()); // invisible after invalidation

    // extract_all returns every record, invalidated ones included.
    let all = r.extract_all().unwrap();
    assert_eq!(all.len(), 1024);

    // Stored envelopes carry the uncompressed length and xxh64 hash; comp_id kept.
    let sample = all
        .iter()
        .find(|(e, _)| e.chunk_x == 10 && e.chunk_z == 0 && e.type_id == 0)
        .unwrap();
    assert_eq!(sample.0.payload_len, 200);
    assert_eq!(
        sample.0.payload_hash,
        xxhash_rust::xxh64::xxh64(&sample.1, 0)
    );
    assert_eq!(sample.0.comp_id, 0);
    assert_eq!(sample.1, vec![10u8; 200]);

    // The persisted bitmap survives a reopen and keeps the slot hidden.
    drop(r);
    let mut r2 = ArchiveReader::open(&path).unwrap();
    assert_eq!(r2.invalid_count(), 1);
    assert!(r2.get(10, 0, 0).unwrap().is_none());
    assert_eq!(r2.get(10, 0, 1).unwrap(), None); // still no phantom slot
}

/// 256 records: two families of 128. Each NBT = 2972-byte family-shared
/// prefix + 100-byte pseudo-random unique tail (3072 B = 96 windows of 32 B).
fn make_record(i: u16, phase: u8) -> (i32, i32, u16, Vec<u8>) {
    let x = (i % 32) as i32;
    let z = (i / 32) as i32;
    let unit: &[u8] = if phase == 0 {
        b"ALPHA-COLD-PREFIX-"
    } else {
        b"BRAVO-COLD-PREFIX-"
    };
    let mut nbt = Vec::with_capacity(3072);
    while nbt.len() + unit.len() <= 2972 {
        nbt.extend_from_slice(unit);
    }
    nbt.resize(2972, 0x5A);
    // Unique tail: deterministic LCG noise, incompressible, differs per record.
    let mut h = xxhash_rust::xxh64::xxh64(&nbt, i as u64 + phase as u64 * 7919 + 1);
    for _ in 0..100 {
        h = h
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        nbt.push((h >> 33) as u8);
    }
    (x, z, phase as u16, nbt)
}

#[test]
fn superfeatures_ordering_improves_compression() {
    let dir = tempfile::tempdir().unwrap();

    // Interleaved A,B,A,B record set; one builder inserts forward, one in
    // reverse. finish() re-sorts by superfeatures, so both archives must come
    // out identical regardless of insertion order.
    let set: Vec<(i32, i32, u16, Vec<u8>)> = (0..128u16)
        .flat_map(|i| [make_record(i, 0), make_record(i, 1)])
        .collect();

    let mut forward = ArchiveBuilder::new(0, 0, 9, None);
    for (x, z, t, nbt) in &set {
        forward.add(env(*x, *z, *t, nbt.len() as u32), nbt.clone());
    }
    let mut backward = ArchiveBuilder::new(0, 0, 9, None);
    for (x, z, t, nbt) in set.iter().rev() {
        backward.add(env(*x, *z, *t, nbt.len() as u32), nbt.clone());
    }
    let sf = forward.finish(&dir.path().join("f.varc")).unwrap();
    let sb = backward.finish(&dir.path().join("b.varc")).unwrap();
    assert_eq!(sf.blocks, sb.blocks);
    assert_eq!(sf.compressed_bytes, sb.compressed_bytes);
    assert_eq!(
        std::fs::read(dir.path().join("f.varc")).unwrap(),
        std::fs::read(dir.path().join("b.varc")).unwrap(),
        "superfeature sort must make the archive independent of insertion order"
    );

    // Unsorted baseline: same records in the shuffled (interleaved) order,
    // chunked one record per zstd frame and summed. Per-record chunking is
    // the worst case for zstd (no cross-record matches), and zstd's 128 KB
    // window makes a same-chunk-size interleaved comparison unstable, so this
    // baseline gives a large, deterministic gap.
    let mut baseline_bytes = 0u64;
    for (x, z, t, nbt) in &set {
        let mut e = env(*x, *z, *t, nbt.len() as u32);
        e.payload_hash = xxhash_rust::xxh64::xxh64(nbt, 0);
        let mut buf = [0u8; ENVELOPE_SIZE];
        e.encode(&mut buf);
        let mut unit = Vec::with_capacity(ENVELOPE_SIZE + nbt.len());
        unit.extend_from_slice(&buf);
        unit.extend_from_slice(nbt);
        let mut frame = Vec::new();
        zstd::stream::copy_encode(&unit[..], &mut frame, 9).unwrap();
        baseline_bytes += frame.len() as u64;
    }
    assert!(
        sf.compressed_bytes < baseline_bytes,
        "superfeature-sorted archive ({} B) must beat unsorted per-record \
         chunking ({} B)",
        sf.compressed_bytes,
        baseline_bytes
    );

    // Stored order is non-decreasing in the min superfeature: recompute the
    // 32-byte-window xxh64 minimum per extracted NBT and check the sequence.
    let mut rd = ArchiveReader::open(&dir.path().join("f.varc")).unwrap();
    let all = rd.extract_all().unwrap();
    assert_eq!(all.len(), 256);
    let mut prev_min = 0u64;
    for (_e, nbt) in &all {
        let mut min_h = u64::MAX;
        for win in nbt.chunks(32) {
            min_h = min_h.min(xxhash_rust::xxh64::xxh64(win, 0));
        }
        assert!(min_h >= prev_min, "superfeature min order regressed");
        prev_min = min_h;
    }
}
