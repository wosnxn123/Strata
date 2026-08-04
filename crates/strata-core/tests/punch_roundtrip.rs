//! 挖洞 × 扫描互斥回归（#11）：punch → rescan → verify → rebuild 往返。
//!
//! 不变量：挖洞只碰死记录负载、保留信封壳，因此任意长度的死区间被挖后，
//! 段仍可被 `scan_segment` / `verify` / rebuild（删 manifest 重开）正确处理。
//! 两个构造：>64KB 的中部死区间（超过重同步窗口）与覆盖文件尾的尾洞。

use strata_core::gc::GcConfig;
use strata_core::store::{Store, StoreConfig};

/// 不压缩 = 盘上负载长度与写入长度一致，死区间大小可精确控制。
fn cfg() -> StoreConfig {
    StoreConfig {
        hot_enabled: false,
        ..StoreConfig::default()
    }
}

/// 预算 0 → 压实永不搬迁：挖洞后的段必须留在原地接受扫描考验。
fn punch_only_gc() -> GcConfig {
    GcConfig {
        invalid_threshold: 0.4,
        budget_bytes: 0,
        min_hole_bytes: 64 * 1024,
    }
}

fn remove_manifest(dir: &std::path::Path) {
    let _ = std::fs::remove_file(dir.join("manifest.vsm"));
    let _ = std::fs::remove_file(dir.join("manifest.vsm.bak"));
}

#[test]
fn punch_over_64kb_dead_span_survives_rescan_verify_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    let scfg = cfg();
    let mut s = Store::open(dir.path(), scfg.clone()).unwrap();

    // 30 × 4KB = 120KB 记录；覆盖中间 20 条 → 80KB+ 连续死区间 > 64KB 重同步窗口。
    for i in 0..30i32 {
        s.write(i, 0, 0, &vec![i as u8; 4096]).unwrap();
    }
    s.flush().unwrap();
    for i in 5..25i32 {
        s.write(i, 0, 0, &vec![i as u8; 64]).unwrap();
    }
    s.flush().unwrap();

    // 挖洞是否真正生效取决于文件系统；数据完整性两种情况都必须成立。
    s.gc_pass(&punch_only_gc()).unwrap();

    // 全部 30 条数据可读且值正确。
    for i in 0..30i32 {
        let expect: Vec<u8> = if (5..25).contains(&i) {
            vec![i as u8; 64]
        } else {
            vec![i as u8; 4096]
        };
        assert_eq!(s.read(i, 0, 0).unwrap().unwrap(), expect, "key {i}");
    }

    // rescan + verify：洞后的段必须可扫描（壳保留 → 无需 MAGIC 重同步）。
    let rep = s.verify().unwrap();
    assert!(rep.records >= 30);

    // rebuild：删 manifest 重开，数据全部找回。
    drop(s);
    remove_manifest(dir.path());
    let s = Store::open(dir.path(), scfg).unwrap();
    for i in 0..30i32 {
        let expect: Vec<u8> = if (5..25).contains(&i) {
            vec![i as u8; 64]
        } else {
            vec![i as u8; 4096]
        };
        assert_eq!(s.read(i, 0, 0).unwrap().unwrap(), expect, "rebuild key {i}");
    }
    assert!(s.verify().is_ok());
}

#[test]
fn tail_dead_span_keeps_last_envelope_scannable() {
    let dir = tempfile::tempdir().unwrap();
    let scfg = cfg();
    let mut s = Store::open(dir.path(), scfg.clone()).unwrap();

    // A B 为存活记录；C D E（各 32KB）位于段尾且全部失效：
    // 死区间 [C.start, EOF) 覆盖最后一条记录的信封壳（尾洞构造）。
    s.write(0, 0, 0, &vec![0xAA; 16]).unwrap();
    s.write(1, 0, 0, &vec![0xBB; 16]).unwrap();
    for i in 2..5i32 {
        s.write(i, 0, 0, &vec![i as u8; 32 * 1024]).unwrap();
    }
    s.flush().unwrap();
    for i in 2..5i32 {
        s.write(i, 0, 0, &vec![i as u8; 64]).unwrap();
    }
    s.flush().unwrap();

    // 挖洞是否真正生效取决于文件系统；尾洞语义两种情况都必须成立。
    s.gc_pass(&punch_only_gc()).unwrap();

    assert_eq!(s.read(0, 0, 0).unwrap().unwrap(), vec![0xAA; 16]);
    assert_eq!(s.read(1, 0, 0).unwrap().unwrap(), vec![0xBB; 16]);
    for i in 2..5i32 {
        assert_eq!(s.read(i, 0, 0).unwrap().unwrap(), vec![i as u8; 64]);
    }

    // 尾洞不得让扫描器在 EOF 前丢失同步：verify 必须成功。
    let rep = s.verify().unwrap();
    assert!(rep.records >= 5);

    // rebuild 往返。
    drop(s);
    remove_manifest(dir.path());
    let s = Store::open(dir.path(), scfg).unwrap();
    assert_eq!(s.read(0, 0, 0).unwrap().unwrap(), vec![0xAA; 16]);
    assert_eq!(s.read(1, 0, 0).unwrap().unwrap(), vec![0xBB; 16]);
    for i in 2..5i32 {
        assert_eq!(s.read(i, 0, 0).unwrap().unwrap(), vec![i as u8; 64]);
    }
    assert!(s.verify().is_ok());
}
