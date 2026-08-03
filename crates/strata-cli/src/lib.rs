//! Strata storage engine CLI.
//!
//! 子命令：
//! - `convert --to-strata <world>`：Anvil → Strata（覆盖目标 vstore、保留源）；
//! - `convert --to-anvil <world>`：Strata → Anvil（保留 vstore）；
//! - `verify` / `compact` / `stats`。
//!
//! 世界布局：`<world>/region|r.0.0.mca`（Anvil），`<world>/vstore/`（Strata Store root）。
//! type_id 映射：0=chunk(region/)、1=entities(entities/)、2=poi(poi/)。
//! Phase 1 仅 overworld。

pub mod anvil;
pub mod config;

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use clap::{Parser, Subcommand};
use strata_core::gc::GcConfig;
use strata_core::segment::scan_segment;
use strata_core::store::Store;

use crate::anvil::ChunkLoc;
use crate::config::{load_or_create_template, validate_matrix, CONFIG_FILE};

/// vstore 目录名。
const VSTORE_DIR: &str = "vstore";
/// vstore 内段文件子目录（与 strata-core 的布局一致）。
const SEGMENTS_DIR: &str = "segments";
/// 断点续转进度文件（位于 vstore 内）。
const PROGRESS_FILE: &str = ".convert-progress";
/// Anvil 源目录（按 type_id 顺序）。
const SOURCE_DIRS: [&str; 3] = ["region", "entities", "poi"];

#[derive(Parser)]
#[command(name = "strata-cli", version, about = "Strata storage engine CLI")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Anvil → Strata（Cesium 式：覆盖目标、保留源）
    Convert {
        #[arg(long)]
        to_strata: Option<PathBuf>,
        #[arg(long)]
        to_anvil: Option<PathBuf>,
    },
    /// 全量校验 vstore 段文件。
    Verify { world: PathBuf },
    /// GC + 冷热分层循环压实。
    Compact { world: PathBuf },
    /// 打印存储统计。
    Stats { world: PathBuf },
}

/// 可测试入口：`run(&["convert", "--to-strata", "<world>"])` 风格。
pub fn run(args: &[&str]) -> anyhow::Result<()> {
    let cli = Cli::try_parse_from(std::iter::once("strata-cli").chain(args.iter().copied()))?;
    main_impl(cli.cmd)
}

fn main_impl(cmd: Cmd) -> anyhow::Result<()> {
    match cmd {
        Cmd::Convert { to_strata, to_anvil } => match (to_strata, to_anvil) {
            (Some(world), None) => convert_to_strata(&world),
            (None, Some(world)) => convert_to_anvil(&world),
            _ => bail!("convert 需要且只能指定 --to-strata 或 --to-anvil 之一"),
        },
        Cmd::Verify { world } => verify(&world),
        Cmd::Compact { world } => compact(&world),
        Cmd::Stats { world } => stats(&world),
    }
}

/// type_id ↔ 源目录 ↔ 进度文件类型名。
#[derive(Clone, Copy)]
struct TypeKind {
    type_id: u16,
    dir: &'static str,
    name: &'static str,
}

const TYPE_KINDS: [TypeKind; 3] = [
    TypeKind { type_id: 0, dir: "region", name: "chunk" },
    TypeKind { type_id: 1, dir: "entities", name: "entity" },
    TypeKind { type_id: 2, dir: "poi", name: "poi" },
];

fn kind_by_type_id(type_id: u16) -> Option<&'static TypeKind> {
    TYPE_KINDS.iter().find(|k| k.type_id == type_id)
}

fn kind_by_name(name: &str) -> Option<&'static TypeKind> {
    TYPE_KINDS.iter().find(|k| k.name == name)
}

/// Phase 1 仅 overworld：存在 DIM-1/DIM1 目录直接拒绝。
fn require_overworld(world: &Path) -> anyhow::Result<()> {
    for dim in ["DIM-1", "DIM1"] {
        if world.join(dim).is_dir() {
            bail!("暂不支持非 overworld 维度");
        }
    }
    Ok(())
}

