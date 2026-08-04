//! 冷归档对账与分层崩溃窗口回归（#5 晋升顺序、#6 降级顺序、#12 对账、#21 判据）。

use strata_core::cold::ArchiveBuilder;
use strata_core::envelope::Envelope;
use strata_core::manifest::Manifest;
use strata_core::store::{Store, StoreConfig};
use strata_core::tier::TierConfig;
use xxhash_rust::xxh64::xxh64;

fn cfg() -> StoreConfig {
    StoreConfig::default()
}

fn fast_tier() -> TierConfig {
    TierConfig {
        enabled: true,
        stable_flushes: 2,
        invalid_demote_ratio: 0.25,
    }
}

fn env_for(x: i32, z: i32, nbt: &[u8]) -> Envelope {
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

/// 写满 region(0,0) 的 8 个 chunk 并推过稳定窗口。
fn fill_region(s: &mut Store) {
    for x in 0..4i32 {
        for z in 0..2i32 {
            s.write(x, z, 0, &vec![(x * 2 + z) as u8; 64]).unwrap();
        }
    }
    s.flush().unwrap();
    for _ in 0..3 {
        s.flush().unwrap();
    }
}

/// #12：未登记但可解析的 .varc（晋升在"文件落盘→登记"之间崩溃的残留）
/// 必须在 open 时重新注册；热层索引仍优先（重注册不遮蔽热数据）。
#[test]
fn unregistered_parseable_archive_is_reregistered_and_hot_wins() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut s = Store::open(dir.path(), cfg()).unwrap();
        s.write(0, 0, 0, b"hot-value").unwrap();
        s.write(1, 0, 0, b"hot-value-2").unwrap();
        s.flush().unwrap();
    }

    // 手工放一个未登记的合法归档（模拟晋升崩溃窗口）。
    let cold_dir = dir.path().join("cold");
    std::fs::create_dir_all(&cold_dir).unwrap();
    let mut b = ArchiveBuilder::new(0, 0, 9, None);
    b.add(env_for(0, 0, b"cold-stale"), b"cold-stale".to_vec());
    b.add(env_for(1, 0, b"cold-stale-2"), b"cold-stale-2".to_vec());
    b.finish(&cold_dir.join("r.0.0.varc")).unwrap();

    // 重开：对账注册冷区。
    let s = Store::open(dir.path(), cfg()).unwrap();
    let m = Manifest::load(dir.path()).unwrap().unwrap();
    assert!(m.cold.iter().any(|c| c.region_x == 0 && c.region_z == 0));
    // 热层索引在 → 热值胜出，冷副本只是兜底。
    assert_eq!(s.read(0, 0, 0).unwrap().unwrap(), b"hot-value");
    assert_eq!(s.read(1, 0, 0).unwrap().unwrap(), b"hot-value-2");
}

/// #12：未登记且不可解析的半截 .varc（写归档中途崩溃）必须被清除。
#[test]
fn unregistered_half_written_archive_is_removed() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut s = Store::open(dir.path(), cfg()).unwrap();
        s.write(0, 0, 0, b"x").unwrap();
        s.flush().unwrap();
    }
    let cold_dir = dir.path().join("cold");
    std::fs::create_dir_all(&cold_dir).unwrap();
    std::fs::write(cold_dir.join("r.3.4.varc"), b"VARC-garbage-truncated").unwrap();
    std::fs::write(cold_dir.join("r.3.4.varc.inv"), [0u8; 1]).unwrap();

    let s = Store::open(dir.path(), cfg()).unwrap();
    assert!(!cold_dir.join("r.3.4.varc").exists());
    assert!(!cold_dir.join("r.3.4.varc.inv").exists());
    assert_eq!(s.read(0, 0, 0).unwrap().unwrap(), b"x");
}

