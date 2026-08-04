use std::io::{Read, Write};
use std::path::Path;

use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use strata_cli::anvil::{read_region, write_region, ChunkLoc};
use strata_core::store::{Store, StoreConfig};

fn synth_anvil_world(world: &Path) {
    for dir in ["region", "entities", "poi"] {
        std::fs::create_dir_all(world.join(dir)).unwrap();
    }
    let chunks: Vec<_> = (0..10)
        .map(|i| ChunkLoc {
            x: i,
            z: 0,
            nbt: vec![i; 200],
            timestamp: i as u32,
        })
        .collect();
    write_region(&world.join("region/r.0.0.mca"), &chunks).unwrap();
    write_region(&world.join("entities/r.0.0.mca"), &chunks[..3]).unwrap();
    write_region(&world.join("poi/r.0.0.mca"), &chunks[..2]).unwrap();
}

/// 合成多维度世界：overworld（region + entities）、DIM-1（vanilla 布局）、
/// dimensions/minecraft/the_end（Canvas/Paper 布局）。各维度 payload 带标签，
/// 便于检测跨维度串数据。
fn synth_dim_world(world: &Path) {
    let dims = [
        ("region", 0xAAu8),
        ("DIM-1/region", 0xBBu8),
        ("dimensions/minecraft/the_end/region", 0xCCu8),
    ];
    for (dir, tag) in dims {
        let dir = world.join(dir);
        std::fs::create_dir_all(&dir).unwrap();
        let chunks: Vec<_> = (0..5)
            .map(|i| ChunkLoc {
                x: i,
                z: 1,
                nbt: vec![tag ^ i, 1, 2, 3],
                timestamp: i as u32,
            })
            .collect();
        write_region(&dir.join("r.0.0.mca"), &chunks).unwrap();
    }
    // overworld 额外带 entities，验证维度间 type_id 互不串扰。
    std::fs::create_dir_all(world.join("entities")).unwrap();
    let ent = [ChunkLoc { x: 9, z: 9, nbt: vec![0xEE, 5], timestamp: 7 }];
    write_region(&world.join("entities/r.0.0.mca"), &ent).unwrap();
}

/// 打开只读用途的 vstore（默认配置；read 的解码与配置级别无关）。
fn open_vstore(vstore: &Path) -> Store {
    Store::open(vstore, StoreConfig::default()).unwrap()
}

/// 与 CLI 相同的规范负载编码：gzip(NBT)。
fn gzip(nbt: &[u8]) -> Vec<u8> {
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    enc.write_all(nbt).unwrap();
    enc.finish().unwrap()
}

fn gunzip(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    GzDecoder::new(payload).read_to_end(&mut out).unwrap();
    out
}

#[test]
fn convert_to_strata_preserves_anvil_and_overwrites() {
    let dir = tempfile::tempdir().unwrap();
    synth_anvil_world(dir.path());
    strata_cli::run(&["convert", "--to-strata", dir.path().to_str().unwrap()]).unwrap();
    assert!(dir.path().join("region/r.0.0.mca").exists()); // 源保留
    assert!(dir.path().join("vstore/manifest.vsm").exists());
    assert!(!dir.path().join("vstore/.convert-progress").exists());

    // vstore 内负载为规范格式 gzip(NBT)：gzip magic 开头，解开即源 NBT。
    let store = open_vstore(&dir.path().join("vstore"));
    let payload = store.read(0, 0, 0).unwrap().unwrap();
    assert_eq!(&payload[..2], &[0x1F, 0x8B]);
    assert_eq!(gunzip(&payload), vec![0u8; 200]);
    drop(store);

    strata_cli::run(&["convert", "--to-strata", dir.path().to_str().unwrap()]).unwrap(); // 幂等覆盖
    assert!(dir.path().join("region/r.0.0.mca").exists());
}

