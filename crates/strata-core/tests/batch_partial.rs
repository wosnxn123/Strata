//! write_batch 前缀提交语义回归（#19）：失败错误必须携带已提交条数。
//!
//! 故障注入方式：把下一个待分配段号的文件路径预置为**目录**——
//! `create_new` 撞目录必失败（root/CAP_DAC_OVERRIDE 环境同样生效，
//! 不依赖文件权限语义），孤儿探测的 `remove_file` 也删不掉目录。

use strata_core::store::{BatchItem, Store, StoreConfig};
use strata_core::StrataError;

/// 段滚动把写入器关掉后，下一条记录需要新建段文件；此时目标段路径是目录 →
/// alloc 失败。错误必须是 `BatchPartial` 且 `committed` 精确等于已落盘条数。
#[test]
fn batch_failure_carries_committed_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = StoreConfig {
        hot_enabled: false, // 负载长度 = 盘上长度，滚动时机可精确控制
        segment_max_bytes: 1000,
        ..StoreConfig::default()
    };
    let mut s = Store::open(dir.path(), cfg).unwrap();
    // 预热记录：offset 156 < 1000，写入器保持打开（seg-0001）。
    s.write(0, 0, 0, &[0u8; 100]).unwrap();

    // 注入：下一个段号（seg-0002）的路径预置为目录 → alloc 必败。
    std::fs::create_dir(dir.path().join("segments").join("seg-0002.vseg")).unwrap();

    // i1：offset 996，不滚动；i2：offset 1836 ≥ 1000，滚动关写入器；
    // i3：写入器空 → 新建段 → 撞目录 → 失败。committed 必须 = 2。
    let items: Vec<BatchItem> = (0..3)
        .map(|i| BatchItem {
            x: 10 + i,
            z: 0,
            type_id: 0,
            nbt: vec![i as u8; 800],
        })
        .collect();
    let err = s.write_batch(&items).unwrap_err();

    match err {
        StrataError::BatchPartial { committed, source } => {
            assert_eq!(committed, 2, "prefix commit count must be exact");
            assert!(
                !matches!(*source, StrataError::BatchPartial { .. }),
                "source should be the root cause, not another BatchPartial"
            );
        }
        other => panic!("expected BatchPartial, got: {other:?}"),
    }

    // 已提交前缀持久可读：重试时跳过前 committed 条即可续传。
    s.flush().unwrap();
    assert_eq!(s.read(10, 0, 0).unwrap().unwrap(), vec![0u8; 800]);
    assert_eq!(s.read(11, 0, 0).unwrap().unwrap(), vec![1u8; 800]);
    assert!(s.read(12, 0, 0).unwrap().is_none());
}

/// 首条记录即失败 → committed = 0 的同型错误（统一契约）。
#[test]
fn batch_failure_before_any_commit_reports_zero() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = StoreConfig {
        hot_enabled: false,
        ..StoreConfig::default()
    };
    let mut s = Store::open(dir.path(), cfg).unwrap();

    // 注入：首个段号（seg-0001）的路径预置为目录 → 首次 alloc 即败。
    std::fs::create_dir(dir.path().join("segments").join("seg-0001.vseg")).unwrap();

    let items = vec![BatchItem {
        x: 1,
        z: 1,
        type_id: 0,
        nbt: b"first".to_vec(),
    }];
    let err = s.write_batch(&items).unwrap_err();

    match err {
        StrataError::BatchPartial { committed, .. } => assert_eq!(committed, 0),
        other => panic!("expected BatchPartial, got: {other:?}"),
    }
}
