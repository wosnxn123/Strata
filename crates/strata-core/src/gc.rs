//! 三档 GC：整段删除 → hole-punch → 打分压实。
//!
//! 以 [`Store::latest_index`]（每键最大 gen 的全局视图）为死活判据，段内
//! 扫描记录三分类：
//! - latest 指向它且负载哈希有效 → **存活**；
//! - latest 指向它但 `payload_hash == 0` → **损坏的最新记录**
//!   （live-but-unreadable）：不参与挖洞/整段删除/压实，避免销毁唯一副本；
//! - 其余（被更高 gen 遮蔽或无索引）→ **死记录**，可回收。

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::sync::Arc;

use crate::envelope::ENVELOPE_SIZE;
use crate::index::{IndexKey, IndexPage, IndexVal};
use crate::manifest::Bucket;
use crate::punch::{punch_hole, PunchOutcome};
use crate::segment::{scan_segment, ScannedRecord};
use crate::store::{ix_path, remove_file_with_retry, seg_path, write_index_page, MutexExt, Store};
use crate::StrataError;

/// 分桶晋升阈值：Young → Active 所需的 flush 次数。
const ACTIVE_AFTER_FLUSHES: u64 = 2;

/// 单次挖洞子区间长度上限。挖洞本身只碰负载、保留信封壳（扫描安全不变量
/// 见档位 2 注释）；≤32KB 的子区间是纵深防御：一旦壳也损坏迫使扫描器走
/// 重同步路径，64KB 窗口（`segment::RESYNC_WINDOW`）内必有下一个洞边界外
/// 的有效记录可供找回，同时契合 Linux 块对齐收缩的粒度。
const PUNCH_MAX_CHUNK: u64 = 32 * 1024;

/// GC 配置。
#[derive(Debug, Clone)]
pub struct GcConfig {
    /// 死字节占比达到该比例才考虑 hole-punch / 压实。
    pub invalid_threshold: f64,
    /// 单次压实搬迁的存活字节预算。
    pub budget_bytes: u64,
    /// 可挖洞的最小失效区间长度。
    pub min_hole_bytes: u64,
}

impl Default for GcConfig {
    fn default() -> Self {
        Self {
            invalid_threshold: 0.6,
            budget_bytes: 32 * 1024 * 1024,
            min_hole_bytes: 64 * 1024,
        }
    }
}

/// 单次 `gc_pass` 的回收统计。
#[derive(Debug, Default, Clone, Copy)]
pub struct GcStats {
    /// 回收的字节数（整段 total + 挖洞区间长 + 压实净回收）。
    pub reclaimed_bytes: u64,
    /// 被整段删除的段数。
    pub segments_removed: u32,
    /// 成功挖洞的区间数。
    pub holes_punched: u32,
    /// 压实搬迁的记录数。
    pub records_moved: u64,
}

/// 分桶晋升（flush 收尾调用）。
///
/// Phase 1 只实现 Young → Active（`epoch_flush_count >= 2`）；
/// Stable 判定（gen 距离 + 30 次 flush 未重写）留给 tier 任务。
pub(crate) fn advance_buckets(store: &mut Store) {
    if store.epoch_flush_count < ACTIVE_AFTER_FLUSHES {
        return;
    }
    for m in store.manifest.segments.iter_mut() {
        if m.bucket == Bucket::Young {
            m.bucket = Bucket::Active;
        }
    }
}

/// 压实候选：段 id、得分、扫描出的存活记录快照、段规模与已挖洞字节数。
struct CompactCandidate {
    id: u32,
    score: f64,
    live_records: Vec<ScannedRecord>,
    live_bytes: u64,
    /// 压实前该段在段表中的 total_bytes。
    old_total: u64,
    /// 档位 2 已记账的挖洞字节数（压实回收里扣除，避免重复计数）。
    punched: u64,
}