#[test]
fn convert_roundtrip_preserves_all_types() {
    let dir = tempfile::tempdir().unwrap();
    synth_anvil_world(dir.path());
    strata_cli::run(&["convert", "--to-strata", dir.path().to_str().unwrap()]).unwrap();
    std::fs::remove_dir_all(dir.path().join("region")).unwrap(); // 证明转回来自 vstore
    strata_cli::run(&["convert", "--to-anvil", dir.path().to_str().unwrap()]).unwrap();
    let back = read_region(&dir.path().join("region/r.0.0.mca")).unwrap();
    assert_eq!(back.len(), 10);
    assert_eq!(back[3].nbt, vec![3u8; 200]); // Anvil→vstore→Anvil 字节一致
    assert!(dir.path().join("entities/r.0.0.mca").exists());
    assert!(dir.path().join("vstore/manifest.vsm").exists()); // 源 vstore 保留
}

/// 真中断续传：vstore 已有有效 manifest + 进度文件 → 续传模式。
/// 被跳过的 region（r.0.0 chunk）数据必须原样保留（以不同于源的负载证明是
/// "跳过"而非"重写"），未完成的 region 继续转换。
#[test]
fn interrupted_conversion_resumes_and_preserves_skipped_regions() {
    let dir = tempfile::tempdir().unwrap();
    synth_anvil_world(dir.path());

    // 模拟 chunk region 已写入后中断的盘上状态：负载故意与源不同。
    {
        let mut store =
            Store::open(&dir.path().join("vstore"), StoreConfig::default()).unwrap();
        for i in 0..10i32 {
            store.write(i, 0, 0, &gzip(&[0x5Au8; 200])).unwrap();
        }
        store.flush().unwrap();
    }
    // 进度键含维度根相对路径（overworld 为 "."）。
    std::fs::write(dir.path().join("vstore/.convert-progress"), ".:chunk:r0.0\n").unwrap();

    strata_cli::run(&["convert", "--to-strata", dir.path().to_str().unwrap()]).unwrap();

    assert!(!dir.path().join("vstore/.convert-progress").exists());
    let store = open_vstore(&dir.path().join("vstore"));
    // 被跳过的 r.0.0：全部 10 条记录可读回，且仍是中断前的负载。
    for i in 0..10i32 {
        let payload = store.read(i, 0, 0).unwrap().unwrap();
        assert_eq!(gunzip(&payload), vec![0x5Au8; 200]);
    }
    // 未完成的 entities/poi 正常转换（负载 = 源数据 gzip）。
    for i in 0..3i32 {
        let payload = store.read(i, 0, 1).unwrap().unwrap();
        assert_eq!(gunzip(&payload), vec![i as u8; 200]);
    }
    for i in 0..2i32 {
        let payload = store.read(i, 0, 2).unwrap().unwrap();
        assert_eq!(gunzip(&payload), vec![i as u8; 200]);
    }
}

/// 陈旧进度 + 无 manifest：进度对应的 vstore 已不存在 → 必须全量重建，
/// 不得按陈旧进度跳过 region（否则数据永久丢失）。
#[test]
fn stale_progress_without_manifest_rebuilds_from_scratch() {
    let dir = tempfile::tempdir().unwrap();
    synth_anvil_world(dir.path());
    std::fs::create_dir_all(dir.path().join("vstore")).unwrap(); // 空 vstore，无 manifest
    std::fs::write(dir.path().join("vstore/.convert-progress"), ".:chunk:r0.0\n").unwrap();

    strata_cli::run(&["convert", "--to-strata", dir.path().to_str().unwrap()]).unwrap();

    assert!(!dir.path().join("vstore/.convert-progress").exists()); // 进度被清除
    let store = open_vstore(&dir.path().join("vstore"));
    // chunk region 被重新转换（负载 = 源数据，而非被跳过丢失）。
    for i in 0..10i32 {
        let payload = store.read(i, 0, 0).unwrap().unwrap();
        assert_eq!(gunzip(&payload), vec![i as u8; 200]);
    }
    assert!(store.read(0, 0, 1).unwrap().is_some());
    assert!(store.read(0, 0, 2).unwrap().is_some());
}

