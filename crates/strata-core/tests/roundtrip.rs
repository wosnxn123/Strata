//! Store 门面往返/恢复集成测试。

use strata_core::store::{Store, StoreConfig};

#[test]
fn write_read_roundtrip_and_startup_without_scan() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut s = Store::open(dir.path(), StoreConfig::default()).unwrap();
        s.write(10, -10, 0, &[1, 2, 3]).unwrap();
        s.write(10, -10, 1, &[9, 9]).unwrap();
        s.flush().unwrap();
    }
    let s = Store::open(dir.path(), StoreConfig::default()).unwrap();
    assert_eq!(s.read(10, -10, 0).unwrap().unwrap(), vec![1, 2, 3]);
    assert_eq!(s.read(10, -10, 1).unwrap().unwrap(), vec![9, 9]);
    assert!(s.read(11, -10, 0).unwrap().is_none());
}

#[test]
fn latest_write_wins_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut s = Store::open(dir.path(), StoreConfig::default()).unwrap();
        s.write(0, 0, 0, b"old").unwrap();
        s.flush().unwrap();
        s.write(0, 0, 0, b"new").unwrap();
        s.flush().unwrap();
    }
    let s = Store::open(dir.path(), StoreConfig::default()).unwrap();
    assert_eq!(s.read(0, 0, 0).unwrap().unwrap(), b"new");
}

#[test]
fn crash_between_write_and_flush_recovers_via_epoch() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut s = Store::open(dir.path(), StoreConfig::default()).unwrap();
        s.write(1, 1, 0, b"committed").unwrap();
        s.flush().unwrap();
        s.write(2, 2, 0, b"inflight").unwrap();
        // 不 flush，直接 drop —— 模拟崩溃
    }
    let s = Store::open(dir.path(), StoreConfig::default()).unwrap();
    assert_eq!(s.read(1, 1, 0).unwrap().unwrap(), b"committed");
    assert_eq!(s.read(2, 2, 0).unwrap().unwrap(), b"inflight");
}

#[test]
fn corrupted_manifest_triggers_scan_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut s = Store::open(dir.path(), StoreConfig::default()).unwrap();
        s.write(3, 3, 0, b"data").unwrap();
        s.flush().unwrap();
    }
    std::fs::remove_file(dir.path().join("manifest.vsm")).unwrap();
    std::fs::remove_file(dir.path().join("manifest.vsm.bak")).unwrap();
    let s = Store::open(dir.path(), StoreConfig::default()).unwrap();
    assert_eq!(s.read(3, 3, 0).unwrap().unwrap(), b"data");
}

#[test]
fn verify_reports_clean_store() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::open(dir.path(), StoreConfig::default()).unwrap();
    s.write(0, 0, 0, b"x").unwrap();
    s.flush().unwrap();
    let r = s.verify().unwrap();
    assert_eq!(r.records, 1);
    assert!(r.corrupt_records.is_empty());
}
