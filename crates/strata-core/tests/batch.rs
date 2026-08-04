//! `Store::write_batch` 集成测试：数据完整性 + 并行压缩吞吐。

use std::time::Instant;

use strata_core::store::{BatchItem, Store, StoreConfig};

/// 固定种子 LCG（同序列跨平台可复现）。
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0
    }

    fn next_i32(&mut self) -> i32 {
        (self.next() >> 33) as i32
    }

    fn next_u16(&mut self) -> u16 {
        (self.next() >> 48) as u16
    }
}

/// 生成 NBT 风格负载：带重复字段头的可压缩字节（zstd 能实际压缩）。
fn nbt_payload(seed: u64, len: usize) -> Vec<u8> {
    let mut rng = Lcg(seed);
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        // 字段头（0x08 = TAG_String 风格）+ 少量随机值字节 → 重复模式占主导。
        out.push(0x08);
        out.push(0x00);
        out.push(0x07);
        let n = (rng.next_u16() % 4) as usize;
        for _ in 0..n {
            out.push(rng.next_u16() as u8);
        }
    }
    out.truncate(len);
    out
}

#[test]
fn batch_write_preserves_all() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::open(dir.path(), StoreConfig::default()).unwrap();

    let mut rng = Lcg(0xDEAD_BEEF_CAFE_BABE);
    let mut items: Vec<BatchItem> = Vec::with_capacity(8000);
    for _ in 0..8000 {
        items.push(BatchItem {
            x: rng.next_i32(),
            z: rng.next_i32(),
            type_id: rng.next_u16() % 2,
            nbt: nbt_payload(rng.next(), 200),
        });
    }

    let res = s.write_batch(&items).unwrap();
    assert_eq!(res.written, 8000);
    s.flush().unwrap();

    for it in &items {
        let got = s.read(it.x, it.z, it.type_id).unwrap();
        assert_eq!(
            got.as_deref(),
            Some(it.nbt.as_slice()),
            "mismatch at ({}, {}, {})",
            it.x,
            it.z,
            it.type_id
        );
    }
}

#[test]
fn batch_parallel_faster_than_serial() {
    // 注意：该断言在多核机器（CI 32 核）上依赖 rayon 并行压缩的显著收益；
    // 核数很少时并行收益可能退化，故用 `<=`（允许平局）而非严格 2x。
    let make_items = || -> Vec<BatchItem> {
        let mut rng = Lcg(0x5EED_5EED_5EED_5EED);
        (0..2000)
            .map(|_| BatchItem {
                x: rng.next_i32(),
                z: rng.next_i32(),
                type_id: rng.next_u16() % 2,
                nbt: nbt_payload(rng.next(), 8 * 1024),
            })
            .collect()
    };

    // 串行路径：逐条 write。
    let dir_serial = tempfile::tempdir().unwrap();
    let items_serial = make_items();
    let serial = {
        let mut s = Store::open(dir_serial.path(), StoreConfig::default()).unwrap();
        let t0 = Instant::now();
        for it in &items_serial {
            s.write(it.x, it.z, it.type_id, &it.nbt).unwrap();
        }
        s.flush().unwrap();
        t0.elapsed()
    };

    // 批量路径：write_batch（rayon 并行压缩 + 串行追加）。
    let dir_batch = tempfile::tempdir().unwrap();
    let items_batch = make_items();
    let batch = {
        let mut s = Store::open(dir_batch.path(), StoreConfig::default()).unwrap();
        let t0 = Instant::now();
        let res = s.write_batch(&items_batch).unwrap();
        s.flush().unwrap();
        assert_eq!(res.written, items_batch.len() as u64);
        t0.elapsed()
    };

    assert!(
        batch <= serial,
        "batch {batch:?} slower than serial {serial:?}"
    );
}

#[test]
fn empty_batch_is_noop() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::open(dir.path(), StoreConfig::default()).unwrap();
    let res = s.write_batch(&[]).unwrap();
    assert_eq!(res.written, 0);
    let r = s.verify().unwrap();
    assert_eq!(r.records, 0);
}