/// vstore 缺失但 vstore.old 存在（recompress 交换中途崩溃态）→
/// verify/stats 自动 rename 恢复并可读。
#[test]
fn vstore_old_auto_recovery() {
    let dir = tempfile::tempdir().unwrap();
    synth_anvil_world(dir.path());
    strata_cli::run(&["convert", "--to-strata", dir.path().to_str().unwrap()]).unwrap();

    std::fs::rename(dir.path().join("vstore"), dir.path().join("vstore.old")).unwrap();
    assert!(!dir.path().join("vstore").exists());

    strata_cli::run(&["verify", dir.path().to_str().unwrap()]).unwrap();
    assert!(dir.path().join("vstore/manifest.vsm").exists()); // 已自动恢复
    assert!(!dir.path().join("vstore.old").exists());

    let store = open_vstore(&dir.path().join("vstore"));
    let payload = store.read(3, 0, 0).unwrap().unwrap();
    assert_eq!(gunzip(&payload), vec![3u8; 200]);
    drop(store);

    strata_cli::run(&["stats", dir.path().to_str().unwrap()]).unwrap(); // 恢复后 stats 可用
}

/// LZ4 解压炸弹：orig_len = 0xFFFFFFFF → 干净错误（不 abort、不分配 4GiB）。
#[test]
fn lz4_bomb_rejected_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("region")).unwrap();

    // 手工 .mca：location 表仅 1 项（offset=2, count=1）；
    // 记录 = u32 n + version 4(lz4) + payload(orig_len BE=0xFFFFFFFF + 垃圾)。
    let mut data = vec![0u8; 8192 + 4096];
    data[0..4].copy_from_slice(&(((2u32) << 8) | 1).to_be_bytes());
    let start = 8192usize;
    let body: [u8; 6] = [0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x00];
    let n = (1 + body.len()) as u32;
    data[start..start + 4].copy_from_slice(&n.to_be_bytes());
    data[start + 4] = 4; // VER_LZ4
    data[start + 5..start + 5 + body.len()].copy_from_slice(&body);
    std::fs::write(dir.path().join("region/r.0.0.mca"), &data).unwrap();

    let err = read_region(&dir.path().join("region/r.0.0.mca")).unwrap_err();
    assert!(format!("{err:#}").contains("lz4"), "{err:#}");
    // CLI 层：转换干净失败（非 panic）。
    assert!(strata_cli::run(&["convert", "--to-strata", dir.path().to_str().unwrap()]).is_err());
}

/// 扇区溢出：>255 扇区的记录 Anvil 无法表示 → to-anvil 显式错误。
#[test]
fn sector_overflow_rejected_on_to_anvil() {
    let dir = tempfile::tempdir().unwrap();
    // ~1.1MiB 不可压缩负载（LCG 伪随机）：zstd/deflate 均压不动。
    let mut nbt = Vec::with_capacity(1_100_000);
    let mut state: u64 = 0x2545_F491_4F6C_DD1D;
    for _ in 0..1_100_000 {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        nbt.push((state >> 33) as u8);
    }

    // 直接写入 vstore（Anvil 源本身装不下 >255 扇区的记录，vanilla 同样失败）。
    {
        let mut store =
            Store::open(&dir.path().join("vstore"), StoreConfig::default()).unwrap();
        store.write(0, 0, 0, &nbt).unwrap();
        store.flush().unwrap();
    }

    let err =
        strata_cli::run(&["convert", "--to-anvil", dir.path().to_str().unwrap()]).unwrap_err();
    assert!(format!("{err:#}").contains("255"), "{err:#}");
}

