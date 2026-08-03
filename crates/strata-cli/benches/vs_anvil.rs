//! Strata vs Anvil 基准套件（Phase 1 Wave E）。
//!
//! 四组基准：
//! 1. `bench_footprint` — 合成世界（4 region / 4096 chunk）Anvil → Strata
//!    转换 + compact 后的磁盘占用对比，断言 vault/anvil ≤ 0.65。
//!    转换太慢不适合重复采样，全流程只跑一次，criterion 仅挂一个空转测点。
//! 2. `bench_write_throughput` — 空 Store 上 10k 次随机 write + 末尾 flush。
//! 3. `bench_read_latency` — 预写 1k 条记录后，每样本 1k 次随机 read，
//!    另单次采样逐条计时并打印 p50/p99。
//! 4. `bench_memory_bound` — SieveCache 64 MiB 字节预算上界断言
//!    （RSS 采样留给实机基准，见函数内注释）。
//!
//! 运行：`cargo bench --bench vs_anvil`。

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use criterion::{black_box, criterion_group, criterion_main, BatchSize, Criterion};
use tempfile::TempDir;

use strata_cli::anvil::{write_region, ChunkLoc};
use strata_core::index::{IndexKey, IndexPage, IndexVal, SieveCache};
use strata_core::store::{Store, StoreConfig};

/// 固定种子 LCG（Knuth MMIX 常数）— 保证基准数据可复现。
fn lcg(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    *state
}

/// 递归累计目录字节数（文件长度之和；不存在 → 0）。
fn dir_bytes(path: &Path) -> u64 {
    let Ok(rd) = fs::read_dir(path) else {
        return 0;
    };
    let mut total = 0u64;
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            total += dir_bytes(&path);
        } else if let Ok(meta) = entry.metadata() {
            total += meta.len();
        }
    }
    total
}

/// 生物群系模板前缀长度：50% 的 chunk 共享该前缀（模拟重复 NBT 模式）。
const BIOME_TEMPLATE_LEN: usize = 128;

/// 合成世界：4 个 region（r.0.0 – r.1.1，共 4096 chunk），每 chunk
/// NBT 长度 200–800 B（固定种子 LCG），50% chunk 共享同一生物群系模板前缀。
/// 写入 `<world>/region/r.X.Z.mca`，返回 region/ 目录总字节数（Anvil 占用）。
fn synth_world(world: &Path, seed: u64) -> u64 {
    let region_dir = world.join("region");
    fs::create_dir_all(&region_dir).expect("create region dir");

    let biome_template: Vec<u8> = (0..BIOME_TEMPLATE_LEN)
        .map(|i| i.wrapping_mul(31).wrapping_add(7) as u8)
        .collect();

    let mut state = seed;
    for rx in 0i32..2 {
        for rz in 0i32..2 {
            let mut chunks = Vec::with_capacity(1024);
            for z in 0u8..32 {
                for x in 0u8..32 {
                    let len = 200 + (lcg(&mut state) % 601) as usize; // 200..=800
                    let shared = lcg(&mut state) & 1 == 0; // 50% 共享生物群系模板
                    let mut nbt = Vec::with_capacity(len);
                    if shared {
                        nbt.extend_from_slice(&biome_template);
                    }
                    while nbt.len() < len {
                        nbt.push((lcg(&mut state) & 0xFF) as u8);
                    }
                    chunks.push(ChunkLoc {
                        x,
                        z,
                        nbt,
                        timestamp: (lcg(&mut state) & 0xFFFF_FFFF) as u32,
                    });
                }
            }
            write_region(&region_dir.join(format!("r.{rx}.{rz}.mca")), &chunks)
                .expect("write region");
        }
    }
    dir_bytes(&region_dir)
}

/// 磁盘占用对比：合成世界 → `convert --to-strata` → `compact`，
/// 断言 vault/anvil ≤ 0.65。转换 + compact 太慢不适合重复采样，
/// 全流程恰好执行一次；体积数字经 println 输出，criterion 只保留
/// 一个空转测点以维持 harness 输出。
fn bench_footprint(c: &mut Criterion) {
    let tmp = TempDir::new().expect("tempdir");
    let world = tmp.path();
    let world_str = world.to_str().expect("utf8 world path").to_owned();

    let anvil_bytes = synth_world(world, 0x5EED_2026);

    strata_cli::run(&["convert", "--to-strata", &world_str]).expect("convert to strata");
    strata_cli::run(&["compact", &world_str]).expect("compact");

    let vault_bytes = dir_bytes(&world.join("vstore"));
    let ratio = vault_bytes as f64 / anvil_bytes as f64;
    println!("footprint: anvil_bytes = {anvil_bytes}");
    println!("footprint: vault_bytes = {vault_bytes}");
    println!("footprint: vault/anvil  = {ratio:.4}");
    assert!(
        vault_bytes as f64 / anvil_bytes as f64 <= 0.65,
        "vault/anvil ratio {ratio:.4} exceeds budget 0.65"
    );

    // 单次测量的 criterion 测点（空转）：管线耗时以上面的输出为准。
    c.bench_function("footprint_setup", |b| b.iter(|| {}));
}

