//! Strata storage engine CLI.
//!
//! 子命令：
//! - `convert --to-strata <world>`：Anvil → Strata（覆盖目标 vstore、保留源）；
//! - `convert --to-anvil <world>`：Strata → Anvil（保留 vstore）；
//! - `verify` / `compact` / `stats`；
//! - `recompress <world>`：按当前配置重写各维度 vstore 的全部存活记录。
//!
//! 关键语义：
//! - vstore 负载规范格式 = gzip 压缩的 NBT（与运行时 NbtIo.writeCompressed/
//!   readCompressed 对称）；to-anvil 对历史裸 NBT 负载按 legacy raw 原样写回。
//! - 断点续转进度仅在同一 vstore 生命周期内有效：vstore 存在且 manifest 完好
//!   且进度文件存在才续传，否则删除后全量重建。
//! - 每个维度处理前做 vstore.old / vstore.new 残留预检（崩溃恢复/清理）。
//! - verify / compact / stats / recompress 的多维度循环为聚合式：单维度失败
//!   不中止，全部执行完后汇总报错（非零退出码）。
//!
//! 世界布局：`<world>/region|r.0.0.mca`（Anvil），`<world>/vstore/`（Strata Store root）。
//! 维度根候选（按序）：世界根（overworld）、`DIM-1`/`DIM1`（vanilla 布局）、
//! `dimensions/minecraft/<name>`（Canvas/Paper 布局）；目录含 region/entities/poi
//! 之一即为有效维度根。每个维度根拥有自己的 `<dimroot>/vstore`。
//! type_id 映射：0=chunk(region/)、1=entities(entities/)、2=poi(poi/)。

pub mod anvil;
pub mod config;

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use clap::{Parser, Subcommand};
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use strata_core::cold::ArchiveReader;
use strata_core::gc::GcStats;
use strata_core::manifest::Manifest;
use strata_core::segment::scan_segment;
use strata_core::store::Store;
use xxhash_rust::xxh64::xxh64;

use crate::anvil::ChunkLoc;
use crate::config::{load_or_create_template, validate_matrix, CONFIG_FILE};

/// vstore 目录名。
const VSTORE_DIR: &str = "vstore";
/// recompress 的新 vstore 中间目录名。
const VSTORE_NEW_DIR: &str = "vstore.new";
/// recompress 后原 vstore 的备份目录名。
const VSTORE_OLD_DIR: &str = "vstore.old";
/// vstore 内段文件子目录（与 strata-core 的布局一致）。
const SEGMENTS_DIR: &str = "segments";
/// 断点续转进度文件（位于各维度的 vstore 内）。
const PROGRESS_FILE: &str = ".convert-progress";
/// Anvil 源目录（按 type_id 顺序），同时是有效维度根的判定依据。
const SOURCE_DIRS: [&str; 3] = ["region", "entities", "poi"];
/// vstore 内冷归档子目录。
const COLD_DIR: &str = "cold";
/// manifest 文件名（与 strata-core 的布局一致）。
const MANIFEST_FILE: &str = "manifest.vsm";
/// overworld（世界根）的进度键。
const OVERWORLD_LABEL: &str = ".";

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
    /// 全量校验各维度 vstore 段文件。
    Verify { world: PathBuf },
    /// GC + 冷热分层循环压实。
    Compact { world: PathBuf },
    /// 打印存储统计。
    Stats { world: PathBuf },
    /// 按当前 strata.properties 配置重写各维度 vstore 的全部存活记录。
    Recompress { world: PathBuf },
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
        Cmd::Recompress { world } => recompress(&world),
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

// ---------------------------------------------------------------- dimension discovery

/// 世界根下的一个维度根。
#[derive(Debug)]
struct DimRoot {
    /// 相对世界根的路径（`/` 分隔）；overworld 为空串。
    rel: String,
    /// 维度根完整路径。
    path: PathBuf,
}

impl DimRoot {
    /// 进度/报告用的标签：overworld 为 `.`，其余为相对路径。
    fn label(&self) -> &str {
        if self.rel.is_empty() {
            OVERWORLD_LABEL
        } else {
            &self.rel
        }
    }
}

/// 含 region/entities/poi 至少其一的目录才是有效维度根。
fn is_dim_root(path: &Path) -> bool {
    SOURCE_DIRS.iter().any(|d| path.join(d).is_dir())
}