/// verify 发现损坏记录 → 返回错误（main 层映射非零退出码）。
#[test]
fn verify_reports_corruption_as_error() {
    let dir = tempfile::tempdir().unwrap();
    synth_anvil_world(dir.path());
    strata_cli::run(&["convert", "--to-strata", dir.path().to_str().unwrap()]).unwrap();

    // 翻转首个段文件第一条记录的 1 字节负载（16B 段头 + 40B 信封之后）。
    let seg_dir = dir.path().join("vstore/segments");
    let mut segs: Vec<_> = std::fs::read_dir(&seg_dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "vseg"))
        .collect();
    segs.sort();
    let mut bytes = std::fs::read(&segs[0]).unwrap();
    bytes[16 + 40] ^= 0xFF;
    std::fs::write(&segs[0], &bytes).unwrap();

    assert!(strata_cli::run(&["verify", dir.path().to_str().unwrap()]).is_err());
}

/// 符号链接环：stats 不跟随符号链接（旧实现会无限递归）。
#[test]
#[cfg(unix)]
fn stats_skips_symlink_loops() {
    let dir = tempfile::tempdir().unwrap();
    synth_anvil_world(dir.path());
    strata_cli::run(&["convert", "--to-strata", dir.path().to_str().unwrap()]).unwrap();
    std::os::unix::fs::symlink(dir.path().join("vstore"), dir.path().join("vstore/loop"))
        .unwrap();
    strata_cli::run(&["stats", dir.path().to_str().unwrap()]).unwrap();
}

/// BOM：strata.properties 带 U+FEFF 前缀也能正确解析。
#[test]
fn bom_prefixed_config_parses() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("strata.properties"),
        "\u{feff}strata.enabled=true\n",
    )
    .unwrap();
    let cfg = strata_cli::config::load_or_create_template(dir.path()).unwrap();
    assert!(cfg.enabled);
}

/// 表项指向文件头（offset < 2）→ read_region 拒绝。
#[test]
fn read_region_rejects_header_offset() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("r.0.0.mca");
    let mut data = vec![0u8; 8192];
    data[0..4].copy_from_slice(&(((1u32) << 8) | 1).to_be_bytes()); // offset=1 → 指向头
    std::fs::write(&path, &data).unwrap();
    let err = read_region(&path).unwrap_err();
    assert!(format!("{err}").contains("header"), "{err}");
}

/// strata.gc.min-hole-bytes 解析（CLI/Java parity）；缺省 gc_enabled=true。
#[test]
fn gc_min_hole_bytes_parsed() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("strata.properties"),
        "strata.enabled=true\nstrata.gc.min-hole-bytes=99999\n",
    )
    .unwrap();
    let cfg = strata_cli::config::load_or_create_template(dir.path()).unwrap();
    assert_eq!(cfg.gc.min_hole_bytes, 99999);
    assert!(cfg.gc_enabled); // 未显式给出 → 模板默认 true
}

/// strata.gc.enabled=false → compact 跳过 GC（退出码 0，无任何回收）。
#[test]
fn compact_skips_gc_when_disabled() {
    let dir = tempfile::tempdir().unwrap();
    // 同一键覆写 50 次：49 条死记录（死占比 ≈ 0.98 ≥ 阈值 0.6，GC 若运行必回收）。
    {
        let mut store =
            Store::open(&dir.path().join("vstore"), StoreConfig::default()).unwrap();
        for i in 0..50u8 {
            store.write(0, 0, 0, &[i; 1024]).unwrap();
        }
        store.flush().unwrap();
    }
    std::fs::write(
        dir.path().join("strata.properties"),
        "strata.enabled=true\nstrata.gc.enabled=false\n",
    )
    .unwrap();

    strata_cli::run(&["compact", dir.path().to_str().unwrap()]).unwrap();

    // GC 被跳过：50 条记录（含 49 条死记录）全部仍在盘上。
    let store = open_vstore(&dir.path().join("vstore"));
    assert_eq!(store.verify().unwrap().records, 50);
}

