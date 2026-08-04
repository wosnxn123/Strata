//! 崩溃一致性回归：next_gen 回卷、孤儿段认领、幽灵条目、压实崩溃窗口。
//!
//! 崩溃窗口统一用"手动构造中间磁盘状态 + 重新 open"模拟 kill。

use std::io::Write;

use strata_core::envelope::{Envelope, ENVELOPE_SIZE};
use strata_core::epoch::ENTRY_SIZE;
use strata_core::index::{IndexKey, IndexPage, IndexVal};
use strata_core::manifest::Manifest;
use strata_core::segment::{scan_segment, SegmentWriter};
use strata_core::store::{Store, StoreConfig};
use xxhash_rust::xxh64::xxh64;

fn env(x: i32, z: i32, type_id: u16, gen: u64, payload: &[u8]) -> Envelope {
    Envelope {
        record_ver: 1,
        type_id,
        comp_id: 0, // CODEC_NONE：负载原样存储
        chunk_x: x,
        chunk_z: z,
        gen,
        epoch_ts: 0,
        payload_len: payload.len() as u32,
        payload_hash: xxh64(payload, 0),
    }
}

/// 手工构造 64B epoch 条目：seg_id u32 | 40B 信封 | offset u64 | 12B 填充。
fn epoch_entry_bytes(seg_id: u32, env: &Envelope, offset: u64) -> Vec<u8> {
    let mut buf = vec![0u8; ENTRY_SIZE];
    buf[0..4].copy_from_slice(&seg_id.to_le_bytes());
    let mut env_buf = [0u8; ENVELOPE_SIZE];
    env.encode(&mut env_buf);
    buf[4..4 + ENVELOPE_SIZE].copy_from_slice(&env_buf);
    buf[4 + ENVELOPE_SIZE..4 + ENVELOPE_SIZE + 8].copy_from_slice(&offset.to_le_bytes());
    buf
}

fn append_epoch(dir: &std::path::Path, entry: &[u8]) {
    let mut log = std::fs::OpenOptions::new()
        .append(true)
        .open(dir.join("epoch/current.velog"))
        .unwrap();
    log.write_all(entry).unwrap();
    log.sync_all().unwrap();
}

/// #2：崩溃重开后 next_gen 必须与回放观察到的最大 gen 对齐。
/// 若回卷到 manifest 持久值（1），崩溃前未 flush 的 gen 1 被复用，
/// flush 合并同键同 gen 条目时保留先出现的旧值，最新写入被静默吞掉。
#[test]
fn next_gen_does_not_regress_after_crash() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut s = Store::open(dir.path(), StoreConfig::default()).unwrap();
        s.write(0, 0, 0, b"v1").unwrap();
        s.flush().unwrap(); // gen 0 持久化，manifest.next_gen = 1
        s.write(0, 0, 0, b"v2-crash").unwrap(); // gen 1 只在 epoch 日志
                                                  // 崩溃：不 flush 直接 drop
    }
    {
        let mut s = Store::open(dir.path(), StoreConfig::default()).unwrap();
        s.write(0, 0, 0, b"v3").unwrap();
        s.flush().unwrap();
    }
    let s = Store::open(dir.path(), StoreConfig::default()).unwrap();
    assert_eq!(s.read(0, 0, 0).unwrap().unwrap(), b"v3");
}

/// #3：manifest 未登记但文件存在、且被 epoch 条目引用的段，
/// 回放必须按段头认领而不是丢弃条目（旧代码静默丢数据）。
#[test]
fn replay_claims_orphan_segment_by_header() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut s = Store::open(dir.path(), StoreConfig::default()).unwrap();
        s.write(1, 1, 0, b"base").unwrap();
        s.flush().unwrap(); // seg 1 已登记，next_seg_id = 2
    }

    // 模拟旧代码崩溃窗口：段文件已创建并写了日志，manifest 未来得及登记。
    let payload = b"orphan-record".to_vec();
    let e = env(9, 9, 0, 77, &payload);
    let seg2 = dir.path().join("segments/seg-0002.vseg");
    let mut w = SegmentWriter::create(&seg2, 2).unwrap();
    let off = w.append(&e, &payload).unwrap();
    w.fsync().unwrap();
    w.close().unwrap();
    append_epoch(dir.path(), &epoch_entry_bytes(2, &e, off));

    let mut s = Store::open(dir.path(), StoreConfig::default()).unwrap();
    assert_eq!(s.read(1, 1, 0).unwrap().unwrap(), b"base");
    assert_eq!(s.read(9, 9, 0).unwrap().unwrap(), b"orphan-record");

    // next_gen 同步对齐（孤儿记录 gen=77）：新写必须覆盖孤儿值。
    s.write(9, 9, 0, b"shadow").unwrap();
    s.flush().unwrap();
    assert_eq!(s.read(9, 9, 0).unwrap().unwrap(), b"shadow");
}

