use strata_cli::anvil::{read_region, write_region, ChunkLoc};

fn synth_anvil_world(world: &std::path::Path) {
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
    std::fs::write(dir.path().join("vstore/.convert-progress"), "chunk:r0.0\n").unwrap();
    strata_cli::run(&["convert", "--to-strata", dir.path().to_str().unwrap()]).unwrap();
    assert!(!dir.path().join("vstore/.convert-progress").exists());
    // 未完成的 entities/poi 仍被转换（chunk region 按进度跳过）。
    assert!(dir.path().join("vstore/manifest.vsm").exists());
}
