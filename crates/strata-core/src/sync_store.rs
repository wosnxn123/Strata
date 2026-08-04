//! 线程安全门面：`RwLock<Store>` 包装，供多线程宿主（含 FFI）共享。
//!
//! 锁粒度：
//! - 只读路径（`read`/`verify`/`touch_stats`）取读锁，多线程并发读不互斥；
//! - 写路径（`write`/`write_batch`/`flush`/`gc_pass`/`tier_pass`）取写锁，
//!   全程独占 `Store`，避免 `&self` 与 `&mut self` 路径交错触碰内部状态。
//!
//! `Store` 的 `RefCell`（cold_readers）本身阻碍自动 `Sync`，但被 `RwLock`
//! 包裹后，任何时刻至多一条写锁线程持有 `&mut Store`，读锁线程只拿 `&Store`
//! 且其借用不跨越写锁临界区——内部可变性被外层锁串行化，整体满足 `Send + Sync`
//! （`Store` 在 [`crate::store`] 中显式声明了 `Sync`，见该处 Safety 注释）。

use std::path::Path;
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::gc::{GcConfig, GcStats};
use crate::store::{BatchItem, BatchWriteResult, Store, StoreConfig, VerifyReport};
use crate::tier::{TierConfig, TierStats};
use crate::StrataError;

/// 线程安全的 vstore 门面。所有方法取 `&self`，可从任意线程并发调用。
pub struct SyncStore {
    inner: RwLock<Store>,
}

impl SyncStore {
    /// 打开（或创建）`root` 处的 vstore 并包裹为线程安全门面。
    pub fn open(root: &Path, cfg: StoreConfig) -> Result<Self, StrataError> {
        Ok(Self {
            inner: RwLock::new(Store::open(root, cfg)?),
        })
    }

    fn read_lock(&self) -> Result<RwLockReadGuard<'_, Store>, StrataError> {
        self.inner
            .read()
            .map_err(|_| StrataError::Manifest("store lock poisoned".into()))
    }

    fn write_lock(&self) -> Result<RwLockWriteGuard<'_, Store>, StrataError> {
        self.inner
            .write()
            .map_err(|_| StrataError::Manifest("store lock poisoned".into()))
    }

    /// 读取一条记录（读锁，多线程可并发）。
    pub fn read(&self, x: i32, z: i32, type_id: u16) -> Result<Option<Vec<u8>>, StrataError> {
        self.read_lock()?.read(x, z, type_id)
    }

    /// 写入一条记录（写锁）。
    pub fn write(&self, x: i32, z: i32, type_id: u16, nbt: &[u8]) -> Result<(), StrataError> {
        self.write_lock()?.write(x, z, type_id, nbt)
    }

    /// 批量写入：并行压缩 + 串行落盘（写锁）。
    pub fn write_batch(&self, items: &[BatchItem]) -> Result<BatchWriteResult, StrataError> {
        self.write_lock()?.write_batch(items)
    }

    /// 落盘（写锁）。
    pub fn flush(&self) -> Result<(), StrataError> {
        self.write_lock()?.flush()
    }

    /// 一次 GC（写锁）。
    pub fn gc_pass(&self, cfg: &GcConfig) -> Result<GcStats, StrataError> {
        self.write_lock()?.gc_pass(cfg)
    }

    /// 一次分层迁移（写锁）。
    pub fn tier_pass(&self, cfg: &TierConfig) -> Result<TierStats, StrataError> {
        self.write_lock()?.tier_pass(cfg)
    }

    /// 全量校验（读锁）。
    pub fn verify(&self) -> Result<VerifyReport, StrataError> {
        self.read_lock()?.verify()
    }

    /// `(live_bytes 总和, total_bytes 总和)`（读锁）。
    pub fn touch_stats(&self) -> (u64, u64) {
        // 统计读取不得因锁污染失败：退化到恢复守卫取回 Store。
        let guard = match self.inner.read() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        guard.touch_stats()
    }
}

#[cfg(test)]
mod tests {
    use super::SyncStore;
    use crate::store::{BatchItem, StoreConfig};
    use std::sync::Arc;
    use std::thread;

    fn open(dir: &std::path::Path) -> Arc<SyncStore> {
        Arc::new(SyncStore::open(dir, StoreConfig::default()).unwrap())
    }

    #[test]
    fn concurrent_read_write_threads() {
        let dir = tempfile::tempdir().unwrap();
        let store = open(dir.path());

        // 4 写线程 × 500 条，x 坐标按线程分段互不重叠。
        let mut writers = Vec::new();
        for t in 0..4i32 {
            let s = Arc::clone(&store);
            writers.push(thread::spawn(move || {
                for i in 0..500i32 {
                    let x = t * 10_000 + i;
                    s.write(x, t, 0, &[(t & 0xff) as u8, (i & 0xff) as u8, 0xAA])
                        .unwrap();
                }
            }));
        }

        // 2 读线程：循环读"已写键"，写者尚未写入时允许 None。
        let mut readers = Vec::new();
        for t in 0..2i32 {
            let s = Arc::clone(&store);
            readers.push(thread::spawn(move || {
                let mut hits = 0u64;
                for _ in 0..2_000 {
                    let x = (t * 2) * 10_000 + (hits % 500) as i32;
                    match s.read(x, t * 2, 0).unwrap() {
                        Some(v) => {
                            assert_eq!(v.len(), 3);
                            hits += 1;
                        }
                        None => {}
                    }
                }
                hits
            }));
        }

        for w in writers {
            w.join().unwrap();
        }
        for r in readers {
            r.join().unwrap();
        }

        // join 后全部数据可读。
        for t in 0..4i32 {
            for i in 0..500i32 {
                let x = t * 10_000 + i;
                let got = store.read(x, t, 0).unwrap().expect("record present");
                assert_eq!(got, vec![(t & 0xff) as u8, (i & 0xff) as u8, 0xAA]);
            }
        }
        store.flush().unwrap();
    }

    #[test]
    fn concurrent_readers() {
        let dir = tempfile::tempdir().unwrap();
        let store = open(dir.path());
        store.write(7, 7, 0, b"shared-value").unwrap();
        store.flush().unwrap();

        let mut handles = Vec::new();
        for _ in 0..8 {
            let s = Arc::clone(&store);
            handles.push(thread::spawn(move || {
                for _ in 0..1_000 {
                    assert_eq!(
                        s.read(7, 7, 0).unwrap().as_deref(),
                        Some(b"shared-value" as &[u8])
                    );
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn batch_through_facade() {
        let dir = tempfile::tempdir().unwrap();
        let store = open(dir.path());
        let items: Vec<BatchItem> = (0..64)
            .map(|i| BatchItem {
                x: i,
                z: -i,
                type_id: (i % 2) as u16,
                nbt: vec![i as u8; 16],
            })
            .collect();
        let res = store.write_batch(&items).unwrap();
        assert_eq!(res.written, 64);
        store.flush().unwrap();
        for it in &items {
            assert_eq!(
                store.read(it.x, it.z, it.type_id).unwrap().as_deref(),
                Some(it.nbt.as_slice())
            );
        }
    }
}
