//! 热↔冷迁移：`tier_pass` 一趟先降级后晋升。
//!
//! * 晋升：存活记录全部稳定（最近写距当前 epoch 至少
//!   [`TierConfig::stable_flushes`] 次 flush）且尚无冷归档的 region，整块
//!   打包进 `cold/r.{rx}.{rz}.varc`（原始 NBT，信封原样保留）。持久化顺序：
//!   `.varc` 落盘 + fsync → manifest 登记冷区 → 清理热层 `.vix`。崩溃基准 =
//!   冷已注册：此后回放跳过冷区键（数据在冷归档），此前热层完好。
//! * 降级：`.varc.inv` 位图 popcount / total_slots 超过
//!   [`TierConfig::invalid_demote_ratio`] 的冷归档整体解包，未失效槽位逐条
//!   [`crate::store::Store::write`] 回热层（新 gen），失效槽位已被更新的热
//!   写覆盖、不得复活旧值。顺序：回写热层（进 epoch 日志，期间抑制冷槽失效
//!   记账）→ manifest 除名并持久化 → 删除 `.varc`/`.varc.inv`（删除失败留待
//!   下轮，不中止）。

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use crate::cold::{ArchiveBuilder, ArchiveReader};
use crate::envelope::Envelope;
use crate::index::{IndexKey, IndexPage, IndexVal, RegionBitmap};
use crate::manifest::{ColdMeta, RegionKey};
use crate::segment::scan_segment;
use crate::store::{
    cold_path, ix_path, seg_path, write_index_page, MutexExt, Store, COLD_DIR,
};
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
    ///
    /// 判据是 `.varc.inv` 位图的 popcount（[`ArchiveReader::invalid_count`]），
    /// 不用 manifest 的 `invalid_count`——后者只是记账提示，可能漂移。
    fn demote_pass(&mut self, cfg: &TierConfig, stats: &mut TierStats) -> Result<(), StrataError> {
        let regions: Vec<(i32, i32)> = self
            .manifest
            .cold
            .iter()
            .map(|c| (c.region_x, c.region_z))
            .collect();

        for (rx, rz) in regions {
            let path = cold_path(&self.root, rx, rz);
            let mut reader = match ctx(ArchiveReader::open(&path), "打开冷归档", &path) {
                Ok(r) => r,
                Err(_) => {
                    // 已登记但不可读：除名让读路径回落热层，避免 tier_pass
                    // 永远卡死在坏归档上。文件留给 open 对账清理。
                    self.unregister_cold(rx, rz)?;
                    continue;
                }
            };

            let total = reader.total_slots();
            let eligible = total > 0
                && (reader.invalid_count() as f64) / (total as f64)
                    > cfg.invalid_demote_ratio;
            if !eligible {
                continue;
            }

            // 只搬回未被失效的槽位：失效槽位已被更新的热写覆盖，
            // 以新 gen 写回旧值会让用户写入被静默回滚。
            // 槽位信封校验失败（Corrupt）视为不可见：不回写不可信数据。
            let mut keep: Vec<(Envelope, Vec<u8>)> = Vec::new();
            for (env, nbt) in ctx(reader.extract_all(), "解包冷归档", &path)? {
                match reader.get(env.chunk_x, env.chunk_z, env.type_id) {
                    Ok(Some(_)) => keep.push((env, nbt)),
                    Ok(None) => {}
                    Err(_) => {}
                }
            }
            // 关闭句柄：Windows 下持句柄会导致后续删除失败。
            drop(reader);

            // 顺序第 1 步：回写热层（进 epoch 日志）。期间抑制冷槽失效记账：
            // 若照常吃记账，崩溃在"回写已进日志、manifest 未除名"窗口时，
            // 回放跳过冷区键（回写丢失）而槽位已失效——两个副本同时蒸发。
            self.demote_in_progress = true;
            let res: Result<(), StrataError> = keep.iter().try_for_each(|(env, nbt)| {
                self.write_durable(env.chunk_x, env.chunk_z, env.type_id, nbt)
                    .map_err(|e| {
                        StrataError::Manifest(format!(
                            "tier: 降级回写 ({}, {}, type={}) 至热层失败: {e}",
                            env.chunk_x, env.chunk_z, env.type_id
                        ))
                    })
            });
            self.demote_in_progress = false;
            res?;

            // 第 2 步：除名冷区并持久化。此后崩溃回放正常应用热回写
            // （region 不在 manifest.cold，不再被跳过）。
            self.manifest
                .cold
                .retain(|c| !(c.region_x == rx && c.region_z == rz));
            self.rebuild_cold_lookup();
            self.cold_readers
                .lock()
                .unwrap_or_poisoned()
                .remove(&RegionKey { x: rx, z: rz });
            ctx(
                self.manifest.save(&self.root),
                "保存 manifest（降级除名）",
                &self.root,
            )?;

            // 第 3 步：删除归档文件。失败留待下轮（open 对账会把孤儿归档
            // 重新注册，下轮再降级删除），不中止本轮迁移。
            if path.exists() {
                let _ = crate::store::remove_file_with_retry(&path);
            }
            let inv = path.with_extension("varc.inv");
            if inv.exists() {
                let _ = crate::store::remove_file_with_retry(&inv);
            }
            stats.demoted += 1;
        }

        ctx(
            self.manifest.save(&self.root),
            "保存 manifest（降级收尾）",
            &self.root,
        )?;
        Ok(())
    }

    /// 除名一个冷区（manifest + 读取器缓存）并持久化。
    fn unregister_cold(&mut self, rx: i32, rz: i32) -> Result<(), StrataError> {
        self.manifest
            .cold
            .retain(|c| !(c.region_x == rx && c.region_z == rz));
        self.rebuild_cold_lookup();
        self.cold_readers
            .lock()
            .unwrap_or_poisoned()
            .remove(&RegionKey { x: rx, z: rz });
        ctx(
            self.manifest.save(&self.root),
            "保存 manifest（冷区除名）",
            &self.root,
        )?;
        Ok(())
    }

    /// 晋升：稳定 region 打包为冷归档，并从热层索引/位图移除其键。
    ///
    /// 崩溃基准 = 冷已注册：`.varc` 落盘 + fsync 后立即 manifest 登记并
    /// 保存，之后才清理热层索引。登记后崩溃 → 回放跳过冷区键，数据在冷
    /// 归档；登记前崩溃 → 热层完好，孤儿 `.varc` 由 open 对账接管。
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
                .cold_lookup
                .contains_key(&RegionKey { x: rk.0, z: rk.1 });
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

            // 5a. 注册冷区并立即持久化（崩溃基准：冷已注册）。
            self.manifest.cold.push(ColdMeta {
                region_x: rk.0,
                region_z: rk.1,
                invalid_count: 0,
                total_slots: entries,
            });
            self.rebuild_cold_lookup();
            ctx(
                self.manifest.save(&self.root),
                "保存 manifest（冷区注册）",
                &self.root,
            )?;

            // 5b. 从热层移除全部键。中途崩溃：冷基准已确立，未 purge 的键
            //    热层仍可读（冷归档是它们的备份副本，读路径热优先）。
            self.purge_region_keys(&RegionKey { x: rk.0, z: rk.1 }, &keys)?;
            stats.promoted += 1;
        }

        ctx(
            self.manifest.save(&self.root),
            "保存 manifest（晋升收尾）",
            &self.root,
        )?;
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
            let page = ctx(
                self.load_page(seg_id),
                "读段索引页（判定受影响段）",
                &ix_path(&self.root, seg_id),
            )?;
            let page_has = page.is_some_and(|p| p.iter().any(|(k, _)| in_region(k)));
            let inc_has = st.incremental.iter().any(|(k, _)| in_region(k));
            if page_has || inc_has {
                touched.push(seg_id);
            }
        }

        for seg_id in touched {
            // 重建磁盘页：过滤掉 region 条目后原子覆盖替换（tmp + fsync +
            // rename，不预删旧文件）。
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
            let new_page = Arc::new(IndexPage::from_entries(kept));
            ctx(
                write_index_page(&self.root, seg_id, &new_page),
                "替换段索引页",
                &ix_path(&self.root, seg_id),
            )?;

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
            self.cache.lock().unwrap_or_poisoned().put(seg_id, new_page);
        }
        Ok(())
    }
}