/// 递归累计目录字节数（文件长度之和；不存在 → 0）。
fn dir_bytes(path: &Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(path) else {
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

/// 递归统计扩展名为 `ext` 的文件数（`ext` 含点，如 ".varc"）。
fn count_files_recursive(path: &Path, ext: &str) -> u64 {
    let Ok(rd) = std::fs::read_dir(path) else {
        return 0;
    };
    let mut count = 0u64;
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            count += count_files_recursive(&path, ext);
        } else if entry.file_name().to_string_lossy().ends_with(ext) {
            count += 1;
        }
    }
    count
}

/// 解析 `r.X.Z.mca` 文件名中的 region 坐标。
fn parse_region_name(name: &str) -> Option<(i32, i32)> {
    let stem = name.strip_suffix(".mca")?;
    let mut parts = stem.strip_prefix("r.")?.split('.');
    let x: i32 = parts.next()?.parse().ok()?;
    let z: i32 = parts.next()?.parse().ok()?;
    (parts.next().is_none()).then_some((x, z))
}

/// 列出 vstore 磁盘上的全部段文件（按 seg_id 升序）。
fn list_segment_files(vstore: &Path) -> anyhow::Result<Vec<(u32, PathBuf)>> {
    let dir = vstore.join(SEGMENTS_DIR);
    let mut out: Vec<(u32, PathBuf)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(stem) = name.strip_suffix(".vseg") {
                if let Some(num) = stem.strip_prefix("seg-") {
                    if let Ok(id) = num.parse::<u32>() {
                        out.push((id, entry.path()));
                    }
                }
            }
        }
    }
    out.sort_by_key(|&(id, _)| id);
    Ok(out)
}

// ---------------------------------------------------------------- convert

/// 读进度文件（不存在 → 空集）。行格式 `<type>:r<rx>.<rz>`；坏行告警忽略。
fn load_progress(path: &Path) -> HashSet<String> {
    let mut done = HashSet::new();
    if let Ok(text) = std::fs::read_to_string(path) {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let valid = line
                .split_once(":r")
                .map(|(name, rest)| {
                    let Some((rx, rz)) = rest.split_once('.') else {
                        return false;
                    };
                    kind_by_name(name).is_some()
                        && rx.parse::<i32>().is_ok()
                        && rz.parse::<i32>().is_ok()
                })
                .unwrap_or(false);
            if valid {
                done.insert(line.to_string());
            } else {
                eprintln!("WARN: 进度文件行 '{line}' 无法识别，已忽略");
            }
        }
    }
    done
}

/// 追加一行进度并 fsync。
fn append_progress(root: &Path, line: &str) -> anyhow::Result<()> {
    let path = root.join(PROGRESS_FILE);
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("打开进度文件 {}", path.display()))?;
    writeln!(f, "{line}")?;
    f.sync_all()?;
    Ok(())
}

