//! 冷读并发回归（#1）：cold_readers 改为 Mutex<HashMap<_, Arc<Mutex<_>>>> 后，
//! 多线程经 SyncStore 读锁并发读同一冷归档必须正确且无竞态。

use std::sync::Arc;
use std::thread;

use strata_core::store::StoreConfig;
use strata_core::sync_store::SyncStore;
use strata_core::tier::TierConfig;

#[test]
fn concurrent_cold_reads_are_consistent() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(SyncStore::open(dir.path(), StoreConfig::default()).unwrap());

    // region(0,0)：32 × 2 = 64 个键，值 = 线性编号，便于并发断言。
    for x in 0..32i32 {
        for z in 0..2i32 {
            let v = (x * 2 + z) as u8;
            store.write(x, z, 0, &vec![v; 64]).unwrap();
        }
    }
    store.flush().unwrap();
    for _ in 0..3 {
        store.flush().unwrap(); // epoch 推过 stable_flushes
    }
    let stats = store
        .tier_pass(&TierConfig {
            enabled: true,
            stable_flushes: 2,
            invalid_demote_ratio: 0.25,
        })
        .unwrap();
    assert_eq!(stats.promoted, 1);
    // 晋升后热索引已 purge：read 走冷路径。
    assert_eq!(store.read(0, 0, 0).unwrap().unwrap(), vec![0u8; 64]);

    // 8 线程 × 1000 次并发冷读：任意线程读到错值即断言失败。
    let mut handles = Vec::new();
    for t in 0..8usize {
        let s = Arc::clone(&store);
        handles.push(thread::spawn(move || {
            for i in 0..1000usize {
                let x = ((t * 1000 + i) % 32) as i32;
                let z = ((t + i) % 2) as i32;
                let expect = (x * 2 + z) as u8;
                let got = s.read(x, z, 0).unwrap().expect("cold key present");
                assert_eq!(got, vec![expect; 64], "mismatch at ({x}, {z})");
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    // 并发读后单线程读仍然一致。
    assert_eq!(store.read(31, 1, 0).unwrap().unwrap(), vec![63u8; 64]);
}
