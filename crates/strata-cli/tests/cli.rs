use std::path::Path;

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

#[test]
fn convert_to_strata_preserves_anvil_and_overwrites() {
    let dir = tempfile::tempdir().unwrap();
    synth_anvil_world(dir.path());
    strata_cli::run(&["convert", "--to-strata", dir.path().to_str().unwrap()]).unwrap();
    assert!(dir.path().join("region/r.0.0.mca").exists()); // 源保留
    assert!(dir.path().join("vstore/manifest.vsm").exists());
    assert!(!dir.path().join("vstore/.convert-progress").exists());
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
    assert_eq!(back[3].nbt, vec![3u8; 200]);
    assert!(dir.path().join("entities/r.0.0.mca").exists());
    assert!(dir.path().join("vstore/manifest.vsm").exists()); // 源 vstore 保留
}

#[test]
fn interrupted_conversion_resumes() {
    let dir = tempfile::tempdir().unwrap();
    synth_anvil_world(dir.path());
    std::fs::create_dir_all(dir.path().join("vstore")).unwrap();
    // 进度键含维度根相对路径（overworld 为 "."）。
    std::fs::write(dir.path().join("vstore/.convert-progress"), ".:chunk:r0.0\n").unwrap();
    strata_cli::run(&["convert", "--to-strata", dir.path().to_str().unwrap()]).unwrap();
    assert!(!dir.path().join("vstore/.convert-progress").exists());
    // 未完成的 entities/poi 仍被转换（chunk region 按进度跳过）。
    assert!(dir.path().join("vstore/manifest.vsm").exists());
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

    // 各维度记录可读回，payload 互不串扰。
    let over = open_vstore(&dir.path().join("vstore"));
    for i in 0..5i32 {
        assert_eq!(
            over.read(i, 1, 0).unwrap().as_deref(),
            Some([0xAAu8 ^ i as u8, 1, 2, 3].as_slice())
        );
    }
    assert_eq!(over.read(9, 9, 1).unwrap().as_deref(), Some([0xEE, 5].as_slice()));
    drop(over);

    let nether = open_vstore(&dir.path().join("DIM-1/vstore"));
    for i in 0..5i32 {
        assert_eq!(
            nether.read(i, 1, 0).unwrap().as_deref(),
            Some([0xBBu8 ^ i as u8, 1, 2, 3].as_slice())
        );
    }
    assert_eq!(nether.read(9, 9, 1).unwrap(), None); // entities 只在 overworld
    drop(nether);

    let end = open_vstore(&dir.path().join("dimensions/minecraft/the_end/vstore"));
    for i in 0..5i32 {
        assert_eq!(
            end.read(i, 1, 0).unwrap().as_deref(),
            Some([0xCCu8 ^ i as u8, 1, 2, 3].as_slice())
        );
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
        assert_eq!(
            store.read(i as i32, 0, 0).unwrap().as_deref(),
            Some(vec![i; 200].as_slice())
        );
    }
    for i in 0..3u8 {
        assert_eq!(
            store.read(i as i32, 0, 1).unwrap().as_deref(),
            Some(vec![i; 200].as_slice())
        );
    }
    for i in 0..2u8 {
        assert_eq!(
            store.read(i as i32, 0, 2).unwrap().as_deref(),
            Some(vec![i; 200].as_slice())
        );
    }
    drop(store);

    // 二次 recompress：旧备份被替换，数据仍一致（幂等）。
    strata_cli::run(&["recompress", dir.path().to_str().unwrap()]).unwrap();
    assert!(dir.path().join("vstore.old/manifest.vsm").exists());
    let store = open_vstore(&dir.path().join("vstore"));
    assert_eq!(store.verify().unwrap().records, before);
    assert_eq!(
        store.read(3, 0, 0).unwrap().as_deref(),
        Some(vec![3u8; 200].as_slice())
    );
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
    assert_eq!(
        nether.read(2, 1, 0).unwrap().as_deref(),
        Some([0xBB ^ 2, 1, 2, 3].as_slice())
    );
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
