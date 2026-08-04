//! 热↔冷迁移：`tier_pass` 一趟先降级后晋升。
//!
//! * 晋升：存活记录全部稳定（最近写距当前 epoch 至少
//!   [`TierConfig::stable_flushes`] 次 flush）且尚无冷归档的 region，整块
//!   打包进 `cold/r.{rx}.{rz}.varc`（原始 NBT，信封原样保留），随后从热层
//!   索引与位图中移除这些键；冷读由 [`crate::store::Store::read`] 的冷查
//!   路径透明兜底。
//! * 降级：`invalid_count / total_slots` 超过
//!   [`TierConfig::invalid_demote_ratio`] 的冷归档整体解包，未失效槽位逐条
//!   [`crate::store::Store::write`] 回热层（新 gen），失效槽位已被更新的热
//!   写覆盖、不得复活旧值；随后删除 `.varc`/`.varc.inv` 与冷区元数据。

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::path::Path;

use crate::cold::{ArchiveBuilder, ArchiveReader};
use crate::envelope::Envelope;
use crate::index::{IndexKey, IndexPage, IndexVal, RegionBitmap};
use crate::manifest::{ColdMeta, RegionKey};
use crate::segment::scan_segment;
use crate::store::{cold_path, ix_path, remove_file_with_retry, seg_path, Store, COLD_DIR};
use crate::StrataError;

/// 分层迁移配置。
#[derive(Debug, Clone)]
pub struct TierConfig {
    /// 总开关：`false` 时 `tier_pass` 是纯 no-op（纯热模式）。
    pub enabled: bool,
    /// 晋升稳定性窗口：region 最近一次写入距今至少这么多次 flush。
    pub stable_flushes: u32,
    /// 降级阈值：`invalid_count / total_slots` 超过它即解包回热层。
    pub invalid_demote_ratio: f64,
}

impl Default for TierConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            stable_flushes: 30,
            invalid_demote_ratio: 0.25,
        }
    }
}

/// 单次 `tier_pass` 的统计。
#[derive(Debug, Default, Clone, Copy)]
pub struct TierStats {
    /// 本轮晋升为冷归档的 region 数。
    pub promoted: u32,
    /// 本轮解包回热层的冷归档数。
    pub demoted: u32,
    /// 本轮结束时全部 `.varc` 文件的字节总和。
    pub bytes_cold: u64,
}

/// 给已是 `StrataError` 的结果附加“操作名 + 路径”上下文。
///
/// Windows 上裸 `Os { code: 5, "Access is denied" }` 不带任何线索，
/// tier 路径全部 IO 出口统一经此包装，CI 报错可直接定位步骤与文件。
fn ctx<T>(res: Result<T, StrataError>, op: &str, path: &Path) -> Result<T, StrataError> {
    res.map_err(|e| {
        StrataError::Manifest(format!("tier: {op} `{}` 失败: {e}", path.display()))
    })
}

/// 同 [`ctx`]，用于原始 `io::Result`。
fn ctx_io<T>(res: std::io::Result<T>, op: &str, path: &Path) -> Result<T, StrataError> {
    res.map_err(|e| {
        StrataError::Manifest(format!("tier: {op} `{}` 失败: {e}", path.display()))
    })
}

impl Store {
    /// 热↔冷迁移一趟：降级在前（回热后当轮即有资格再晋升），晋升在后。
    ///
    /// 开始前关闭活跃写入器（同 `gc_pass`：避免 Windows 句柄占用，也让
    /// 未落盘增量先经段文件 fsync 定型）。
    pub fn tier_pass(&mut self, cfg: &TierConfig) -> Result<TierStats, StrataError> {
        let mut stats = TierStats::default();
        if !cfg.enabled {
            return Ok(stats);
        }

        if let Some(mut w) = self.writer.take() {
            let sp = seg_path(&self.root, self.active_seg);
            ctx(w.fsync(), "关闭活跃段前 fsync", &sp)?;
            ctx(w.close(), "关闭活跃段写入器", &sp)?;
        }

        self.demote_pass(cfg, &mut stats)?;
        if self.cfg.cold_enabled {
            self.promote_pass(cfg, &mut stats)?;
        }

        for cm in &self.manifest.cold {
            if let Ok(meta) = std::fs::metadata(cold_path(&self.root, cm.region_x, cm.region_z)) {
                stats.bytes_cold += meta.len();
            }
        }
        Ok(stats)
    }