fn dim_root(world: &Path, path: PathBuf) -> DimRoot {
    let rel = path
        .strip_prefix(world)
        .map(|r| r.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default();
    DimRoot { rel, path }
}

/// 按契约顺序枚举候选维度根路径：
/// 世界根 → `DIM-1`/`DIM1`（存在时）→ `dimensions/minecraft/<子目录>`（字典序）。
fn candidate_dim_paths(world: &Path) -> Vec<PathBuf> {
    let mut out = vec![world.to_path_buf()];
    for name in ["DIM-1", "DIM1"] {
        let p = world.join(name);
        if p.is_dir() {
            out.push(p);
        }
    }
    let dims = world.join("dimensions").join("minecraft");
    if let Ok(rd) = std::fs::read_dir(&dims) {
        let mut subs: Vec<PathBuf> =
            rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
        subs.sort();
        out.extend(subs);
    }
    out
}

/// 世界根下全部有效维度根（`convert --to-strata` 用）。
fn discover_dim_roots(world: &Path) -> Vec<DimRoot> {
    candidate_dim_paths(world)
        .into_iter()
        .filter(|p| is_dim_root(p))
        .map(|path| dim_root(world, path))
        .collect()
}

/// 世界根下全部含 vstore（或可恢复 vstore.old）的维度根
/// （to-anvil / verify / compact / stats / recompress 用）。
fn discover_vstore_roots(world: &Path) -> Vec<DimRoot> {
    candidate_dim_paths(world)
        .into_iter()
        .filter(|p| {
            p.join(VSTORE_DIR).join(MANIFEST_FILE).exists()
                || p.join(VSTORE_OLD_DIR).join(MANIFEST_FILE).exists()
        })
        .map(|path| dim_root(world, path))
        .collect()
}

// ---------------------------------------------------------------- shared helpers

/// 递归累计目录字节数（文件长度之和；不存在 → 0）。
/// 符号链接一律跳过（不跟随）：避免链接环死循环与重复计数。
fn dir_bytes(path: &Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(path) else {
        return 0;
    };
    let mut total = 0u64;
    for entry in rd.flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            total += dir_bytes(&path);
        } else {
            total += meta.len();
        }
    }
    total
}

/// 递归统计扩展名为 `ext` 的文件数（`ext` 含点，如 ".varc"）。
/// 符号链接一律跳过（不跟随），与 [`dir_bytes`] 一致。
fn count_files_recursive(path: &Path, ext: &str) -> u64 {
    let Ok(rd) = std::fs::read_dir(path) else {
        return 0;
    };
    let mut count = 0u64;
    for entry in rd.flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            count += count_files_recursive(&path, ext);
        } else if entry.file_name().to_string_lossy().ends_with(ext) {
            count += 1;
        }
    }
    count
}

/// 预检：处理上次崩溃遗留的 vstore.old / vstore.new（处理每个维度前调用）。
///
/// - vstore 缺失但 vstore.old 存在 → rename vstore.old → vstore
///   （recompress 交换中途崩溃的恢复路径），打印醒目提示；
/// - vstore 存在且残留 vstore.new → 删除 vstore.new（未完成的中间产物）；
/// - vstore 与 vstore.old 同时存在 → 报错要求人工介入，不猜
///   （`is_recompress` 例外：recompress 自己管理备份槽，交换协议会替换 vstore.old）。
fn preflight_vstore(dim: &DimRoot, is_recompress: bool) -> anyhow::Result<()> {
    let vstore = dim.path.join(VSTORE_DIR);
    let old_root = dim.path.join(VSTORE_OLD_DIR);
    let new_root = dim.path.join(VSTORE_NEW_DIR);

    if vstore.exists() && old_root.exists() && !is_recompress {
        bail!(
            "维度 {} 同时存在 {} 与 {}，无法自动判定哪个有效，请人工核对后只保留其一",
            dim.path.display(),
            VSTORE_DIR,
            VSTORE_OLD_DIR
        );
    }
    if !vstore.exists() && old_root.exists() {
        std::fs::rename(&old_root, &vstore).with_context(|| {
            format!(
                "恢复维度 {}：重命名 {} → {}",
                dim.label(),
                old_root.display(),
                vstore.display()
            )
        })?;
        eprintln!(
            "WARN: 维度 {} 缺少 {}，已从备份自动恢复 {} → {}（上次 recompress 可能崩在交换中途）",
            dim.label(),
            VSTORE_DIR,
            VSTORE_OLD_DIR,
            VSTORE_DIR
        );
    }
    if vstore.exists() && new_root.exists() {
        remove_dir_with_retry(&new_root)
            .with_context(|| format!("清理遗留的 {}", new_root.display()))?;
        eprintln!(
            "WARN: 维度 {} 残留未完成的中间产物 {}，已删除",
            dim.label(),
            VSTORE_NEW_DIR
        );
    }
    Ok(())
}

/// 聚合各维度失败：逐一打印后以汇总错误返回（非零退出码）。
fn report_dim_failures(op: &str, failures: &[(String, anyhow::Error)]) -> anyhow::Result<()> {
    if failures.is_empty() {
        return Ok(());
    }
    let mut msg = format!("{op}：{} 个维度失败", failures.len());
    for (label, err) in failures {
        msg.push_str(&format!("\n  {label}: {err:#}"));
    }
    bail!("{msg}")
}