impl Store {
    /// 合并所有段的磁盘索引页 + 未落盘增量，同键留最大 gen。
    ///
    /// 这是 GC 判定记录死活的唯一依据。
    pub(crate) fn latest_index(&self) -> Result<HashMap<IndexKey, IndexVal>, StrataError> {
        let mut latest: HashMap<IndexKey, IndexVal> = HashMap::new();
        let ids: Vec<u32> = self.segs.keys().copied().collect();
        for id in ids {
            if let Some(page) = self.load_page(id)? {
                for (k, v) in page.iter() {
                    match latest.get(k) {
                        Some(e) if e.gen >= v.gen => {}
                        _ => {
                            latest.insert(k.clone(), *v);
                        }
                    }
                }
            }
            if let Some(st) = self.segs.get(&id) {
                for (k, v) in &st.incremental {
                    match latest.get(k) {
                        Some(e) if e.gen >= v.gen => {}
                        _ => {
                            latest.insert(k.clone(), *v);
                        }
                    }
                }
            }
        }
        Ok(latest)
    }

    /// 三档 GC 一遍：整段删除 → hole-punch → 打分压实。
    pub fn gc_pass(&mut self, cfg: &GcConfig) -> Result<GcStats, StrataError> {
        let mut stats = GcStats::default();

        // GC 期间不持有任何段文件的写句柄（Windows 下删除打开的文件会失败；
        // 压实也不应搬走活跃写入器名下的段）。关闭后下次 write 按需开新段。
        if let Some(mut w) = self.writer.take() {
            w.fsync()?;
            w.close()?;
        }

        // 判据快照先于任何段变更；压实搬迁时回写被搬记录的最新指向。
        let mut latest = self.latest_index()?;
        let mut candidates: Vec<CompactCandidate> = Vec::new();

        let mut ids: Vec<u32> = self.manifest.segments.iter().map(|m| m.id).collect();
        ids.sort_unstable();

        for id in ids {
            let path = seg_path(&self.root, id);
            let scan = match scan_segment(&path) {
                Ok(s) => s,
                Err(_) => continue, // 不可读的段留给 verify/rebuild 路径
            };

            let mut total = 0u64;
            let mut dead = 0u64;
            let mut live_count = 0usize;
            let mut corrupt_latest = 0usize;
            let mut live_records: Vec<ScannedRecord> = Vec::new();
            let mut spans: Vec<(u64, u64)> = Vec::new(); // 死记录 [start, end)
            // 死记录负载区间（不含信封壳）：挖洞只碰负载，壳永远保留。
            let mut dead_payloads: Vec<(u64, u64)> = Vec::new();

            for rec in &scan.records {
                let rec_bytes = ENVELOPE_SIZE as u64 + rec.env.payload_len as u64;
                total += rec_bytes;
                let key = IndexKey {
                    x: rec.env.chunk_x,
                    z: rec.env.chunk_z,
                    type_id: rec.env.type_id,
                };
                let is_latest = matches!(latest.get(&key),
                    Some(v) if v.seg_id == id && v.offset == rec.offset && v.gen == rec.env.gen);
                if is_latest && rec.env.payload_hash != 0 {
                    live_count += 1;
                    live_records.push(ScannedRecord {
                        env: rec.env.clone(),
                        offset: rec.offset,
                        payload: rec.payload.clone(),
                    });
                } else if is_latest {
                    // 损坏的最新记录：唯一副本不可读但绝不销毁，也不得
                    // 计入死字节（它的区间不参与任何回收动作）。
                    corrupt_latest += 1;
                } else {
                    dead += rec_bytes;
                    spans.push((rec.offset, rec.offset + rec_bytes));
                    if rec.env.payload_len > 0 {
                        dead_payloads.push((
                            rec.offset + ENVELOPE_SIZE as u64,
                            rec.offset + rec_bytes,
                        ));
                    }
                }
            }

            // —— 档位 1：整段删除（无存活且无损坏最新才可删）——
            if live_count == 0 && corrupt_latest == 0 {
                self.remove_segment(id)?;
                stats.reclaimed_bytes += total;
                stats.segments_removed += 1;
                continue;
            }

            if dead == 0 {
                continue;
            }
            let (seg_total, seg_created) = match self.manifest.segments.iter().find(|m| m.id == id)
            {
                Some(m) => (m.total_bytes, m.created_epoch),
                None => continue,
            };
            if (dead as f64) / (seg_total.max(1) as f64) < cfg.invalid_threshold {
                continue;
            }

            // —— 档位 2：hole-punch 死记录负载 ——
            //
            // 扫描安全不变量：**只挖负载、保留全部 40B 信封壳**（含段内最后
            // 一条记录的壳——"尾洞变体"）。扫描器沿保留的壳正常行走：归零的
            // 负载只会在哈希校验处被标记损坏，绝不会进入"零头→MAGIC 重同步"
            // 路径，因此任意长度的死区间挖洞后段仍可扫描/verify/rebuild。
            // 若连壳挖掉，>64KB（重同步窗口）的洞会让扫描器永久丢失同步。
            //
            // 每段负载区间再切 ≤32KB 子区间逐次调用 punch（PUNCH_MAX_CHUNK：
            // 在 64KB 重同步窗口内留足余量，也契合 Linux 块对齐收缩的粒度）。
            spans.sort_unstable();
            let merged = merge_spans(&spans);
            // 可挖区间 = 达到 min_hole_bytes 门槛的合并死区间。
            let eligible: Vec<(u64, u64)> = merged
                .into_iter()
                .filter(|&(s, e)| e - s >= cfg.min_hole_bytes)
                .collect();
            let mut remaining_dead = dead;
            let mut punched = 0u64;
            if !eligible.is_empty() {
                let mut f = OpenOptions::new().read(true).write(true).open(&path)?;
                for &(pay_start, pay_end) in &dead_payloads {
                    // 仅挖落在合格合并区间内的死记录（壳在区间计数里但永不挖）。
                    let rec_off = pay_start - ENVELOPE_SIZE as u64;
                    if !eligible.iter().any(|&(s, e)| rec_off >= s && rec_off < e) {
                        continue;
                    }
                    let mut off = pay_start;
                    while off < pay_end {
                        let len = (pay_end - off).min(PUNCH_MAX_CHUNK);
                        match punch_hole(&mut f, off, len)? {
                            PunchOutcome::Done => {
                                stats.holes_punched += 1;
                                stats.reclaimed_bytes += len;
                                punched += len;
                                remaining_dead = remaining_dead.saturating_sub(len);
                            }
                            PunchOutcome::Unsupported => {}
                        }
                        off += len;
                    }
                }
            }

            // —— 档位 3 候选：punch 未消化全部死字节，且段内无损坏最新记录 ——
            if remaining_dead == 0 {
                continue;
            }
            if corrupt_latest > 0 {
                // 压实 = 搬迁存活记录后删除旧段；不可读的损坏记录搬不走，
                // 删除旧段等于销毁唯一副本。留给 verify 报告人工处置。
                continue;
            }
            let age = self.manifest.epoch.saturating_sub(seg_created).max(1);
            let score =
                (dead as f64 / seg_total.max(1) as f64) * (seg_total as f64) / age as f64;
            let live_bytes: u64 = live_records
                .iter()
                .map(|r| ENVELOPE_SIZE as u64 + r.env.payload_len as u64)
                .sum();
            candidates.push(CompactCandidate {
                id,
                score,
                live_records,
                live_bytes,
                old_total: seg_total,
                punched,
            });
        }

        // —— 档位 3：打分压实（score 降序，预算内搬迁）——
        candidates.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut moved_bytes = 0u64;
        for cand in candidates {
            if moved_bytes.saturating_add(cand.live_bytes) > cfg.budget_bytes {
                continue;
            }
            let (_, moved) = compact_segment(self, cand.id, cand.live_records, &mut latest)?;
            // 净回收 = 删除的旧段 - 搬进新段的存活字节（扣除已记账的挖洞）。
            stats.reclaimed_bytes += cand
                .old_total
                .saturating_sub(cand.live_bytes)
                .saturating_sub(cand.punched);
            moved_bytes += cand.live_bytes;
            stats.records_moved += moved;
        }

        self.manifest.save(&self.root)?;
        Ok(stats)
    }