/// 历史裸 NBT 负载（未按规范 gzip）→ to-anvil 按原样写回（legacy raw 路径）。
#[test]
fn to_anvil_handles_legacy_raw_payloads() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut store =
            Store::open(&dir.path().join("vstore"), StoreConfig::default()).unwrap();
        store.write(1, 2, 0, b"legacy-raw-nbt").unwrap();
        store.flush().unwrap();
    }
    strata_cli::run(&["convert", "--to-anvil", dir.path().to_str().unwrap()]).unwrap();
    let back = read_region(&dir.path().join("region/r.0.0.mca")).unwrap();
    assert_eq!(back.len(), 1);
    assert_eq!(back[0].nbt, b"legacy-raw-nbt"); // 裸 NBT 原样写回，未被破坏
}

#[test]
fn multi_dim_convert_and_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    synth_dim_world(dir.path());
    strata_cli::run(&["convert", "--to-strata", dir.path().to_str().unwrap()]).unwrap();

    // 三种布局各得一个 vstore，且挂在各自维度根下。
    assert!(dir.path().join("vstore/manifest.vsm").exists());
    assert!(dir.path().join("DIM-1/vstore/manifest.vsm").exists());
    assert!(dir
        .path()
        .join("dimensions/minecraft/the_end/vstore/manifest.vsm")
        .exists());
    assert!(!dir.path().join("region/vstore").exists());

    // 各维度记录可读回（规范 gzip 负载），payload 互不串扰。
    let over = open_vstore(&dir.path().join("vstore"));
    for i in 0..5i32 {
        let payload = over.read(i, 1, 0).unwrap().unwrap();
        assert_eq!(gunzip(&payload), vec![0xAAu8 ^ i as u8, 1, 2, 3]);
    }
    let ent = over.read(9, 9, 1).unwrap().unwrap();
    assert_eq!(gunzip(&ent), vec![0xEE, 5]);
    drop(over);

    let nether = open_vstore(&dir.path().join("DIM-1/vstore"));
    for i in 0..5i32 {
        let payload = nether.read(i, 1, 0).unwrap().unwrap();
        assert_eq!(gunzip(&payload), vec![0xBBu8 ^ i as u8, 1, 2, 3]);
    }
    assert!(nether.read(9, 9, 1).unwrap().is_none()); // entities 只在 overworld
    drop(nether);

    let end = open_vstore(&dir.path().join("dimensions/minecraft/the_end/vstore"));
    for i in 0..5i32 {
        let payload = end.read(i, 1, 0).unwrap().unwrap();
        assert_eq!(gunzip(&payload), vec![0xCCu8 ^ i as u8, 1, 2, 3]);
    }
    drop(end);

    // 删除全部源目录后转回，证明数据全部来自各维度 vstore。
    for src in [
        "region",
        "entities",
        "DIM-1/region",
        "dimensions/minecraft/the_end/region",
    ] {
        std::fs::remove_dir_all(dir.path().join(src)).unwrap();
    }
    strata_cli::run(&["convert", "--to-anvil", dir.path().to_str().unwrap()]).unwrap();

    let over = read_region(&dir.path().join("region/r.0.0.mca")).unwrap();
    assert_eq!(over.len(), 5);
    assert_eq!(over[0].nbt, vec![0xAA, 1, 2, 3]);
    let ent = read_region(&dir.path().join("entities/r.0.0.mca")).unwrap();
    assert_eq!(ent.len(), 1);
    assert_eq!(ent[0].nbt, vec![0xEE, 5]);
    let nether = read_region(&dir.path().join("DIM-1/region/r.0.0.mca")).unwrap();
    assert_eq!(nether.len(), 5);
    assert_eq!(nether[0].nbt, vec![0xBB, 1, 2, 3]);
    let end = read_region(
        &dir.path().join("dimensions/minecraft/the_end/region/r.0.0.mca"),
    )
    .unwrap();
    assert_eq!(end.len(), 5);
    assert_eq!(end[0].nbt, vec![0xCC, 1, 2, 3]);
}

