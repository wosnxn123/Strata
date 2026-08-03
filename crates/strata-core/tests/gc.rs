//! 三档 GC 集成测试。

use strata_core::gc::GcConfig;
use strata_core::store::{Store, StoreConfig};

fn small_cfg() -> StoreConfig {
    StoreConfig {
        segment_max_bytes: 4096,
        ..StoreConfig::default()
    }
}

#[test]
fn gc_compacts_dead_records_and_preserves_live() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::open(dir.path(), small_cfg()).unwrap();
    for i in 0..20i32 {
        s.write(i, 0, 0, &[i as u8; 100]).unwrap();
    }
    for i in 0..18i32 {
        s.write(i, 0, 0, &[i as u8; 50]).unwrap();
    }
    s.flush().unwrap();
    let (live_before, total_before) = s.touch_stats();
    assert!(live_before < total_before);
    let stats = s
        .gc_pass(&GcConfig {
            invalid_threshold: 0.3,
            budget_bytes: u64::MAX,
            min_hole_bytes: 64 * 1024,
        })
        .unwrap();
    assert!(stats.reclaimed_bytes > 0);
    let (live_after, total_after) = s.touch_stats();
    assert_eq!(live_after, live_before);
    assert!(total_after < total_before);
    for i in 0..20i32 {
        let v = s.read(i, 0, 0).unwrap().unwrap();
        assert_eq!(v.len(), if i < 18 { 50 } else { 100 });
    }
}

#[test]
fn sparse_dead_spans_reclaimed_by_some_tier() {
    // 大段写 40 条（每条 2KB 负载，segment_max_bytes 足够大不滚动），
    // 覆盖中间 20 条使其失效；gc_pass 后数据全部完整可读。
    let dir = tempfile::tempdir().unwrap();
    let cfg = StoreConfig {
        segment_max_bytes: 1024 * 1024,
        ..StoreConfig::default()
    };
    let mut s = Store::open(dir.path(), cfg).unwrap();
    for i in 0..40i32 {
        s.write(i, 0, 0, &vec![i as u8; 2048]).unwrap();
    }
    s.flush().unwrap();
    for i in 10..30i32 {
        s.write(i, 0, 0, &vec![i as u8; 2048]).unwrap();
    }
    s.flush().unwrap();

    let (live_before, total_before) = s.touch_stats();
    assert!(live_before < total_before);

    let stats = s
        .gc_pass(&GcConfig {
            invalid_threshold: 0.3,
            budget_bytes: u64::MAX,
            min_hole_bytes: 64 * 1024,
        })
        .unwrap();
    assert!(stats.reclaimed_bytes > 0);
    let (_, total_after) = s.touch_stats();
    assert!(total_after < total_before);

    // 全部 40 条数据完整可读且值正确。
    for i in 0..40i32 {
        let v = s.read(i, 0, 0).unwrap().unwrap();
        assert_eq!(v.len(), 2048);
        assert!(v.iter().all(|&b| b == i as u8));
    }
}

#[test]
fn nearly_dead_segment_fully_removed() {
    // 不压缩 → 记录尺寸精确 = 40 + 180 = 220B。
    // 19 条 × 220B + 16B 头 = 4196 > 4096 → keys 0..=18 落在 seg-0001 后滚动。
    let dir = tempfile::tempdir().unwrap();
    let cfg = StoreConfig {
        hot_enabled: false,
        segment_max_bytes: 4096,
        ..StoreConfig::default()
    };
    let mut s = Store::open(dir.path(), cfg).unwrap();
    for i in 0..20i32 {
        s.write(i, 0, 0, &[i as u8; 180]).unwrap();
    }
    s.flush().unwrap();
    // 覆盖 keys 0..=18 → seg-0001 内记录 100% 失效（key 19 在 seg-0002）。
    for i in 0..19i32 {
        s.write(i, 0, 0, &[i as u8; 180]).unwrap();
    }
    // 再写一条新数据（落在滚动后的新段）。
    s.write(100, 0, 0, &[0xAA; 180]).unwrap();
    s.flush().unwrap();

    let stats = s
        .gc_pass(&GcConfig {
            invalid_threshold: 0.3,
            budget_bytes: u64::MAX,
            min_hole_bytes: 64 * 1024,
        })
        .unwrap();
    assert!(stats.segments_removed >= 1);

    // 100% 死的旧段文件已不存在。
    let old_seg = dir.path().join("segments").join("seg-0001.vseg");
    assert!(!old_seg.exists());

    // 新数据可读。
    assert_eq!(s.read(100, 0, 0).unwrap().unwrap(), vec![0xAA; 180]);
    for i in 0..19i32 {
        let v = s.read(i, 0, 0).unwrap().unwrap();
        assert_eq!(v.len(), 180);
    }
    // 唯一未被覆盖的记录也可读。
    let v = s.read(19, 0, 0).unwrap().unwrap();
    assert_eq!(v.len(), 180);
}