/// Anvil → Strata：覆盖目标 vstore、保留 Anvil 源。
fn convert_to_strata(world: &Path) -> anyhow::Result<()> {
    require_overworld(world)?;
    let vstore = world.join(VSTORE_DIR);

    // 1. 配置 + 矩阵 WARN（stderr）。
    let cfg = load_or_create_template(world)?;
    for w in validate_matrix(&cfg) {
        eprintln!("WARN: {w}");
    }

    // 2. 覆盖语义：进度文件在 vstore 内 → 先读进度，再整体删除重建。
    let mut done = HashSet::new();
    if vstore.exists() {
        done = load_progress(&vstore.join(PROGRESS_FILE));
        std::fs::remove_dir_all(&vstore)
            .with_context(|| format!("删除旧 vstore {}", vstore.display()))?;
    }

    // 3. 打开（新建）store。
    let mut store = Store::open(&vstore, cfg.store.clone())
        .with_context(|| format!("打开 vstore {}", vstore.display()))?;

    // 4. 遍历三类源目录。
    let mut regions_converted = 0u64;
    let mut regions_skipped = 0u64;
    let mut chunks_written = 0u64;
    for kind in &TYPE_KINDS {
        let dir = world.join(kind.dir);
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue; // 目录不存在 → 该类型无数据
        };
        let mut files: Vec<(i32, i32, PathBuf)> = Vec::new();
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some((rx, rz)) = parse_region_name(&name) {
                files.push((rx, rz, entry.path()));
            }
        }
        files.sort_by_key(|&(rx, rz, _)| (rx, rz));
        for (rx, rz, path) in files {
            let marker = format!("{}:r{rx}.{rz}", kind.name);
            if done.contains(&marker) {
                regions_skipped += 1;
                continue;
            }
            let chunks = anvil::read_region(&path)
                .with_context(|| format!("读取 Anvil 区域文件 {}", path.display()))?;
            for c in &chunks {
                let cx = rx * 32 + c.x as i32;
                let cz = rz * 32 + c.z as i32;
                store.write(cx, cz, kind.type_id, &c.nbt).with_context(|| {
                    format!("写入 ({cx}, {cz}) type {}", kind.type_id)
                })?;
                chunks_written += 1;
            }
            store.flush()?;
            append_progress(&vstore, &marker)?;
            regions_converted += 1;
        }
    }

    // 5. 收尾：删除进度文件。
    drop(store);
    let progress = vstore.join(PROGRESS_FILE);
    if progress.exists() {
        std::fs::remove_file(&progress)?;
    }

    // 6. 汇总。
    println!(
        "转换完成：{regions_converted} 个 region（{chunks_written} 条记录）写入 {}",
        vstore.display()
    );
    if regions_skipped > 0 {
        println!("按进度文件跳过 {regions_skipped} 个已完成 region");
    }
    println!("源 Anvil 文件已保留：{}", SOURCE_DIRS.join(" / "));
    println!("请验证后手动删除源目录");
    if !cfg.enabled {
        eprintln!("WARN: strata.enabled=false，转换后记得在 {CONFIG_FILE} 中启用");
    }
    Ok(())
}

/// Strata → Anvil：聚合 vstore 最新记录（同键最大 gen），覆盖写回 Anvil；vstore 保留。
fn convert_to_anvil(world: &Path) -> anyhow::Result<()> {
    require_overworld(world)?;
    let vstore = world.join(VSTORE_DIR);
    if !vstore.join("manifest.vsm").exists() {
        bail!(
            "{} 不存在或不是有效的 Strata vstore（缺少 manifest.vsm）",
            vstore.display()
        );
    }

    // 1. 配置 + 打开 store（只读用途：段扫描 + store.read 解压）。
    let cfg = load_or_create_template(world)?;
    let store = Store::open(&vstore, cfg.store.clone())
        .with_context(|| format!("打开 vstore {}", vstore.display()))?;

    // 2. 遍历 vstore 全部段 scan_segment，按 (type_id, region_x, region_z) 聚合
    //    存活记录（latest gen——与 GC 相同的判据：同键取最大 gen）。
    let mut latest: HashMap<(i32, i32, u16), u64> = HashMap::new();
    for (_id, path) in list_segment_files(&vstore)? {
        let scan = scan_segment(&path)
            .with_context(|| format!("扫描段文件 {}", path.display()))?;
        for rec in scan.records {
            if rec.env.payload_hash == 0 {
                continue; // 损坏记录
            }
            let key = (rec.env.chunk_x, rec.env.chunk_z, rec.env.type_id);
            match latest.get(&key) {
                Some(&g) if g >= rec.env.gen => {}
                _ => {
                    latest.insert(key, rec.env.gen);
                }
            }
        }
    }

    // 3. 解压 NBT（store.read = latest gen 视图 + 编码/字典解析）。
    let mut out: HashMap<(u16, i32, i32), Vec<ChunkLoc>> = HashMap::new();
    let mut skipped = 0u64;
    for &(x, z, type_id) in latest.keys() {
        if kind_by_type_id(type_id).is_none() {
            skipped += 1; // 未知类型不写回 Anvil
            continue;
        }
        let Some(nbt) = store.read(x, z, type_id)? else {
            skipped += 1;
            continue;
        };
        let (rx, rz) = (x >> 5, z >> 5);
        out.entry((type_id, rx, rz)).or_default().push(ChunkLoc {
            x: (x & 31) as u8,
            z: (z & 31) as u8,
            nbt,
            timestamp: 0,
        });
    }

    // 4. 覆盖写回：`.mca.tmp` → rename 为 `.mca`。
    let mut regions_written = 0u64;
    let mut chunks_written = 0u64;
    for ((type_id, rx, rz), mut chunks) in out {
        let kind = kind_by_type_id(type_id).expect("filtered by kind_by_type_id above");
        chunks.sort_by_key(|c| (c.z, c.x));
        let dir = world.join(kind.dir);
        std::fs::create_dir_all(&dir)?;
        let tmp = dir.join(format!("r.{rx}.{rz}.mca.tmp"));
        let final_path = dir.join(format!("r.{rx}.{rz}.mca"));
        anvil::write_region(&tmp, &chunks)
            .with_context(|| format!("写入 {}", tmp.display()))?;
        if final_path.exists() {
            std::fs::remove_file(&final_path)?; // Windows rename 不覆盖已存在文件
        }
        std::fs::rename(&tmp, &final_path)?;
        regions_written += 1;
        chunks_written += chunks.len() as u64;
    }

    if skipped > 0 {
        eprintln!("WARN: {skipped} 条记录无法读出（损坏或未知类型），未写回 Anvil");
    }
    println!("转回完成：{regions_written} 个 region（{chunks_written} 条记录）写回 Anvil");
    println!("vstore 已保留：{}", vstore.display());
    println!("请验证后手动删除 vstore");
    Ok(())
}

