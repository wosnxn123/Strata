//! 冷归档读取的槽位信封校验回归（#20）。

use strata_core::cold::{ArchiveBuilder, ArchiveReader};
use strata_core::envelope::Envelope;
use strata_core::StrataError;
use xxhash_rust::xxh64::xxh64;

fn env(x: i32, z: i32, nbt: &[u8]) -> Envelope {
    Envelope {
        record_ver: 1,
        type_id: 0,
        comp_id: 0,
        chunk_x: x,
        chunk_z: z,
        gen: 1,
        epoch_ts: 0,
        payload_len: nbt.len() as u32,
        payload_hash: xxh64(nbt, 0),
    }
}

/// 篡改槽位表的 plain_len 使其与信封 payload_len 不符 → get 必须报 Corrupt，
/// 而不是静默返回截断/越界字节；未篡改的槽位不受影响。
#[test]
fn cold_get_rejects_slot_envelope_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("r.0.0.varc");

    let mut b = ArchiveBuilder::new(0, 0, 9, None);
    b.add(env(0, 0, &[7u8; 100]), vec![7u8; 100]);
    b.add(env(1, 0, &[8u8; 100]), vec![8u8; 100]);
    b.finish(&path).unwrap();

    // 头部布局：magic(4) | region_x(4) | region_z(4) | block_count u32 | slot_count u32；
    // 块表紧随其后（每条 16B），再后是槽位表（每条 16B，plain_len 在 +12）。
    let mut data = std::fs::read(&path).unwrap();
    let block_count = u32::from_le_bytes(data[12..16].try_into().unwrap()) as usize;
    let slot_base = 20 + block_count * 16;
    // 槽位表按 (z_rel, x_rel, type_id) 排序，(0,0,0) 是第一条。
    let plen_off = slot_base + 12;
    assert_eq!(
        u32::from_le_bytes(data[plen_off..plen_off + 4].try_into().unwrap()),
        100
    );
    data[plen_off] = 99; // payload_len 100 ≠ 99 → 信封校验必须失败
    std::fs::write(&path, &data).unwrap();

    let mut r = ArchiveReader::open(&path).unwrap();
    match r.get(0, 0, 0) {
        Err(StrataError::Corrupt { .. }) => {}
        other => panic!("expected Corrupt for tampered slot, got: {other:?}"),
    }
    // 未篡改槽位照常服务。
    assert_eq!(r.get(1, 0, 0).unwrap().unwrap(), vec![8u8; 100]);
}

/// 完整归档的正常读取不受新校验影响（信封与槽位天然一致）。
#[test]
fn cold_get_serves_intact_archive() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("r.0.0.varc");
    let mut b = ArchiveBuilder::new(0, 0, 9, None);
    for x in 0..4i32 {
        b.add(env(x, 0, &[x as u8; 50]), vec![x as u8; 50]);
    }
    b.finish(&path).unwrap();
    let mut r = ArchiveReader::open(&path).unwrap();
    for x in 0..4i32 {
        assert_eq!(r.get(x, 0, 0).unwrap().unwrap(), vec![x as u8; 50]);
    }
}
