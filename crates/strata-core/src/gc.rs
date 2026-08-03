//! 三档 GC：整段删除 → hole-punch → 打分压实。
//!
//! 以 [`Store::latest_index`]（每键最大 gen 的全局视图）为唯一死活判据：
//! 段内扫描记录若与 latest 索引指向不符，即为死记录。

use std::collections::HashMap;
use std::fs::OpenOptions;

use crate::envelope::ENVELOPE_SIZE;
use crate::index::{IndexKey, IndexVal};
use crate::manifest::Bucket;
use crate::punch::{punch_hole, PunchOutcome};
use crate::segment::{scan_segment, ScannedRecord, SegmentWriter};
use crate::store::{seg_path, Store};
use crate::StrataError;

/// 分桶晋升阈值：Young → Active 所需的 flush 次数。
const ACTIVE_AFTER_FLUSHES: u64 = 2;

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
            let mut live_records: Vec<ScannedRecord> = Vec::new();
            let mut spans: Vec<(u64, u64)> = Vec::new(); // 死记录 [start, end)

            for rec in &scan.records {
                let rec_bytes = ENVELOPE_SIZE as u64 + rec.env.payload_len as u64;
                total += rec_bytes;
                let key = IndexKey {
                    x: rec.env.chunk_x,
                    z: rec.env.chunk_z,
                    type_id: rec.env.type_id,
                };
                let live = match latest.get(&key) {
                    Some(v) => {
                        v.seg_id == id
                            && v.offset == rec.offset
                            && v.gen == rec.env.gen
                            && rec.env.payload_hash != 0
                    }
                    None => false,
                };
                if live {
                    live_count += 1;
                    live_records.push(ScannedRecord {
                        env: rec.env.clone(),
                        offset: rec.offset,
                        payload: rec.payload.clone(),
                    });
                } else {
                    dead += rec_bytes;
                    spans.push((rec.offset, rec.offset + rec_bytes));
                }
            }

            // —— 档位 1：整段删除 ——
            if live_count == 0 {
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

            // —— 档位 2：hole-punch 连续死区间 ——
            spans.sort_unstable();
            let merged = merge_spans(&spans);
            let mut remaining_dead = dead;
            let mut punched = 0u64;
            for (start, end) in merged {
                let len = end - start;
                if len < cfg.min_hole_bytes {
                    continue;
                }
                let mut f = OpenOptions::new().read(true).write(true).open(&path)?;
                match punch_hole(&mut f, start, len)? {
                    PunchOutcome::Done => {
                        stats.holes_punched += 1;
                        stats.reclaimed_bytes += len;
                        punched += len;
                        remaining_dead -= len.min(remaining_dead);
                    }
                    PunchOutcome::Unsupported => {}
                }
            }

            // —— 档位 3 候选：仅当 punch 未消化全部死字节 ——
            if remaining_dead == 0 {
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
            let (new_id, moved) = compact_segment(self, cand.id, cand.live_records)?;
            // 搬迁后回写判据：被搬记录的最新指向改为新段。
            let entries: Vec<(IndexKey, IndexVal)> = self
                .segs
                .get(&new_id)
                .map(|st| st.incremental.clone())
                .unwrap_or_default();
            for (k, v) in entries {
                latest.insert(k, v);
            }
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

/// 档位 3 压实：把旧段的存活记录原样搬进新段，然后删除旧段。
///
/// 搬迁不改动信封任何字段（decode 出的原样 encode 回去）；新段
/// `SegmentMeta` 的 live/total 为搬迁字节和，`last_rewrite_epoch` 取当前 epoch。
/// 返回 `(新段 id, 搬迁记录数)`。
fn compact_segment(
    store: &mut Store,
    old_id: u32,
    live: Vec<ScannedRecord>,
) -> Result<(u32, u64), StrataError> {
    let bucket = store
        .manifest
        .segments
        .iter()
        .find(|m| m.id == old_id)
        .map(|m| m.bucket)
        .unwrap_or(Bucket::Young);

    let new_id = store.alloc_segment(bucket)?;
    let path = seg_path(&store.root, new_id);
    let mut w = SegmentWriter::create(&path, new_id)?;

    let mut moved_bytes = 0u64;
    let mut moved = 0u64;
    for rec in live {
        let offset = w.append(&rec.env, &rec.payload)?;
        let st = store
            .segs
            .get_mut(&new_id)
            .expect("alloc_segment inserted state");
        let key = IndexKey {
            x: rec.env.chunk_x,
            z: rec.env.chunk_z,
            type_id: rec.env.type_id,
        };
        st.bitmap.set(key.x, key.z, key.type_id);
        st.incremental.push((
            key,
            IndexVal {
                seg_id: new_id,
                offset,
                payload_len: rec.env.payload_len,
                gen: rec.env.gen,
                comp_id: rec.env.comp_id,
            },
        ));
        moved_bytes += ENVELOPE_SIZE as u64 + rec.env.payload_len as u64;
        moved += 1;
    }
    w.fsync()?;
    w.close()?;

    if let Some(m) = store.manifest.segments.iter_mut().find(|m| m.id == new_id) {
        m.live_bytes = moved_bytes;
        m.total_bytes = moved_bytes;
        m.last_rewrite_epoch = store.manifest.epoch;
    }

    store.remove_segment(old_id)?;
    Ok((new_id, moved))
}