    /// `(live_bytes 总和, total_bytes 总和)`，来自段表。
    pub fn touch_stats(&self) -> (u64, u64) {
        self.manifest
            .segments
            .iter()
            .fold((0, 0), |(l, t), m| (l + m.live_bytes, t + m.total_bytes))
    }
}

/// 合并重叠/相邻的 `[start, end)` 区间。输入需已按 start 排序。
fn merge_spans(spans: &[(u64, u64)]) -> Vec<(u64, u64)> {
    let mut out: Vec<(u64, u64)> = Vec::new();
    for &(s, e) in spans {
        if let Some(last) = out.last_mut() {
            if s <= last.1 {
                last.1 = last.1.max(e);
                continue;
            }
        }
        out.push((s, e));
    }
    out
}

/// 档位 3 压实：把旧段的存活记录原样搬进新段并删除旧段。
///
/// 持久化顺序（崩溃在任何一点都不丢数据）：
/// 1. `alloc_segment`：新段文件创建 + fsync + manifest 登记保存；
/// 2. 搬迁记录追加进新段 + fsync 段数据；
/// 3. 新段 `.vix` 索引页落盘（tmp + fsync + 覆盖 rename）；
/// 4. `manifest.save`：新段记账、旧段除名；
/// 5. 删除旧段文件与索引页。
///
/// 搬迁记录不再依赖后续 flush 才有索引：第 3 步已持久化 `.vix`，
/// 因此也不写入新段的 `incremental`。搬迁不改动信封任何字段。
fn compact_segment(
    store: &mut Store,
    old_id: u32,
    live: Vec<ScannedRecord>,
    latest: &mut HashMap<IndexKey, IndexVal>,
) -> Result<(u32, u64), StrataError> {
    let bucket = store
        .manifest
        .segments
        .iter()
        .find(|m| m.id == old_id)
        .map(|m| m.bucket)
        .unwrap_or(Bucket::Young);

    // 1. 新段（alloc 内部已：文件头落盘 → manifest 登记保存）。
    let (new_id, mut w) = store.alloc_segment(bucket)?;

    // 2. 搬迁。
    let mut entries: Vec<(IndexKey, IndexVal)> = Vec::with_capacity(live.len());
    let mut moved_bytes = 0u64;
    let mut moved = 0u64;
    for rec in live {
        let offset = w.append(&rec.env, &rec.payload)?;
        let key = IndexKey {
            x: rec.env.chunk_x,
            z: rec.env.chunk_z,
            type_id: rec.env.type_id,
        };
        let st = store
            .segs
            .get_mut(&new_id)
            .expect("alloc_segment inserted state");
        st.bitmap.set(key.x, key.z, key.type_id);
        let val = IndexVal {
            seg_id: new_id,
            offset,
            payload_len: rec.env.payload_len,
            gen: rec.env.gen,
            comp_id: rec.env.comp_id,
        };
        latest.insert(key.clone(), val);
        entries.push((key, val));
        moved_bytes += ENVELOPE_SIZE as u64 + rec.env.payload_len as u64;
        moved += 1;
    }
    w.fsync()?;
    w.close()?;

    // 3. 新段索引页持久化（此刻崩溃：新段已登记但索引为空 → 搬迁记录不可见，
    //    旧段完好无损，下轮 GC 会把新段当全死段回收）。
    let page = Arc::new(IndexPage::from_entries(entries));
    write_index_page(&store.root, new_id, &page)?;
    store.cache.lock().unwrap_or_poisoned().put(new_id, page);

    // 4. manifest：新段记账 + 旧段除名，持久化后才可删旧。
    if let Some(m) = store.manifest.segments.iter_mut().find(|m| m.id == new_id) {
        m.live_bytes = moved_bytes;
        m.total_bytes = moved_bytes;
        m.last_rewrite_epoch = store.manifest.epoch;
    }
    store.manifest.segments.retain(|m| m.id != old_id);
    store.manifest.save(&store.root)?;

    // 5. 替代物已全部持久化：删除旧段。
    store.segs.remove(&old_id);
    store.cache.lock().unwrap_or_poisoned().evict(old_id);
    let p = seg_path(&store.root, old_id);
    if p.exists() {
        remove_file_with_retry(&p).map_err(|e| {
            StrataError::Manifest(format!("压实后删除旧段 `{}` 失败: {e}", p.display()))
        })?;
    }
    let ix = ix_path(&store.root, old_id);
    if ix.exists() {
        remove_file_with_retry(&ix).map_err(|e| {
            StrataError::Manifest(format!("压实后删除旧段索引 `{}` 失败: {e}", ix.display()))
        })?;
    }
    Ok((new_id, moved))
}