// ---------------------------------------------------------------- verify / compact / stats

fn verify(world: &Path) -> anyhow::Result<()> {
    require_overworld(world)?;
    let vstore = world.join(VSTORE_DIR);
    let cfg = load_or_create_template(world)?;
    let store = Store::open(&vstore, cfg.store)
        .with_context(|| format!("打开 vstore {}", vstore.display()))?;
    let report = store.verify()?;
    println!("records: {}", report.records);
    println!("corrupt: {}", report.corrupt_records.len());
    for (seg_id, offset) in &report.corrupt_records {
        println!("  seg-{seg_id:04} @ offset {offset}");
    }
    Ok(())
}

fn compact(world: &Path) -> anyhow::Result<()> {
    require_overworld(world)?;
    let vstore = world.join(VSTORE_DIR);
    let cfg = load_or_create_template(world)?;
    let mut store = Store::open(&vstore, cfg.store)
        .with_context(|| format!("打开 vstore {}", vstore.display()))?;

    let gc_cfg = GcConfig::default();
    let mut reclaimed_total = 0u64;
    let mut segments_removed = 0u32;
    let mut holes_punched = 0u32;
    let mut records_moved = 0u64;
    let mut promoted_total = 0u64;
    let mut demoted_total = 0u64;

    // 循环直到 GC 与分层都无进展（reclaimed==0 且 promoted+demoted==0）。
    loop {
        let gc = store.gc_pass(&gc_cfg)?;
        let tier = store.tier_pass(&cfg.tier)?;
        if gc.reclaimed_bytes == 0 && tier.promoted + tier.demoted == 0 {
            break;
        }
        reclaimed_total += gc.reclaimed_bytes;
        segments_removed += gc.segments_removed;
        holes_punched += gc.holes_punched;
        records_moved += gc.records_moved;
        promoted_total += tier.promoted as u64;
        demoted_total += tier.demoted as u64;
    }

    let (live, total) = store.touch_stats();
    println!(
        "压实完成：总回收 {reclaimed_total} 字节（整段删除 {segments_removed} 段，挖洞 {holes_punched} 处，搬迁 {records_moved} 条）"
    );
    println!("分层：晋升 {promoted_total} 段，降级 {demoted_total} 段");
    println!("当前段表：live {live} 字节 / total {total} 字节");
    Ok(())
}

fn stats(world: &Path) -> anyhow::Result<()> {
    require_overworld(world)?;
    let vstore = world.join(VSTORE_DIR);
    let cfg = load_or_create_template(world)?;
    let store = Store::open(&vstore, cfg.store)
        .with_context(|| format!("打开 vstore {}", vstore.display()))?;

    let (live, total) = store.touch_stats();
    let segments = list_segment_files(&vstore)?.len();
    let cold_archives = count_files_recursive(&vstore, ".varc");
    let bytes = dir_bytes(&vstore);

    println!("live_bytes: {live}");
    println!("total_bytes: {total}");
    println!("segments: {segments}");
    println!("cold_archives: {cold_archives}");
    println!("vstore_bytes: {bytes}");
    Ok(())
}