/// 写吞吐：每样本新建 Store（全新 tempdir），10k 次随机 write
/// （x/z ∈ 0..256、type 0、固定 256 B payload）+ 末尾 flush。
fn bench_write_throughput(c: &mut Criterion) {
    let payload = vec![0xA5u8; 256];
    let keys: Vec<(i32, i32)> = {
        let mut s = 0xC0FF_EE00u64;
        (0..10_000)
            .map(|_| ((lcg(&mut s) % 256) as i32, (lcg(&mut s) % 256) as i32))
            .collect()
    };

    c.bench_function("write_throughput_10k", |b| {
        b.iter_batched(
            || {
                let tmp = TempDir::new().expect("tempdir");
                let store = Store::open(&tmp.path().join("vstore"), StoreConfig::default())
                    .expect("open store");
                // tmp 放第二位：元组按声明序 drop，store 先于 TempDir 释放。
                (store, tmp)
            },
            |(mut store, _tmp)| {
                for &(x, z) in &keys {
                    store.write(x, z, 0, &payload).expect("write");
                }
                store.flush().expect("flush");
            },
            BatchSize::PerIteration,
        )
    });
}

/// 读延迟：预写 1k 条随机记录 + flush，随后每样本 1k 次随机 read
/// （criterion 自带分布统计）；另单次采样逐条计时，打印 p50/p99。
fn bench_read_latency(c: &mut Criterion) {
    let tmp = TempDir::new().expect("tempdir");
    let mut store = Store::open(&tmp.path().join("vstore"), StoreConfig::default())
        .expect("open store");

    let payload = vec![0x3Cu8; 256];
    let mut s = 0xBAD_5EEDu64;
    let mut coords = Vec::with_capacity(1000);
    for _ in 0..1000 {
        let x = (lcg(&mut s) % 256) as i32;
        let z = (lcg(&mut s) % 256) as i32;
        store.write(x, z, 0, &payload).expect("write");
        coords.push((x, z));
    }
    store.flush().expect("flush");

    c.bench_function("read_latency_1k", |b| {
        b.iter(|| {
            for &(x, z) in &coords {
                black_box(store.read(x, z, 0).expect("read"));
            }
        })
    });

    // 单次采样逐条 read 计时 → p50/p99。
    let mut times: Vec<_> = coords
        .iter()
        .map(|&(x, z)| {
            let t0 = Instant::now();
            black_box(store.read(x, z, 0).expect("read"));
            t0.elapsed()
        })
        .collect();
    times.sort();
    println!("read_latency: p50 = {:?}", times[times.len() / 2]);
    println!("read_latency: p99 = {:?}", times[(times.len() * 99) / 100]);
}

/// SieveCache 内存上界：64 MiB 字节预算，插入 10 万条目
/// （100 页 × 1000 条，用 `IndexPage::from_entries` 构造），
/// 断言计账的 `len_bytes()` 不超预算。
///
/// 注意：进程 RSS 采样（含分配器开销、碎片）留给实机基准；
/// 此组只断言缓存自身计账上界，不依赖进程 RSS。
fn bench_memory_bound(c: &mut Criterion) {
    const BUDGET: u64 = 64 * 1024 * 1024;
    const PAGES: u32 = 100;
    const ENTRIES_PER_PAGE: u32 = 1000;

    // 100 页 × 1000 条的键集（构造一次，样本复用）。
    let pages: Vec<Vec<(IndexKey, IndexVal)>> = (0..PAGES)
        .map(|p| {
            (0..ENTRIES_PER_PAGE)
                .map(|i| {
                    (
                        IndexKey {
                            x: (p * ENTRIES_PER_PAGE + i) as i32,
                            z: i as i32,
                            type_id: 0,
                        },
                        IndexVal {
                            seg_id: p,
                            offset: u64::from(i) * 256,
                            payload_len: 256,
                            gen: 1,
                            comp_id: 1,
                        },
                    )
                })
                .collect()
        })
        .collect();

    // 正确性：预算内插入 10 万条目，断言计账不超上界。
    let mut cache = SieveCache::new(BUDGET);
    for (p, entries) in pages.iter().enumerate() {
        cache.put(p as u32, Arc::new(IndexPage::from_entries(entries.clone())));
    }
    assert!(
        cache.len_bytes() <= BUDGET,
        "SieveCache billed {} bytes, exceeds budget {} bytes",
        cache.len_bytes(),
        BUDGET
    );
    println!(
        "memory_bound: {} pages cached, billed {} / {} bytes",
        cache.len_pages(),
        cache.len_bytes(),
        BUDGET
    );

    // 插入吞吐：每样本重建 10 万条目的缓存。
    c.bench_function("sieve_insert_100k", |b| {
        b.iter_batched(
            || SieveCache::new(BUDGET),
            |mut fresh| {
                for (p, entries) in pages.iter().enumerate() {
                    fresh.put(p as u32, Arc::new(IndexPage::from_entries(entries.clone())));
                }
                fresh.len_bytes()
            },
            BatchSize::PerIteration,
        )
    });

    // 预算内的随机 get 延迟。
    let lookups: Vec<u32> = {
        let mut s = 0xFACEu64;
        (0..10_000)
            .map(|_| (lcg(&mut s) % u64::from(PAGES)) as u32)
            .collect()
    };
    c.bench_function("sieve_lookup_10k", |b| {
        b.iter(|| {
            for &id in &lookups {
                black_box(cache.get(id));
            }
        })
    });
}

criterion_group!(
    benches,
    bench_footprint,
    bench_write_throughput,
    bench_read_latency,
    bench_memory_bound,
);
criterion_main!(benches);