#[test]
fn recompress_preserves_records_and_backs_up() {
    let dir = tempfile::tempdir().unwrap();
    synth_anvil_world(dir.path());
    strata_cli::run(&["convert", "--to-strata", dir.path().to_str().unwrap()]).unwrap();

    // 改配置（热层升到 zstd-9）：recompress 必须按新配置重写。
    std::fs::write(
        dir.path().join("strata.properties"),
        "strata.enabled=true\nstrata.compression.hot=zstd-9\n",
    )
    .unwrap();

    let before = open_vstore(&dir.path().join("vstore")).verify().unwrap().records;
    assert_eq!(before, 15); // 10 chunk + 3 entity + 2 poi

    strata_cli::run(&["recompress", dir.path().to_str().unwrap()]).unwrap();

    assert!(dir.path().join("vstore/manifest.vsm").exists());
    assert!(dir.path().join("vstore.old/manifest.vsm").exists());
    assert!(!dir.path().join("vstore.new").exists());

    let store = open_vstore(&dir.path().join("vstore"));
    assert_eq!(store.verify().unwrap().records, before); // 记录数不变
    for i in 0..10u8 {
        let payload = store.read(i as i32, 0, 0).unwrap().unwrap();
        assert_eq!(gunzip(&payload), vec![i; 200]);
    }
    for i in 0..3u8 {
        let payload = store.read(i as i32, 0, 1).unwrap().unwrap();
        assert_eq!(gunzip(&payload), vec![i; 200]);
    }
    for i in 0..2u8 {
        let payload = store.read(i as i32, 0, 2).unwrap().unwrap();
        assert_eq!(gunzip(&payload), vec![i; 200]);
    }
    drop(store);

    // 二次 recompress：旧备份被替换，数据仍一致（幂等）。
    strata_cli::run(&["recompress", dir.path().to_str().unwrap()]).unwrap();
    assert!(dir.path().join("vstore.old/manifest.vsm").exists());
    let store = open_vstore(&dir.path().join("vstore"));
    assert_eq!(store.verify().unwrap().records, before);
    let payload = store.read(3, 0, 0).unwrap().unwrap();
    assert_eq!(gunzip(&payload), vec![3u8; 200]);
}

#[test]
fn recompress_multi_dim() {
    let dir = tempfile::tempdir().unwrap();
    synth_dim_world(dir.path());
    strata_cli::run(&["convert", "--to-strata", dir.path().to_str().unwrap()]).unwrap();
    strata_cli::run(&["recompress", dir.path().to_str().unwrap()]).unwrap();

    // 三个维度各自备份且数据完好。
    for dim in ["", "DIM-1", "dimensions/minecraft/the_end"] {
        assert!(dir.path().join(dim).join("vstore/manifest.vsm").exists(), "{dim}");
        assert!(dir.path().join(dim).join("vstore.old/manifest.vsm").exists(), "{dim}");
    }
    let nether = open_vstore(&dir.path().join("DIM-1/vstore"));
    let payload = nether.read(2, 1, 0).unwrap().unwrap();
    assert_eq!(gunzip(&payload), vec![0xBB ^ 2, 1, 2, 3]);
}

#[test]
fn empty_world_friendly_errors() {
    let dir = tempfile::tempdir().unwrap();
    let w = dir.path().to_str().unwrap();
    assert!(strata_cli::run(&["convert", "--to-strata", w]).is_err());
    assert!(strata_cli::run(&["convert", "--to-anvil", w]).is_err());
    assert!(strata_cli::run(&["verify", w]).is_err());
    assert!(strata_cli::run(&["recompress", w]).is_err());
}