/// #6 崩溃窗口的可恢复终态：降级在"manifest 除名已持久化、文件未删除"处
/// 崩溃 → 重开时孤儿归档被对账重新注册，数据无损（热回写优先，冷兜底）。
#[test]
fn orphan_archive_after_demote_unregister_is_reconciled() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut s = Store::open(dir.path(), cfg()).unwrap();
        fill_region(&mut s);
        let stats = s.tier_pass(&fast_tier()).unwrap();
        assert_eq!(stats.promoted, 1);
        // 冷读工作正常。
        assert_eq!(s.read(0, 0, 0).unwrap().unwrap(), vec![0u8; 64]);
    }

    // 模拟崩溃终态：manifest 已除名（降级第 2 步已持久化），.varc 未删除。
    let mut m = Manifest::load(dir.path()).unwrap().unwrap();
    m.cold.retain(|c| !(c.region_x == 0 && c.region_z == 0));
    m.save(dir.path()).unwrap();
    assert!(dir.path().join("cold/r.0.0.varc").exists());

    // 重开：对账重新注册 → 冷读兜底恢复，数据无损。
    let s = Store::open(dir.path(), cfg()).unwrap();
    let m = Manifest::load(dir.path()).unwrap().unwrap();
    assert!(m.cold.iter().any(|c| c.region_x == 0 && c.region_z == 0));
    assert_eq!(s.read(3, 1, 0).unwrap().unwrap(), vec![7u8; 64]);
}

/// #21：降级判据必须用 `.varc.inv` 位图 popcount，而不是可能漂移的
/// manifest.invalid_count——这里把 manifest 计数抹成 0、位图拉满，
/// 若误用 manifest 计数则永不降级，测试失败。
#[test]
fn demote_ratio_uses_inv_popcount_not_manifest_counter() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut s = Store::open(dir.path(), cfg()).unwrap();
        fill_region(&mut s);
        let stats = s.tier_pass(&fast_tier()).unwrap();
        assert_eq!(stats.promoted, 1);
    }

    // manifest 计数说"没有失效"，位图说"全部失效"。
    let mut m = Manifest::load(dir.path()).unwrap().unwrap();
    for c in m.cold.iter_mut() {
        c.invalid_count = 0;
    }
    m.save(dir.path()).unwrap();
    let inv = dir.path().join("cold/r.0.0.varc.inv");
    std::fs::write(&inv, [0xFFu8; 1]).unwrap(); // 8 槽全部失效

    let mut s = Store::open(dir.path(), cfg()).unwrap();
    let stats = s.tier_pass(&fast_tier()).unwrap();
    assert_eq!(stats.demoted, 1, "demote must follow inv popcount");
    // 位图全失效 → 无槽位需要回写；归档与位图被删除。
    assert!(!dir.path().join("cold/r.0.0.varc").exists());
    assert!(!inv.exists());
}

/// #5/#6 端到端：晋升后改写触发失效，失效率达标后降级回热层，
/// 崩溃重开后依然收敛（数据全程无损）。
#[test]
fn promote_invalidate_demote_roundtrip_with_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut s = Store::open(dir.path(), cfg()).unwrap();
        fill_region(&mut s);
        assert_eq!(s.tier_pass(&fast_tier()).unwrap().promoted, 1);
        // 改写半数键 → 冷槽失效（热层新值）。
        for x in 0..4i32 {
            s.write(x, 0, 0, &vec![0xEE; 32]).unwrap();
        }
        s.flush().unwrap();
    }
    // 崩溃重开。
    let mut s = Store::open(dir.path(), cfg()).unwrap();
    // 改写键读热层新值；未改写键读冷归档。
    assert_eq!(s.read(2, 0, 0).unwrap().unwrap(), vec![0xEE; 32]);
    assert_eq!(s.read(2, 1, 0).unwrap().unwrap(), vec![5u8; 64]);
    // 失效占比 4/8 = 0.5 > 0.25 → 本轮降级。
    let stats = s.tier_pass(&fast_tier()).unwrap();
    assert_eq!(stats.demoted, 1);
    for x in 0..4i32 {
        assert_eq!(s.read(x, 0, 0).unwrap().unwrap(), vec![0xEE; 32]);
        assert_eq!(s.read(x, 1, 0).unwrap().unwrap(), vec![(x * 2 + 1) as u8; 64]);
    }
}
