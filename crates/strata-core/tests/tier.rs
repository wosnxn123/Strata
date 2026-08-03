//! 热↔冷迁移集成测试。

use strata_core::store::{Store, StoreConfig};
use strata_core::tier::{TierConfig, TierStats};

fn cfg() -> StoreConfig {
    StoreConfig::default()
}

fn tier() -> TierConfig {
    TierConfig {
        enabled: true,
        stable_flushes: 3,
        invalid_demote_ratio: 0.25,
    }
}

/// 写满 region r.0.0 的 64 个 chunk（x 0..32 × z 0..2，type 0）并推到稳定窗口外。
fn fill_region(store: &mut Store) {
    for x in 0..32i32 {
        for z in 0..2i32 {
            store
                .write(x, z, 0, &vec![(x + z * 32) as u8; 64])
                .unwrap();
        }
    }
    store.flush().unwrap();
    for _ in 0..4 {
        store.flush().unwrap(); // epoch 推进超过 stable_flushes
    }
}

#[test]
fn stable_region_promotes_to_cold_and_reads_back() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::open(dir.path(), cfg()).unwrap();
    fill_region(&mut s);

    let stats: TierStats = s.tier_pass(&tier()).unwrap();
    assert_eq!(stats.promoted, 1);
    assert_eq!(stats.demoted, 0);
    assert!(stats.bytes_cold > 0);
    assert!(dir.path().join("cold/r.0.0.varc").exists());

    // 冷读透明：热索引已移除，read 走冷路径。
    assert_eq!(s.read(5, 0, 0).unwrap().unwrap(), vec![5u8; 64]);
    assert_eq!(s.read(31, 1, 0).unwrap().unwrap(), vec![63u8; 64]);

    // 改写已冷 chunk → 回填热层，热值胜出。
    s.write(5, 0, 0, b"rewritten").unwrap();
    assert_eq!(s.read(5, 0, 0).unwrap().unwrap(), b"rewritten");
    // 同 region 未改写的键仍从冷读。
    assert_eq!(s.read(6, 0, 0).unwrap().unwrap(), vec![6u8; 64]);
}

#[test]
fn heavy_invalidation_demotes_archive() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::open(dir.path(), cfg()).unwrap();
    fill_region(&mut s);
    assert_eq!(s.tier_pass(&tier()).unwrap().promoted, 1);

    // 改写 region 内 >25% 的 chunk（17/64 = 0.2656 > 0.25）。
    for x in 0..17i32 {
        s.write(x, 0, 0, &vec![200u8; 32]).unwrap();
    }

    let stats = s.tier_pass(&tier()).unwrap();
    assert_eq!(stats.demoted, 1);
    assert_eq!(stats.promoted, 0);
    assert!(!dir.path().join("cold/r.0.0.varc").exists());
    assert!(!dir.path().join("cold/r.0.0.varc.inv").exists());

    // 全部 64 条数据热读正确：被改写的 17 条是新值，其余是原值。
    for x in 0..32i32 {
        for z in 0..2i32 {
            let got = s.read(x, z, 0).unwrap().unwrap();
            if z == 0 && x < 17 {
                assert_eq!(got, vec![200u8; 32], "key ({x},{z}) should be rewritten");
            } else {
                assert_eq!(
                    got,
                    vec![(x + z * 32) as u8; 64],
                    "key ({x},{z}) should be restored"
                );
            }
        }
    }
}

#[test]
fn tiering_disabled_never_promotes() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::open(dir.path(), cfg()).unwrap();
    fill_region(&mut s);

    let off = TierConfig {
        enabled: false,
        stable_flushes: 3,
        invalid_demote_ratio: 0.25,
    };
    let stats = s.tier_pass(&off).unwrap();
    assert_eq!(stats.promoted, 0);
    assert_eq!(stats.demoted, 0);
    assert_eq!(stats.bytes_cold, 0);
    assert!(!dir.path().join("cold/r.0.0.varc").exists());
    // 纯热模式：数据照常热读。
    assert_eq!(s.read(5, 0, 0).unwrap().unwrap(), vec![5u8; 64]);
}

#[test]
fn reopen_preserves_cold_reads() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::open(dir.path(), cfg()).unwrap();
    fill_region(&mut s);
    assert_eq!(s.tier_pass(&tier()).unwrap().promoted, 1);
    drop(s);

    // 重新打开：cold_readers 为空，冷路径懒加载。
    let s = Store::open(dir.path(), cfg()).unwrap();
    assert_eq!(s.read(5, 0, 0).unwrap().unwrap(), vec![5u8; 64]);
    assert_eq!(s.read(0, 1, 0).unwrap().unwrap(), vec![32u8; 64]);
    // 热层无该 region 残留：改写后热值胜出、冷槽失效。
    drop(s);
    let mut s = Store::open(dir.path(), cfg()).unwrap();
    s.write(5, 0, 0, b"post-reopen").unwrap();
    assert_eq!(s.read(5, 0, 0).unwrap().unwrap(), b"post-reopen");
    assert_eq!(s.read(4, 0, 0).unwrap().unwrap(), vec![4u8; 64]);
}