    /// 降级：失效占比超阈值的冷归档解包回热层并除名。
    fn demote_pass(&mut self, cfg: &TierConfig, stats: &mut TierStats) -> Result<(), StrataError> {
        let cands: Vec<ColdMeta> = self
            .manifest
            .cold
            .iter()
            .filter(|c| {
                c.total_slots > 0
                    && (c.invalid_count as f64) / (c.total_slots as f64)
                        > cfg.invalid_demote_ratio
            })
            .cloned()
            .collect();

        for cm in cands {
            let path = cold_path(&self.root, cm.region_x, cm.region_z);
            // 只搬回未被失效的槽位：失效槽位已被更新的热写覆盖，
            // 以新 gen 写回旧值会让用户写入被静默回滚。
            let entries = {
                let mut reader = ctx(ArchiveReader::open(&path), "打开冷归档", &path)?;
                let mut keep = Vec::new();
                for (env, nbt) in ctx(reader.extract_all(), "解包冷归档", &path)? {
                    let visible = ctx(
                        reader.get(env.chunk_x, env.chunk_z, env.type_id),
                        "复核冷归档槽位",
                        &path,
                    )?;
                    if visible.is_some() {
                        keep.push((env, nbt));
                    }
                }
                keep
            };

            // 先除名（manifest + 读取器缓存）再回写：回写的键不会再触发
            // 冷区失效记账，也避免 Windows 下持有句柄导致文件删不掉。
            self.manifest
                .cold
                .retain(|c| !(c.region_x == cm.region_x && c.region_z == cm.region_z));
            self.cold_readers.borrow_mut().remove(&RegionKey {
                x: cm.region_x,
                z: cm.region_z,
            });

            for (env, nbt) in entries {
                self.write(env.chunk_x, env.chunk_z, env.type_id, &nbt)
                    .map_err(|e| {
                        StrataError::Manifest(format!(
                            "tier: 降级回写 ({}, {}, type={}) 至热层失败: {e}",
                            env.chunk_x, env.chunk_z, env.type_id
                        ))
                    })?;
            }

            // Windows 上杀软/索引器可能短暂锁定文件：删除带重试
            // （此时本地与缓存的读取器均已 drop，无打开句柄）。
            if path.exists() {
                ctx_io(remove_file_with_retry(&path), "删除冷归档", &path)?;
            }
            let inv = path.with_extension("varc.inv");
            if inv.exists() {
                ctx_io(remove_file_with_retry(&inv), "删除冷归档失效位图", &inv)?;
            }
            stats.demoted += 1;
        }

        ctx(self.manifest.save(&self.root), "保存 manifest（降级收尾）", &self.root)?;
        Ok(())
    }