/// #3 配套：文件缺失的 seg_id 条目丢弃，不影响其余回放与打开。
#[test]
fn replay_drops_entries_for_missing_segment_file() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut s = Store::open(dir.path(), StoreConfig::default()).unwrap();
        s.write(1, 1, 0, b"base").unwrap();
        s.flush().unwrap();
    }
    let e = env(8, 8, 0, 5, b"ghost");
    append_epoch(dir.path(), &epoch_entry_bytes(3, &e, 16)); // seg-0003 不存在

    let s = Store::open(dir.path(), StoreConfig::default()).unwrap();
    assert_eq!(s.read(1, 1, 0).unwrap().unwrap(), b"base");
    assert!(s.read(8, 8, 0).unwrap().is_none());
}

/// #13：offset 超出段文件当前长度的幽灵条目必须丢弃（防御残留），
/// 否则 read 会 seek 越界报错而不是隔离返回 None。
#[test]
fn replay_drops_entries_beyond_segment_length() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut s = Store::open(dir.path(), StoreConfig::default()).unwrap();
        s.write(1, 1, 0, b"base").unwrap();
        s.flush().unwrap();
    }
    let e = env(5, 5, 0, 5, &vec![0xAB; 100]);
    append_epoch(dir.path(), &epoch_entry_bytes(1, &e, 999_999));

    let s = Store::open(dir.path(), StoreConfig::default()).unwrap();
    assert_eq!(s.read(1, 1, 0).unwrap().unwrap(), b"base");
    assert!(s.read(5, 5, 0).unwrap().is_none());
    // 段文件本体完好，verify 可扫描。
    let rep = s.verify().unwrap();
    assert_eq!(rep.records, 1);
}

/// #4：压实崩溃窗口——新段数据与 .vix 已落盘但 manifest 未来得及换段。
/// 重开后旧段数据必须完好（搬迁记录此时不可见），孤儿新段被下次分配接管。
#[test]
fn crash_between_compact_vix_and_manifest_keeps_old_data() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut s = Store::open(dir.path(), StoreConfig::default()).unwrap();
        s.write(0, 0, 0, b"keep-a").unwrap();
        s.write(1, 0, 0, b"keep-b").unwrap();
        s.flush().unwrap(); // seg 1
    }

    // 手工构造"压实进行到一半"的磁盘状态：seg-0002 是搬迁副本，
    // .vix 已持久化，但 manifest 仍只认识 seg 1。
    let seg1 = dir.path().join("segments/seg-0001.vseg");
    let scan = scan_segment(&seg1).unwrap();
    let seg2 = dir.path().join("segments/seg-0002.vseg");
    let mut w = SegmentWriter::create(&seg2, 2).unwrap();
    let mut entries: Vec<(IndexKey, IndexVal)> = Vec::new();
    for rec in &scan.records {
        let off = w.append(&rec.env, &rec.payload).unwrap();
        entries.push((
            IndexKey {
                x: rec.env.chunk_x,
                z: rec.env.chunk_z,
                type_id: rec.env.type_id,
            },
            IndexVal {
                seg_id: 2,
                offset: off,
                payload_len: rec.env.payload_len,
                gen: rec.env.gen,
                comp_id: rec.env.comp_id,
            },
        ));
    }
    w.fsync().unwrap();
    w.close().unwrap();
    let page = IndexPage::from_entries(entries);
    std::fs::write(dir.path().join("segments/seg-0002.vix"), page.serialize()).unwrap();
    // 崩溃点：manifest.save（新段登记 + 旧段除名）尚未发生。
    assert!(Manifest::load(dir.path())
        .unwrap()
        .unwrap()
        .segments
        .iter()
        .all(|m| m.id != 2));

    // 重开：旧数据完好，孤儿新段不可见。
    {
        let mut s = Store::open(dir.path(), StoreConfig::default()).unwrap();
        assert_eq!(s.read(0, 0, 0).unwrap().unwrap(), b"keep-a");
        assert_eq!(s.read(1, 0, 0).unwrap().unwrap(), b"keep-b");

        // 下次写入分配到 seg 2：孤儿文件被接管重建，不与 create_new 冲突。
        s.write(2, 0, 0, b"after-crash").unwrap();
        s.flush().unwrap();
        assert_eq!(s.read(2, 0, 0).unwrap().unwrap(), b"after-crash");
        assert_eq!(s.read(0, 0, 0).unwrap().unwrap(), b"keep-a");
    }

    // 再次重开全部可读。
    let s = Store::open(dir.path(), StoreConfig::default()).unwrap();
    assert_eq!(s.read(0, 0, 0).unwrap().unwrap(), b"keep-a");
    assert_eq!(s.read(2, 0, 0).unwrap().unwrap(), b"after-crash");
}
