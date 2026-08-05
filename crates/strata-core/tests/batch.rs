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
    // 批量路径以 compression_threads=0 显式 opt-in 全核并行（默认串行为主）。
    // 正确性（逐条读回）与核数无关，恒验证；速度断言仅在 ≥4 核机器上有
    // 意义——GitHub 标准 runner 只有 2 vCPU，线程调度开销会吃掉并行收益，
    // 故核数不足时跳过速度断言（32 核基准机仍会断言，见 benches/RESULTS.md）。
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

    // 批量路径：write_batch（有界线程并行压缩 + 串行追加，显式 opt-in）。
    let dir_batch = tempfile::tempdir().unwrap();
    let items_batch = make_items();
    let batch = {
        let cfg = StoreConfig {
            compression_threads: 0,
            ..StoreConfig::default()
        };
        let mut s = Store::open(dir_batch.path(), cfg).unwrap();
        let t0 = Instant::now();
        let res = s.write_batch(&items_batch).unwrap();
        s.flush().unwrap();
        assert_eq!(res.written, items_batch.len() as u64);
        t0.elapsed()
    };

    println!(
        "parallel_compression: serial={:?} batch={:?} speedup={:.2}x",
        serial,
        batch,
        serial.as_secs_f64() / batch.as_secs_f64().max(f64::EPSILON)
    );

    // 正确性抽查（与核数无关）：批量写入的数据可逐条读回。
    {
        let s = Store::open(dir_batch.path(), StoreConfig::default()).unwrap();
        for it in items_batch.iter().step_by(500) {
            assert_eq!(s.read(it.x, it.z, it.type_id).unwrap().unwrap(), it.nbt);
        }
    }

    // 速度断言仅在核数足够时有意义：核太少时（如 GitHub windows-latest
    // 2 vCPU），线程调度开销可吃掉并行收益。
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    if cores < 4 {
        eprintln!("batch_parallel: only {cores} core(s), skipping speed assertion");
        return;
    }
    assert!(
        batch <= serial,
        "batch {batch:?} slower than serial {serial:?} on {cores} cores"
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

/// 压缩线程数开关的往返测试：`threads=1`（串行，默认）与 `threads=3`
/// （有界并行）各写一批，验证记录数且全部可读回。
#[test]
fn batch_threads_roundtrip() {
    for threads in [1u32, 3] {
        let dir = tempfile::tempdir().unwrap();
        let cfg = StoreConfig {
            compression_threads: threads,
            ..StoreConfig::default()
        };
        let mut s = Store::open(dir.path(), cfg).unwrap();

        let mut rng = Lcg(0x00C0_FFEE_0DD5 + u64::from(threads));
        let items: Vec<BatchItem> = (0..512)
            .map(|_| BatchItem {
                x: rng.next_i32(),
                z: rng.next_i32(),
                type_id: rng.next_u16() % 2,
                nbt: nbt_payload(rng.next(), 1024),
            })
            .collect();

        let res = s.write_batch(&items).unwrap();
        assert_eq!(res.written, items.len() as u64, "threads={threads}");
        s.flush().unwrap();

        for it in &items {
            let got = s.read(it.x, it.z, it.type_id).unwrap();
            assert_eq!(
                got.as_deref(),
                Some(it.nbt.as_slice()),
                "threads={threads} mismatch at ({}, {}, {})",
                it.x,
                it.z,
                it.type_id
            );
        }
    }
}