    /// 晋升：稳定 region 打包为冷归档，并从热层索引/位图移除其键。
    fn promote_pass(&mut self, cfg: &TierConfig, stats: &mut TierStats) -> Result<(), StrataError> {
        // 1. 全局最新视图判死活（同 gc），段按 id 升序扫描收集每 region
        //    的存活记录（键 + 原始信封）与最大 epoch_ts。
        let latest = ctx(self.latest_index(), "构建最新索引视图", &self.root)?;
        let mut ids: Vec<u32> = self.manifest.segments.iter().map(|m| m.id).collect();
        ids.sort_unstable();

        let mut live: HashMap<IndexKey, ((i32, i32), Envelope)> = HashMap::new();
        let mut max_ts: HashMap<(i32, i32), u32> = HashMap::new();
        for id in ids {
            let scan = match scan_segment(&seg_path(&self.root, id)) {
                Ok(s) => s,
                Err(_) => continue, // 不可读的段留给 verify/rebuild 路径
            };
            for rec in scan.records {
                let key = IndexKey {
                    x: rec.env.chunk_x,
                    z: rec.env.chunk_z,
                    type_id: rec.env.type_id,
                };
                let is_live = match latest.get(&key) {
                    Some(v) => {
                        v.seg_id == id
                            && v.offset == rec.offset
                            && v.gen == rec.env.gen
                            && rec.env.payload_hash != 0
                    }
                    None => false,
                };
                if !is_live {
                    continue;
                }
                let rk = (rec.env.chunk_x >> 5, rec.env.chunk_z >> 5);
                live.insert(key, (rk, rec.env.clone()));
                let ts = max_ts.entry(rk).or_insert(0);
                if rec.env.epoch_ts > *ts {
                    *ts = rec.env.epoch_ts;
                }
            }
        }

        // 2. 按 region 分组、坐标序处理：稳定、未归档、全部存活键可读。
        let mut regions: HashMap<(i32, i32), Vec<IndexKey>> = HashMap::new();
        for (k, (rk, _)) in &live {
            regions.entry(*rk).or_default().push(k.clone());
        }
        let mut region_ids: Vec<(i32, i32)> = regions.keys().copied().collect();
        region_ids.sort_unstable();

        for rk in region_ids {
            let mut keys = regions.remove(&rk).expect("iterated from keys");
            let ts = match max_ts.get(&rk) {
                Some(&t) => t,
                None => continue,
            };
            let stable = (ts as u64) + (cfg.stable_flushes as u64) <= self.manifest.epoch;
            let colded = self
                .manifest
                .cold
                .iter()
                .any(|c| c.region_x == rk.0 && c.region_z == rk.1);
            if !stable || colded {
                continue;
            }
            keys.sort();

            // 3. 逐键读原始 NBT；任何键读不出（损坏隔离）→ 本轮跳过该 region。
            let mut builder = ArchiveBuilder::new(rk.0, rk.1, self.cfg.cold_level, None);
            let mut entries = 0u32;
            let mut ok = true;
            for k in &keys {
                let (_, env) = &live[k];
                let nbt = self.read(k.x, k.z, k.type_id).map_err(|e| {
                    StrataError::Manifest(format!(
                        "tier: 晋升前读键 ({}, {}, type={}) 失败: {e}",
                        k.x, k.z, k.type_id
                    ))
                })?;
                match nbt {
                    Some(nbt) => {
                        builder.add(env.clone(), nbt);
                        entries += 1;
                    }
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            if !ok {
                continue;
            }

            // 4. 落盘 + fsync。
            let cold_dir = self.root.join(COLD_DIR);
            std::fs::create_dir_all(&cold_dir).map_err(|e| {
                StrataError::Manifest(format!(
                    "tier: 创建冷目录 `{}` 失败: {e}",
                    cold_dir.display()
                ))
            })?;
            let path = cold_path(&self.root, rk.0, rk.1);
            ctx(builder.finish(&path), "写冷归档", &path)?;
            // 根因修复：Windows 的 FlushFileBuffers 要求 GENERIC_WRITE，
            // 只读句柄 sync_all 得 ERROR_ACCESS_DENIED (os error 5)；
            // Unix fsync 对 O_RDONLY 无此限制，故仅 Windows 失败。
            ctx_io(
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .and_then(|f| f.sync_all()),
                "fsync 冷归档",
                &path,
            )?;

            // 5. 注册冷区并从热层移除全部键。
            self.manifest.cold.push(ColdMeta {
                region_x: rk.0,
                region_z: rk.1,
                invalid_count: 0,
                total_slots: entries,
            });
            self.purge_region_keys(&RegionKey { x: rk.0, z: rk.1 }, &keys)?;
            stats.promoted += 1;
        }

        ctx(self.manifest.save(&self.root), "保存 manifest（晋升收尾）", &self.root)?;
        Ok(())
    }

    /// 从热层移除 `keys`：逐段重建磁盘页与增量，再按剩余条目重建位图与缓存。
    fn purge_region_keys(
        &mut self,
        rk: &RegionKey,
        keys: &[IndexKey],
    ) -> Result<(), StrataError> {
        let key_set: HashSet<&IndexKey> = keys.iter().collect();
        let in_region = |k: &IndexKey| (k.x >> 5) == rk.x && (k.z >> 5) == rk.z;

        // 受影响段 = 磁盘页或增量中含该 region 条目者。
        let mut touched: Vec<u32> = Vec::new();
        for (&seg_id, st) in &self.segs {
            let page_has = ctx(
                self.load_page(seg_id),
                "读段索引页（判定受影响段）",
                &ix_path(&self.root, seg_id),
            )?
            .is_some_and(|p| p.iter().any(|(k, _)| in_region(k)));
            let inc_has = st.incremental.iter().any(|(k, _)| in_region(k));
            if page_has || inc_has {
                touched.push(seg_id);
            }
        }

        for seg_id in touched {
            // 重建磁盘页：过滤掉 region 条目后原子替换（tmp + fsync + rename）。
            let kept: Vec<(IndexKey, IndexVal)> = match ctx(
                self.load_page(seg_id),
                "读段索引页（重建）",
                &ix_path(&self.root, seg_id),
            )? {
                Some(page) => page
                    .iter()
                    .filter(|(k, _)| !key_set.contains(k))
                    .cloned()
                    .collect(),
                None => Vec::new(),
            };
            let new_page = std::sync::Arc::new(IndexPage::from_entries(kept));
            let bytes = new_page.serialize();
            let final_path = ix_path(&self.root, seg_id);
            let tmp_path = final_path.with_extension("vix.tmp");
            {
                use std::io::Write;
                let mut f = ctx_io(File::create(&tmp_path), "创建临时索引页", &tmp_path)?;
                ctx_io(f.write_all(&bytes), "写临时索引页", &tmp_path)?;
                ctx_io(f.sync_all(), "fsync 临时索引页", &tmp_path)?;
            }
            // Windows 删除带重试（此时 .vix 无打开句柄：页只存在于内存缓存）。
            if final_path.exists() {
                ctx_io(remove_file_with_retry(&final_path), "删除旧索引页", &final_path)?;
            }
            ctx_io(std::fs::rename(&tmp_path, &final_path), "重命名临时索引页", &final_path)?;

            // 增量同样过滤；位图无单槽清除，按新页 ∪ 新增量整体重建。
            if let Some(st) = self.segs.get_mut(&seg_id) {
                st.incremental.retain(|(k, _)| !key_set.contains(k));
                let mut bitmap = RegionBitmap::new();
                for (k, _) in new_page.iter() {
                    bitmap.set(k.x, k.z, k.type_id);
                }
                for (k, _) in &st.incremental {
                    bitmap.set(k.x, k.z, k.type_id);
                }
                st.bitmap = bitmap;
            }
            self.cache.put(seg_id, new_page);
        }
        Ok(())
    }
}