/// vstore 负载规范格式 = gzip 压缩的 NBT 字节（与运行时 NbtIo.writeCompressed 对称）。
fn gzip_nbt(nbt: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    enc.write_all(nbt)?;
    enc.finish()
}

/// [`gzip_nbt`] 的逆操作；负载不是 gzip（历史裸 NBT 数据）→ Err。
fn gunzip_nbt(payload: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    GzDecoder::new(payload).read_to_end(&mut out)?;
    Ok(out)
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

/// 聚合 vstore 全部段中存活记录的最新 gen：键 = (x, z, type_id)。
/// `payload_hash = 0` 的记录视为损坏跳过（与 GC/verify 判据一致）。
fn latest_gens(vstore: &Path) -> anyhow::Result<HashMap<(i32, i32, u16), u64>> {
    let mut latest: HashMap<(i32, i32, u16), u64> = HashMap::new();
    for (_id, path) in list_segment_files(vstore)? {
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
    Ok(latest)
}

/// 冷归档中的存活键（未被失效位图作废、且热层没有更新版本的键）。
fn cold_live_keys(
    vstore: &Path,
    hot: &HashMap<(i32, i32, u16), u64>,
) -> anyhow::Result<Vec<(i32, i32, u16)>> {
    let Some(manifest) = Manifest::load(vstore)
        .with_context(|| format!("加载 manifest {}", vstore.join(MANIFEST_FILE).display()))?
    else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for cm in &manifest.cold {
        let path = vstore
            .join(COLD_DIR)
            .join(format!("r.{}.{}.varc", cm.region_x, cm.region_z));
        let mut reader = ArchiveReader::open(&path)
            .with_context(|| format!("打开冷归档 {}", path.display()))?;
        for (env, _nbt) in reader
            .extract_all()
            .with_context(|| format!("解包冷归档 {}", path.display()))?
        {
            let key = (env.chunk_x, env.chunk_z, env.type_id);
            // 热层已有该键 → 热层是更新版本（冷槽应已失效，防御性跳过）。
            if hot.contains_key(&key) {
                continue;
            }
            if reader.get(env.chunk_x, env.chunk_z, env.type_id)?.is_some() {
                out.push(key);
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------- convert

/// 进度行合法性：`<维度键>:<类型名>:r<rx>.<rz>`（维度键 = 维度根相对路径，
/// overworld 为 `.`）。旧格式（无维度键）无法识别，会被告警忽略。
fn is_valid_progress_line(line: &str) -> bool {
    let Some((head, coords)) = line.rsplit_once(':') else {
        return false;
    };
    let Some((label, name)) = head.rsplit_once(':') else {
        return false;
    };
    let Some(rest) = coords.strip_prefix('r') else {
        return false;
    };
    let Some((rx, rz)) = rest.split_once('.') else {
        return false;
    };
    !label.is_empty()
        && kind_by_name(name).is_some()
        && rx.parse::<i32>().is_ok()
        && rz.parse::<i32>().is_ok()
}

/// 读进度文件（不存在 → 空集）。行格式 `<维度键>:<类型>:r<rx>.<rz>`；坏行告警忽略。
fn load_progress(path: &Path) -> HashSet<String> {
    let mut done = HashSet::new();
    if let Ok(text) = std::fs::read_to_string(path) {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if is_valid_progress_line(line) {
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

/// Windows 上新建文件可能被杀软/索引器短暂锁定：删除失败时重试 3 次。
fn remove_with_retry(path: &Path) -> std::io::Result<()> {
    let mut last = None;
    for _ in 0..3 {
        match std::fs::remove_file(path) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                last = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("retry exhausted")))
}

/// Windows 上新写入的文件可能被杀软/索引器短暂锁定：目录删除失败时重试。
fn remove_dir_with_retry(path: &Path) -> std::io::Result<()> {
    let mut last = None;
    for _ in 0..5 {
        match std::fs::remove_dir_all(path) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                last = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("retry exhausted")))
}

/// Anvil → Strata：遍历全部有效维度根，覆盖各自 vstore、保留 Anvil 源。
fn convert_to_strata(world: &Path) -> anyhow::Result<()> {
    let dims = discover_dim_roots(world);
    if dims.is_empty() {
        bail!(
            "{} 下未发现任何有效维度根（需含 {} 至少其一）",
            world.display(),
            SOURCE_DIRS.join(" / ")
        );
    }

    let cfg = load_or_create_template(world)?;
    for w in validate_matrix(&cfg) {
        eprintln!("WARN: {w}");
    }

    let mut total_regions = 0u64;
    let mut total_chunks = 0u64;
    let mut total_skipped = 0u64;
    for dim in &dims {
        let (converted, written, skipped) = convert_dim_to_strata(dim, &cfg)?;
        total_regions += converted;
        total_chunks += written;
        total_skipped += skipped;
    }

    println!(
        "转换完成：共 {} 个维度，{total_regions} 个 region（{total_chunks} 条记录）",
        dims.len()
    );
    if total_skipped > 0 {
        println!("按进度文件跳过 {total_skipped} 个已完成 region");
    }
    println!("源 Anvil 文件已保留：{}", SOURCE_DIRS.join(" / "));
    println!("请验证后手动删除源目录");
    if !cfg.enabled {
        eprintln!("WARN: strata.enabled=false，转换后记得在 {CONFIG_FILE} 中启用");
    }
    Ok(())
}

/// 单维度 Anvil → Strata：覆盖目标 vstore、保留 Anvil 源，负载以规范格式
/// gzip(NBT) 写入。返回（转换 region 数、写入记录数、按进度跳过 region 数）。
///
/// **进度仅在同一 vstore 生命周期内有效**：vstore 存在且 manifest 完好且
/// 进度文件存在 → 续传（保留现有 vstore，跳过已完成 region）；其它一切情况
/// （无进度 / 无 vstore / manifest 缺失或损坏）→ 删除后从零全量重建。
fn convert_dim_to_strata(
    dim: &DimRoot,
    cfg: &config::StrataConfig,
) -> anyhow::Result<(u64, u64, u64)> {
    preflight_vstore(dim, false)?;
    let vstore = dim.path.join(VSTORE_DIR);
    let progress_path = vstore.join(PROGRESS_FILE);

    // 续传条件：vstore 存在 AND manifest 完好（Manifest::load 读出）AND 进度文件存在。
    // 进度文件在 vstore 内：manifest 缺失/损坏说明进度对应的写入对象已不可信，
    // 此时残留进度只会导致跳过本应重转的 region，必须全量重建。
    let resume_store = if vstore.exists() {
        match Manifest::load(&vstore) {
            Ok(Some(_)) if progress_path.exists() => Some(
                Store::open(&vstore, cfg.store.clone())
                    .with_context(|| format!("打开 vstore {}", vstore.display()))?,
            ),
            Ok(Some(_)) => None, // vstore 完好但无进度 → 覆盖语义：全量重建
            Ok(None) => {
                eprintln!(
                    "WARN: 维度 {} 的 vstore 缺少 manifest，忽略残留进度并全量重建",
                    dim.label()
                );
                None
            }
            Err(e) => {
                eprintln!(
                    "WARN: 维度 {} 的 vstore manifest 损坏（{e}），忽略残留进度并全量重建",
                    dim.label()
                );
                None
            }
        }
    } else {
        None
    };

    let (mut store, done) = match resume_store {
        Some(s) => (s, load_progress(&progress_path)),
        None => {
            // Windows 上新写入的文件可能被杀软短暂锁定，remove_dir_all 带重试。
            if vstore.exists() {
                remove_dir_with_retry(&vstore)
                    .with_context(|| format!("删除旧 vstore {}", vstore.display()))?;
            }
            let s = Store::open(&vstore, cfg.store.clone())
                .with_context(|| format!("打开 vstore {}", vstore.display()))?;
            (s, HashSet::new())
        }
    };

    let mut regions_converted = 0u64;
    let mut regions_skipped = 0u64;
    let mut chunks_written = 0u64;
    for kind in &TYPE_KINDS {
        let dir = dim.path.join(kind.dir);
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
            // 进度键含维度根相对路径，避免跨维度混淆。
            let marker = format!("{}:{}:r{rx}.{rz}", dim.label(), kind.name);
            if done.contains(&marker) {
                regions_skipped += 1;
                continue;
            }
            let chunks = anvil::read_region(&path)
                .with_context(|| format!("读取 Anvil 区域文件 {}", path.display()))?;
            for c in &chunks {
                let cx = rx * 32 + c.x as i32;
                let cz = rz * 32 + c.z as i32;
                // 规范负载格式 = gzip(NBT)，与运行时 NbtIo.writeCompressed 对称。
                let payload = gzip_nbt(&c.nbt).with_context(|| {
                    format!("gzip 编码 ({cx}, {cz}) type {}", kind.type_id)
                })?;
                store.write(cx, cz, kind.type_id, &payload).with_context(|| {
                    format!("写入 ({cx}, {cz}) type {}", kind.type_id)
                })?;
                chunks_written += 1;
            }
            store
                .flush()
                .with_context(|| format!("region {marker} 写入后 flush"))?;
            append_progress(&vstore, &marker)?;
            regions_converted += 1;
        }
    }

    // 收尾：删除进度文件。删除失败必须硬错误——留下完整进度会让下次运行
    // 误入续传路径跳过 region。
    drop(store);
    if progress_path.exists() {
        remove_with_retry(&progress_path).with_context(|| {
            format!(
                "删除进度文件 {} 失败，请手动删除该文件后重跑本命令",
                progress_path.display()
            )
        })?;
    }

    let bytes = dir_bytes(&vstore);
    println!(
        "维度 {}：{regions_converted} 个 region（{chunks_written} 条记录）→ {}（{bytes} 字节）",
        dim.path.display(),
        vstore.display()
    );
    Ok((regions_converted, chunks_written, regions_skipped))
}

/// Strata → Anvil：遍历全部含 vstore 的维度根，聚合最新记录覆盖写回 Anvil；vstore 保留。
fn convert_to_anvil(world: &Path) -> anyhow::Result<()> {
    let dims = discover_vstore_roots(world);
    if dims.is_empty() {
        bail!(
            "{} 下未找到可转换的 vstore（缺少 {VSTORE_DIR}/{MANIFEST_FILE}）",
            world.display()
        );
    }

    let cfg = load_or_create_template(world)?;
    let mut total_regions = 0u64;
    let mut total_chunks = 0u64;
    let mut total_legacy = 0u64;
    for dim in &dims {
        let (regions, chunks, legacy) = convert_dim_to_anvil(dim, &cfg)?;
        total_regions += regions;
        total_chunks += chunks;
        total_legacy += legacy;
    }

    println!(
        "转回完成：共 {} 个维度，{total_regions} 个 region（{total_chunks} 条记录）写回 Anvil",
        dims.len()
    );
    if total_legacy > 0 {
        println!("其中 {total_legacy} 条为 legacy raw records（历史裸 NBT，未按规范 gzip），已按原样写回");
    }
    println!("vstore 已保留，请验证后手动删除");
    Ok(())
}

/// 单维度 Strata → Anvil：vstore → region/entities/poi 的 DEFLATE .mca。
/// 负载按规范格式 gzip(NBT) 解出裸 NBT 再写回；历史裸格式负载按原样写回并计数。
/// 返回（写回 region 数、写回记录数、legacy raw 记录数）。
fn convert_dim_to_anvil(
    dim: &DimRoot,
    cfg: &config::StrataConfig,
) -> anyhow::Result<(u64, u64, u64)> {
    preflight_vstore(dim, false)?;
    let vstore = dim.path.join(VSTORE_DIR);
    let store = Store::open(&vstore, cfg.store.clone())
        .with_context(|| format!("打开 vstore {}", vstore.display()))?;

    // 段扫描聚合存活记录（latest gen），再解出 NBT（store.read = latest gen 视图
    // + 编码/字典解析，冷归档键由 read 的冷查路径兜底）。
    let latest = latest_gens(&vstore)?;
    let mut out: HashMap<(u16, i32, i32), Vec<ChunkLoc>> = HashMap::new();
    let mut skipped = 0u64;
    let mut legacy_raw = 0u64;
    for &(x, z, type_id) in latest.keys() {
        if kind_by_type_id(type_id).is_none() {
            skipped += 1; // 未知类型不写回 Anvil
            continue;
        }
        let Some(payload) = store.read(x, z, type_id)? else {
            skipped += 1;
            continue;
        };
        // 规范负载 = gzip(NBT) → gunzip 出裸 NBT；gunzip 失败说明是历史裸格式
        // 转换产物，按裸 NBT 直接写回并计数提示。
        let (nbt, legacy) = match gunzip_nbt(&payload) {
            Ok(raw) => (raw, false),
            Err(_) => (payload, true),
        };
        if legacy {
            legacy_raw += 1;
        }
        let (rx, rz) = (x >> 5, z >> 5);
        out.entry((type_id, rx, rz)).or_default().push(ChunkLoc {
            x: (x & 31) as u8,
            z: (z & 31) as u8,
            nbt,
            timestamp: 0,
        });
    }

    // 覆盖写回：`.mca.tmp` → rename 为 `.mca`。
    let mut regions_written = 0u64;
    let mut chunks_written = 0u64;
    let mut bytes_written = 0u64;
    for ((type_id, rx, rz), mut chunks) in out {
        let kind = kind_by_type_id(type_id).expect("filtered by kind_by_type_id above");
        chunks.sort_by_key(|c| (c.z, c.x));
        let dir = dim.path.join(kind.dir);
        std::fs::create_dir_all(&dir)?;
        let tmp = dir.join(format!("r.{rx}.{rz}.mca.tmp"));
        let final_path = dir.join(format!("r.{rx}.{rz}.mca"));
        anvil::write_region(&tmp, &chunks)
            .with_context(|| format!("写入 {}", tmp.display()))?;
        if final_path.exists() {
            std::fs::remove_file(&final_path)?; // Windows rename 不覆盖已存在文件
        }
        std::fs::rename(&tmp, &final_path)?;
        bytes_written += std::fs::metadata(&final_path).map(|m| m.len()).unwrap_or(0);
        regions_written += 1;
        chunks_written += chunks.len() as u64;
    }

    if skipped > 0 {
        eprintln!(
            "WARN: 维度 {} 有 {skipped} 条记录无法读出（损坏或未知类型），未写回 Anvil",
            dim.path.display()
        );
    }
    println!(
        "维度 {}：{regions_written} 个 region（{chunks_written} 条记录，{bytes_written} 字节）写回 Anvil",
        dim.path.display()
    );
    Ok((regions_written, chunks_written, legacy_raw))
}

// ---------------------------------------------------------------- verify / compact / stats

fn verify(world: &Path) -> anyhow::Result<()> {
    let dims = discover_vstore_roots(world);
    if dims.is_empty() {
        bail!(
            "{} 下未找到 vstore（缺少 {VSTORE_DIR}/{MANIFEST_FILE}）",
            world.display()
        );
    }
    let cfg = load_or_create_template(world)?;
    // 单维度失败不中止：收集后聚合报告，非零退出。
    let mut failures: Vec<(String, anyhow::Error)> = Vec::new();
    for dim in &dims {
        if let Err(e) = verify_dim(dim, &cfg) {
            eprintln!("ERROR: 维度 {} verify 失败：{e:#}", dim.path.display());
            failures.push((dim.label().to_string(), e));
        }
    }
    report_dim_failures("verify", &failures)
}

/// 单维度校验；发现损坏记录 → 返回错误（由 main 层映射为非零退出码）。
fn verify_dim(dim: &DimRoot, cfg: &config::StrataConfig) -> anyhow::Result<()> {
    preflight_vstore(dim, false)?;
    println!("== {} ==", dim.path.display());
    let vstore = dim.path.join(VSTORE_DIR);
    let store = Store::open(&vstore, cfg.store.clone())
        .with_context(|| format!("打开 vstore {}", vstore.display()))?;
    let report = store.verify()?;
    println!("records: {}", report.records);
    println!("corrupt: {}", report.corrupt_records.len());
    for (seg_id, offset) in &report.corrupt_records {
        println!("  seg-{seg_id:04} @ offset {offset}");
    }
    if !report.corrupt_records.is_empty() {
        bail!(
            "维度 {} 存在 {} 条损坏记录",
            dim.label(),
            report.corrupt_records.len()
        );
    }
    Ok(())
}

fn compact(world: &Path) -> anyhow::Result<()> {
    let dims = discover_vstore_roots(world);
    if dims.is_empty() {
        bail!(
            "{} 下未找到 vstore（缺少 {VSTORE_DIR}/{MANIFEST_FILE}）",
            world.display()
        );
    }
    let cfg = load_or_create_template(world)?;
    let mut failures: Vec<(String, anyhow::Error)> = Vec::new();
    for dim in &dims {
        if let Err(e) = compact_dim(dim, &cfg) {
            eprintln!("ERROR: 维度 {} compact 失败：{e:#}", dim.path.display());
            failures.push((dim.label().to_string(), e));
        }
    }
    report_dim_failures("compact", &failures)
}

fn compact_dim(dim: &DimRoot, cfg: &config::StrataConfig) -> anyhow::Result<()> {
    preflight_vstore(dim, false)?;
    println!("== {} ==", dim.path.display());
    let vstore = dim.path.join(VSTORE_DIR);
    let mut store = Store::open(&vstore, cfg.store.clone())
        .with_context(|| format!("打开 vstore {}", vstore.display()))?;

    if !cfg.gc_enabled {
        println!("提示：strata.gc.enabled=false，跳过 GC 阶段（冷热分层照常）");
    }

    let mut reclaimed_total = 0u64;
    let mut segments_removed = 0u32;
    let mut holes_punched = 0u32;
    let mut records_moved = 0u64;
    let mut promoted_total = 0u64;
    let mut demoted_total = 0u64;

    // 循环直到 GC 与分层都无进展（reclaimed==0 且 promoted+demoted==0）。
    loop {
        let gc = if cfg.gc_enabled {
            store.gc_pass(&cfg.gc)?
        } else {
            GcStats::default()
        };
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
    let dims = discover_vstore_roots(world);
    if dims.is_empty() {
        bail!(
            "{} 下未找到 vstore（缺少 {VSTORE_DIR}/{MANIFEST_FILE}）",
            world.display()
        );
    }
    let cfg = load_or_create_template(world)?;
    let mut failures: Vec<(String, anyhow::Error)> = Vec::new();
    for dim in &dims {
        if let Err(e) = stats_dim(dim, &cfg) {
            eprintln!("ERROR: 维度 {} stats 失败：{e:#}", dim.path.display());
            failures.push((dim.label().to_string(), e));
        }
    }
    report_dim_failures("stats", &failures)
}

fn stats_dim(dim: &DimRoot, cfg: &config::StrataConfig) -> anyhow::Result<()> {
    preflight_vstore(dim, false)?;
    println!("== {} ==", dim.path.display());
    let vstore = dim.path.join(VSTORE_DIR);
    let store = Store::open(&vstore, cfg.store.clone())
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

// ---------------------------------------------------------------- recompress

/// 按当前配置重写各维度 vstore 的全部存活记录：
/// 读出 (x, z, type_id, payload) → 写入 vstore.new → 全量读回比对
/// （记录数 + 逐条 xxhash）→ rename 交换（vstore → vstore.old）。
fn recompress(world: &Path) -> anyhow::Result<()> {
    let dims = discover_vstore_roots(world);
    if dims.is_empty() {
        bail!(
            "{} 下未找到可重压缩的 vstore（缺少 {VSTORE_DIR}/{MANIFEST_FILE}）",
            world.display()
        );
    }

    let cfg = load_or_create_template(world)?;
    for w in validate_matrix(&cfg) {
        eprintln!("WARN: {w}");
    }

    // 单维度失败不中止：收集后聚合报告，非零退出。
    let mut failures: Vec<(String, anyhow::Error)> = Vec::new();
    for dim in &dims {
        if let Err(e) = recompress_dim(dim, &cfg) {
            eprintln!("ERROR: 维度 {} 重压缩失败：{e:#}", dim.path.display());
            failures.push((dim.label().to_string(), e));
        }
    }
    report_dim_failures("recompress", &failures)?;
    println!("重压缩完成：共 {} 个维度", dims.len());
    Ok(())
}

/// 单维度重压缩。失败只清理 vstore.new，绝不动原 vstore。
fn recompress_dim(dim: &DimRoot, cfg: &config::StrataConfig) -> anyhow::Result<()> {
    // is_recompress=true：vstore+vstore.old 并存是上次成功重压缩的正常备份态，
    // 由本函数的交换协议接管（下方先删旧备份），不触发人工介入选路。
    preflight_vstore(dim, true)?;
    let vstore = dim.path.join(VSTORE_DIR);
    let new_root = dim.path.join(VSTORE_NEW_DIR);
    let old_root = dim.path.join(VSTORE_OLD_DIR);
    let bytes_before = dir_bytes(&vstore);

    // 清理上次失败遗留的中间目录。
    if new_root.exists() {
        remove_dir_with_retry(&new_root)
            .with_context(|| format!("清理遗留的 {}", new_root.display()))?;
    }

    let records = match recompress_dim_inner(&vstore, &new_root, cfg) {
        Ok(n) => n,
        Err(e) => {
            if new_root.exists() {
                if let Err(rm) = remove_dir_with_retry(&new_root) {
                    eprintln!("WARN: 清理 {} 失败：{rm}", new_root.display());
                }
            }
            return Err(e)
                .with_context(|| format!("维度 {} 重压缩失败", dim.path.display()));
        }
    };

    // 交换：vstore → vstore.old，vstore.new → vstore。
    if old_root.exists() {
        remove_dir_with_retry(&old_root)
            .with_context(|| format!("删除旧备份 {}", old_root.display()))?;
    }
    if let Err(e) = std::fs::rename(&vstore, &old_root) {
        let _ = remove_dir_with_retry(&new_root);
        return Err(e)
            .with_context(|| format!("重命名 {} → {}", vstore.display(), old_root.display()));
    }
    if let Err(e) = std::fs::rename(&new_root, &vstore) {
        // 回滚第一步重命名，保证原 vstore 仍然存在。
        let _ = std::fs::rename(&old_root, &vstore);
        let _ = remove_dir_with_retry(&new_root);
        return Err(e)
            .with_context(|| format!("重命名 {} → {}", new_root.display(), vstore.display()));
    }

    let bytes_after = dir_bytes(&vstore);
    println!(
        "维度 {}：重写 {records} 条记录（{bytes_before} → {bytes_after} 字节），原 vstore 备份至 {}",
        dim.path.display(),
        old_root.display()
    );
    Ok(())
}

/// 重压缩核心（流式，两遍）：第一遍只收集存活键集合；第二遍逐键
/// 读 → 写 vstore.new → 立即读回比对 xxhash。负载全程透传（规范格式
/// gzip(NBT) 对重压缩透明），不把所有解压负载驻留内存。成功返回记录数。
fn recompress_dim_inner(
    vstore: &Path,
    new_root: &Path,
    cfg: &config::StrataConfig,
) -> anyhow::Result<u64> {
    // 1. 存活键集合：热层 = 段扫描 latest gen；冷层 = 冷归档未被失效的槽位。
    let keys: Vec<(i32, i32, u16)> = {
        let hot = latest_gens(vstore)?;
        let cold = cold_live_keys(vstore, &hot)?;
        hot.keys().copied().chain(cold).collect()
    };

    // 2. 逐键读（旧）→ 写（新）→ 读回新存储比对哈希。write 内部会把段写入
    //    刷到 OS（flush_buf），同会话读回无需先整体 flush。
    let store = Store::open(vstore, cfg.store.clone())
        .with_context(|| format!("打开 vstore {}", vstore.display()))?;
    let mut new_store = Store::open(new_root, cfg.store.clone())
        .with_context(|| format!("创建 {}", new_root.display()))?;
    let mut count = 0u64;
    for &(x, z, type_id) in &keys {
        let Some(nbt) = store.read(x, z, type_id)? else {
            bail!(
                "存活记录 ({x}, {z}, type {type_id}) 无法从 {} 读出",
                vstore.display()
            );
        };
        let hash = xxh64(&nbt, 0);
        new_store.write(x, z, type_id, &nbt).with_context(|| {
            format!("写入 ({x}, {z}, type {type_id}) 至 {}", new_root.display())
        })?;
        let Some(back) = new_store.read(x, z, type_id)? else {
            bail!("重压缩后无法读回 ({x}, {z}, type {type_id})");
        };
        if xxh64(&back, 0) != hash {
            bail!("重压缩后 ({x}, {z}, type {type_id}) 负载哈希不一致");
        }
        count += 1;
    }
    new_store.flush().context("flush vstore.new")?;

    // 3. 收尾校验：记录数/损坏数核对。
    let report = new_store.verify()?;
    if report.records != count {
        bail!(
            "重压缩后记录数不符：期望 {count}，盘上扫描 {}",
            report.records
        );
    }
    if !report.corrupt_records.is_empty() {
        bail!("重压缩后出现 {} 条损坏记录", report.corrupt_records.len());
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_overworld_only() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("region")).unwrap();
        let dims = discover_dim_roots(tmp.path());
        assert_eq!(dims.len(), 1);
        assert_eq!(dims[0].label(), ".");
        assert_eq!(dims[0].path, tmp.path());
    }

    #[test]
    fn discover_vanilla_layout() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("region")).unwrap();
        std::fs::create_dir_all(tmp.path().join("DIM-1/region")).unwrap();
        std::fs::create_dir_all(tmp.path().join("DIM1/entities")).unwrap();
        let dims = discover_dim_roots(tmp.path());
        let labels: Vec<&str> = dims.iter().map(|d| d.label()).collect();
        assert_eq!(labels, vec![".", "DIM-1", "DIM1"]);
    }

    #[test]
    fn discover_canvas_layout_sorted() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("region")).unwrap();
        std::fs::create_dir_all(
            tmp.path().join("dimensions/minecraft/the_nether/region"),
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("dimensions/minecraft/the_end/poi")).unwrap();
        let dims = discover_dim_roots(tmp.path());
        let labels: Vec<&str> = dims.iter().map(|d| d.label()).collect();
        assert_eq!(
            labels,
            vec![
                ".",
                "dimensions/minecraft/the_end",
                "dimensions/minecraft/the_nether"
            ]
        );
    }

    #[test]
    fn discover_ignores_invalid_candidates() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("region")).unwrap();
        // DIM-1 / dimensions 子目录存在但不含 region/entities/poi → 无效。
        std::fs::create_dir_all(tmp.path().join("DIM-1")).unwrap();
        std::fs::create_dir_all(tmp.path().join("dimensions/minecraft/custom")).unwrap();
        let dims = discover_dim_roots(tmp.path());
        assert_eq!(dims.len(), 1);
        assert_eq!(dims[0].label(), ".");
    }

    #[test]
    fn discover_empty_world() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(discover_dim_roots(tmp.path()).is_empty());
        assert!(discover_vstore_roots(tmp.path()).is_empty());
    }

    #[test]
    fn discover_vstore_roots_by_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        // vstore 无 manifest.vsm → 不算；有 → 算（即使源目录已删除）。
        std::fs::create_dir_all(tmp.path().join("vstore")).unwrap();
        std::fs::create_dir_all(tmp.path().join("DIM-1/vstore")).unwrap();
        std::fs::write(tmp.path().join("DIM-1/vstore/manifest.vsm"), b"x").unwrap();
        let dims = discover_vstore_roots(tmp.path());
        let labels: Vec<&str> = dims.iter().map(|d| d.label()).collect();
        assert_eq!(labels, vec!["DIM-1"]);
    }

    #[test]
    fn progress_line_format() {
        assert!(is_valid_progress_line(".:chunk:r0.0"));
        assert!(is_valid_progress_line("DIM-1:entity:r3.-2"));
        assert!(is_valid_progress_line(
            "dimensions/minecraft/the_end:poi:r0.0"
        ));
        assert!(!is_valid_progress_line("chunk:r0.0")); // 旧格式（无维度键）
        assert!(!is_valid_progress_line(".:bogus:r0.0"));
        assert!(!is_valid_progress_line(".:chunk:r0"));
        assert!(!is_valid_progress_line(".:chunk:0.0"));
        assert!(!is_valid_progress_line(""));
    }
}
