//! 会话锁回归（#17）：同一 vstore 不允许多个会话同时打开。

use strata_core::store::{Store, StoreConfig};
use strata_core::sync_store::SyncStore;
use strata_core::StrataError;

#[test]
fn second_open_fails_with_holder_info_until_drop() {
    let dir = tempfile::tempdir().unwrap();
    let s1 = Store::open(dir.path(), StoreConfig::default()).unwrap();

    // 锁文件已写入持有者信息（从偏移 1 读：字节 0 是被锁区间，
    // Windows 持锁期间重叠读取会 ERROR_LOCK_VIOLATION）。
    let info = {
        use std::io::{Read, Seek};
        let mut f = std::fs::File::open(dir.path().join(".strata.lock")).unwrap();
        f.seek(std::io::SeekFrom::Start(1)).unwrap();
        let mut s = String::new();
        f.read_to_string(&mut s).unwrap();
        s
    };
    assert!(info.contains("pid="), "lock file should carry holder info: {info}");

    let err = match Store::open(dir.path(), StoreConfig::default()) {
        Err(e) => e,
        Ok(_) => panic!("second open must fail while the first session holds the lock"),
    };
    match err {
        StrataError::Lock(msg) => {
            assert!(msg.contains(".strata.lock"), "{msg}");
            assert!(msg.contains("pid="), "contention error should carry holder info: {msg}");
        }
        other => panic!("expected Lock error, got: {other:?}"),
    }

    // SyncStore 门面对同一把锁同样敏感。
    assert!(matches!(
        SyncStore::open(dir.path(), StoreConfig::default()),
        Err(StrataError::Lock(_))
    ));

    drop(s1);
    // 释放后可重新打开（drop 解锁并删除锁文件）。
    let s2 = Store::open(dir.path(), StoreConfig::default()).unwrap();
    s2.verify().unwrap();
    drop(s2);
    assert!(!dir.path().join(".strata.lock").exists());
}

#[test]
fn lock_released_between_sequential_opens() {
    let dir = tempfile::tempdir().unwrap();
    for _ in 0..3 {
        let mut s = Store::open(dir.path(), StoreConfig::default()).unwrap();
        s.write(0, 0, 0, b"round").unwrap();
        s.flush().unwrap();
    }
    let s = Store::open(dir.path(), StoreConfig::default()).unwrap();
    assert_eq!(s.read(0, 0, 0).unwrap().unwrap(), b"round");
}
