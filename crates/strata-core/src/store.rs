//! Store 门面：段写入、三层索引、epoch 日志与 manifest 的统一入口。
//!
//! 目录布局（`root` = vstore 目录）：
//!
//! ```text
//! root/
//! ├─ manifest.vsm (+ .bak)          # Manifest::save/load
//! ├─ segments/seg-XXXX.vseg         # 段数据（4 位零填充编号）
//! ├─ segments/seg-XXXX.vix          # 每段的磁盘索引页（IndexPage::serialize）
//! └─ epoch/current.velog            # EpochLog::open(root/epoch)
//! ```

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use xxhash_rust::xxh64::xxh64;

use crate::codec::{codec_for, dict_slot, make_comp_id, CODEC_NONE, CODEC_ZSTD};
use crate::envelope::{Envelope, ENVELOPE_SIZE};
use crate::epoch::{EpochEntry, EpochLog};
use crate::gc;
use crate::index::{IndexKey, IndexPage, IndexVal, RegionBitmap, SieveCache};
use crate::manifest::{
    Bucket, Manifest, RegionKey, SegmentMeta, FORMAT_VERSION, REGION_BITMAP_BYTES,
};
use crate::segment::{scan_segment, SegmentWriter};
use crate::StrataError;

/// 段文件子目录。
pub(crate) const SEGMENTS_DIR: &str = "segments";
/// epoch 日志子目录。
pub(crate) const EPOCH_DIR: &str = "epoch";

/// 段数据文件路径：`segments/seg-XXXX.vseg`（4 位零填充）。
pub(crate) fn seg_path(root: &Path, seg_id: u32) -> PathBuf {
    root.join(SEGMENTS_DIR).join(format!("seg-{seg_id:04}.vseg"))
}

/// 段磁盘索引页路径：`segments/seg-XXXX.vix`。
pub(crate) fn ix_path(root: &Path, seg_id: u32) -> PathBuf {
    root.join(SEGMENTS_DIR).join(format!("seg-{seg_id:04}.vix"))
}

/// Store 配置。
#[derive(Debug, Clone)]
pub struct StoreConfig {
    /// 热层 zstd 压缩级别。
    pub hot_level: i32,
    /// 热层是否压缩（false 时用 [`CODEC_NONE`]）。
    pub hot_enabled: bool,
    /// 冷层压缩级别（Phase 1 store 本体不用，留给 tier）。
    pub cold_level: i32,
    /// 冷层是否启用。
    pub cold_enabled: bool,
    /// 是否使用字典槽（仅当 manifest 中存在该 type_id 的字典时生效）。
    pub dictionary: bool,
    /// 索引页缓存预算（MiB）。
    pub cache_mb: u64,
    /// 单个段文件的大小上限（超过后滚动到新段）。
    pub segment_max_bytes: u64,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            hot_level: 3,
            hot_enabled: true,
            cold_level: 9,
            cold_enabled: true,
            dictionary: true,
            cache_mb: 512,
            segment_max_bytes: 64 * 1024 * 1024,
        }
    }
}

/// `verify` 的校验报告。
#[derive(Debug, Default)]
pub struct VerifyReport {
    /// 扫描到的记录总数。
    pub records: u64,
    /// 负载哈希损坏的记录：`(seg_id, 信封头偏移)`。
    pub corrupt_records: Vec<(u32, u64)>,
}

/// 单个段的内存状态：L0 位图 + 磁盘索引页（在 [`Store::cache`] 中）+ 未落盘增量。
pub(crate) struct SegState {
    /// 段内记录的存在性位图（坐标按 32×32 折叠，跨 region 的超集过滤器）。
    pub(crate) bitmap: RegionBitmap,
    /// 尚未合并进磁盘页的索引条目（追加序，gen 单调递增）。
    pub(crate) incremental: Vec<(IndexKey, IndexVal)>,
}

impl SegState {
    fn new() -> Self {
        Self {
            bitmap: RegionBitmap::new(),
            incremental: Vec::new(),
        }
    }
}

/// vstore 门面。
pub struct Store {
    pub(crate) root: PathBuf,
    pub(crate) cfg: StoreConfig,
    pub(crate) manifest: Manifest,
    /// 每段的位图 + 未落盘增量（磁盘索引页在 `cache` 中）。
    pub(crate) segs: HashMap<u32, SegState>,
    pub(crate) cache: SieveCache,
    pub(crate) epoch: EpochLog,
    /// 当前活跃段写入器；`None` 时下次 write 按需创建新段。
    pub(crate) writer: Option<SegmentWriter>,
    pub(crate) active_seg: u32,
    /// open 以来的 flush 次数（分桶晋升用）。
    pub(crate) epoch_flush_count: u64,
}

/// 空 manifest（新 store 或 manifest 损坏重建时用）。
fn empty_manifest() -> Manifest {
    Manifest {
        format_version: FORMAT_VERSION,
        next_seg_id: 1,
        ..Manifest::default()
    }
}

impl Store {
    /// 打开（或创建）`root` 处的 vstore。
    ///
    /// manifest 缺失或损坏时按段文件扫描重建索引；正常打开路径**不**扫描段文件。
    pub fn open(root: &Path, cfg: StoreConfig) -> Result<Self, StrataError> {
        std::fs::create_dir_all(root)?;
        std::fs::create_dir_all(root.join(SEGMENTS_DIR))?;
        std::fs::create_dir_all(root.join(EPOCH_DIR))?;

        // manifest 缺失（None）或损坏（Err）都需要扫描重建：
        // 缺失可能是"从未保存就崩溃"，此时段文件是唯一的真相来源。
        let (manifest, needs_rebuild) = match Manifest::load(root) {
            Ok(Some(m)) => (m, false),
            Ok(None) => (empty_manifest(), true),
            Err(_) => (empty_manifest(), true),
        };

        let cache_budget = cfg.cache_mb.saturating_mul(1024 * 1024);
        let epoch = EpochLog::open(&root.join(EPOCH_DIR))?;
        let mut store = Store {
            root: root.to_path_buf(),
            cfg,
            manifest,
            segs: HashMap::new(),
            cache: SieveCache::new(cache_budget),
            epoch,
            writer: None,
            active_seg: 0,
            epoch_flush_count: 0,
        };

        if needs_rebuild {
            store.rebuild_index_from_scan()?;
        }

        // 每段：磁盘索引页 + 位图（缺页→空页；位图从段内条目恢复）。
        let ids: Vec<u32> = store.manifest.segments.iter().map(|m| m.id).collect();
        for id in ids {
            let page = match std::fs::read(ix_path(root, id)) {
                Ok(bytes) => IndexPage::deserialize(&bytes).unwrap_or_else(|_| IndexPage::from_entries(Vec::new())),
                Err(_) => IndexPage::from_entries(Vec::new()),
            };
            let st = store.segs.entry(id).or_insert_with(SegState::new);
            for (k, _) in page.iter() {
                st.bitmap.set(k.x, k.z, k.type_id);
            }
            store.cache.put(id, Arc::new(page));
        }

        // epoch 回放：日志里的记录可能比 .vix 新（崩溃前未 flush）。
        for e in store.epoch.replay()? {
            if let Some(st) = store.segs.get_mut(&e.seg_id) {
                let key = IndexKey {
                    x: e.env.chunk_x,
                    z: e.env.chunk_z,
                    type_id: e.env.type_id,
                };
                st.bitmap.set(key.x, key.z, key.type_id);
                st.incremental.push((
                    key,
                    IndexVal {
                        seg_id: e.seg_id,
                        offset: e.offset,
                        payload_len: e.env.payload_len,
                        gen: e.env.gen,
                        comp_id: e.env.comp_id,
                    },
                ));
            }
        }

        store.active_seg = store.manifest.next_seg_id;
        Ok(store)
    }

    /// 写入一条记录（压缩 → 追加段 → epoch 日志 → 内存索引）。
    pub fn write(&mut self, x: i32, z: i32, type_id: u16, nbt: &[u8]) -> Result<(), StrataError> {
        // 1. 压缩：热层开关 + 字典槽解析。
        let codec = if self.cfg.hot_enabled { CODEC_ZSTD } else { CODEC_NONE };
        let dict_pos = if self.cfg.dictionary {
            self.manifest
                .dict_slots
                .iter()
                .position(|(t, _)| *t == type_id)
        } else {
            None
        };
        let slot = dict_pos.unwrap_or(0) as u8;
        let comp_id = make_comp_id(codec, slot);

        let mut compressed = Vec::new();
        {
            let dict: Option<&[u8]> = dict_pos.map(|i| self.manifest.dict_slots[i].1.as_slice());
            let cd = codec_for(comp_id, self.cfg.hot_level, dict)?;
            cd.compress(nbt, &mut compressed)?;
        }

        // 2. gen 分配。
        let gen = self.manifest.next_gen;
        self.manifest.next_gen += 1;

        // 3. 活跃段按需创建。
        if self.writer.is_none() {
            let id = self.alloc_segment(Bucket::Young)?;
            let path = seg_path(&self.root, id);
            let w = SegmentWriter::create(&path, id)?;
            self.writer = Some(w);
            self.active_seg = id;
        }
        // region 位图快照占位（缺则补零页）。
        let rk = RegionKey { x: x >> 5, z: z >> 5 };
        if !self.manifest.region_bitmaps.iter().any(|(k, _)| *k == rk) {
            self.manifest
                .region_bitmaps
                .push((rk, vec![0u8; REGION_BITMAP_BYTES]));
        }

        // 4. 信封。
        let env = Envelope {
            record_ver: 1,
            type_id,
            comp_id,
            chunk_x: x,
            chunk_z: z,
            gen,
            epoch_ts: self.manifest.epoch as u32,
            payload_len: compressed.len() as u32,
            payload_hash: xxh64(&compressed, 0),
        };

        // 5. 追加段文件。
        let seg_id = self.active_seg;
        let offset = self
            .writer
            .as_mut()
            .expect("writer ensured above")
            .append(&env, &compressed)?;

        // 6. epoch 日志。
        self.epoch.record(&EpochEntry {
            seg_id,
            env: env.clone(),
            offset,
        })?;

        // 7. 内存索引。
        let st = self.segs.get_mut(&seg_id).expect("segment state exists");
        st.bitmap.set(x, z, type_id);
        st.incremental.push((
            IndexKey { x, z, type_id },
            IndexVal {
                seg_id,
                offset,
                payload_len: compressed.len() as u32,
                gen,
                comp_id,
            },
        ));

        // 8. 段表记账。
        let rec_bytes = ENVELOPE_SIZE as u64 + compressed.len() as u64;
        if let Some(m) = self.manifest.segments.iter_mut().find(|m| m.id == seg_id) {
            m.total_bytes += rec_bytes;
            m.live_bytes += rec_bytes;
        }

        // 9. 段滚动。
        if self
            .writer
            .as_ref()
            .map_or(false, |w| w.offset() >= self.cfg.segment_max_bytes)
        {
            let mut w = self.writer.take().expect("checked Some above");
            w.fsync()?;
            w.close()?;
        }
        Ok(())
    }

    /// 读取一条记录的最新版本；不存在或损坏隔离时返回 `Ok(None)`。
    pub fn read(&self, x: i32, z: i32, type_id: u16) -> Result<Option<Vec<u8>>, StrataError> {
        let key = IndexKey { x, z, type_id };
        let mut best: Option<IndexVal> = None;

        // 逐段过滤：位图 + （增量 ∪ 磁盘页）取最大 gen。
        for (&seg_id, st) in &self.segs {
            if !st.bitmap.has(x, z, type_id) {
                continue;
            }
            let mut cand: Option<IndexVal> = None;
            // 增量按追加序 gen 递增，倒序第一个命中即最大 gen。
            for (k, v) in st.incremental.iter().rev() {
                if *k == key {
                    cand = Some(*v);
                    break;
                }
            }
            if let Some(page) = self.load_page(seg_id)? {
                if let Some(v) = page.lookup(&key) {
                    cand = Some(match cand {
                        Some(c) if c.gen >= v.gen => c,
                        _ => *v,
                    });
                }
            }
            if let Some(v) = cand {
                if best.map_or(true, |b| v.gen > b.gen) {
                    best = Some(v);
                }
            }
        }

        let val = match best {
            Some(v) => v,
            None => return Ok(None),
        };

        // 读盘：信封头 → 字段校验 → 负载 → 哈希校验。
        let mut f = File::open(seg_path(&self.root, val.seg_id))?;
        f.seek(SeekFrom::Start(val.offset))?;
        let mut hdr = [0u8; ENVELOPE_SIZE];
        f.read_exact(&mut hdr)?;
        let env = Envelope::decode(&hdr)?;
        if env.chunk_x != x
            || env.chunk_z != z
            || env.type_id != type_id
            || env.gen != val.gen
            || env.payload_len != val.payload_len
        {
            // 索引与盘上记录不一致 → 损坏隔离。
            return Ok(None);
        }
        let mut payload = vec![0u8; env.payload_len as usize];
        f.read_exact(&mut payload)?;
        if env.payload_hash == 0 || xxh64(&payload, 0) != env.payload_hash {
            return Ok(None);
        }

        // 解压：字典槽同 write 规则（槽号 = dict_slots 中的位置）。
        let slot = dict_slot(env.comp_id) as usize;
        let dict: Option<&[u8]> = self
            .manifest
            .dict_slots
            .get(slot)
            .map(|(_, d)| d.as_slice());
        let cd = codec_for(env.comp_id, self.cfg.hot_level, dict)?;
        let mut out = Vec::new();
        cd.decompress(&payload, &mut out)?;
        Ok(Some(out))
    }

    /// 落盘：fsync 活跃段 → 合并每段增量进磁盘索引页 → 推进 epoch → 保存 manifest → 轮转日志。
    pub fn flush(&mut self) -> Result<(), StrataError> {
        if let Some(w) = self.writer.as_mut() {
            w.fsync()?;
        }

        let ids: Vec<u32> = self.segs.keys().copied().collect();
        for id in ids {
            if self.segs.get(&id).map_or(true, |s| s.incremental.is_empty()) {
                continue;
            }
            // 旧页：优先缓存，回落磁盘。
            let old = match self.cache.get(id) {
                Some(p) => p,
                None => self
                    .load_page(id)?
                    .unwrap_or_else(|| Arc::new(IndexPage::from_entries(Vec::new()))),
            };
            let mut entries: Vec<(IndexKey, IndexVal)> = old.iter().cloned().collect();
            {
                let st = self.segs.get_mut(&id).expect("iterated from segs");
                entries.extend(st.incremental.drain(..));
            }
            // from_entries 排序并对同键保留最大 gen。
            let page = Arc::new(IndexPage::from_entries(entries));
            let bytes = page.serialize();

            let final_path = ix_path(&self.root, id);
            let tmp_path = final_path.with_extension("vix.tmp");
            let mut f = File::create(&tmp_path)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
            // Windows rename 到已存在目标会失败，先清旧文件。
            if final_path.exists() {
                std::fs::remove_file(&final_path)?;
            }
            std::fs::rename(&tmp_path, &final_path)?;
            self.cache.put(id, page);
        }

        // 以全局最新视图重算每段 live_bytes（覆盖写入不减少 live 的暂态记账）。
        self.recompute_live_bytes()?;

        self.manifest.epoch += 1;
        self.epoch_flush_count += 1;
        gc::advance_buckets(self);
        self.manifest.save(&self.root)?;
        self.epoch.rotate()?;
        Ok(())
    }

    /// 扫描 `segments/*.vseg` 重建索引（manifest 丢失/损坏时的恢复路径）。
    /// 返回扫描到的记录总数。
    pub fn rebuild_index_from_scan(&mut self) -> Result<u64, StrataError> {
        let dir = self.root.join(SEGMENTS_DIR);
        let mut files: Vec<(u32, PathBuf)> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for entry in rd.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if let Some(stem) = name.strip_suffix(".vseg") {
                    if let Some(num) = stem.strip_prefix("seg-") {
                        if let Ok(id) = num.parse::<u32>() {
                            files.push((id, entry.path()));
                        }
                    }
                }
            }
        }
        files.sort_by_key(|(id, _)| *id);

        let mut count = 0u64;
        for (id, path) in files {
            let scan = scan_segment(&path)?;
            for rec in scan.records {
                if !self.segs.contains_key(&id) {
                    let epoch = self.manifest.epoch;
                    self.manifest.segments.push(SegmentMeta {
                        id,
                        live_bytes: 0,
                        total_bytes: 0,
                        bucket: Bucket::Young,
                        created_epoch: epoch,
                        last_rewrite_epoch: epoch,
                    });
                    self.segs.insert(id, SegState::new());
                }
                let rec_bytes = ENVELOPE_SIZE as u64 + rec.env.payload_len as u64;
                if let Some(m) = self.manifest.segments.iter_mut().find(|m| m.id == id) {
                    m.total_bytes += rec_bytes;
                    m.live_bytes += rec_bytes;
                }
                let key = IndexKey {
                    x: rec.env.chunk_x,
                    z: rec.env.chunk_z,
                    type_id: rec.env.type_id,
                };
                let st = self.segs.get_mut(&id).expect("inserted above");
                st.bitmap.set(key.x, key.z, key.type_id);
                st.incremental.push((
                    key,
                    IndexVal {
                        seg_id: id,
                        offset: rec.offset,
                        payload_len: rec.env.payload_len,
                        gen: rec.env.gen,
                        comp_id: rec.env.comp_id,
                    },
                ));
                if rec.env.gen >= self.manifest.next_gen {
                    self.manifest.next_gen = rec.env.gen + 1;
                }
                count += 1;
            }
            if id >= self.manifest.next_seg_id {
                self.manifest.next_seg_id = id + 1;
            }
        }
        self.manifest.save(&self.root)?;
        Ok(count)
    }

    /// 全量校验：扫描每个段，报告记录数与损坏记录。
    pub fn verify(&self) -> Result<VerifyReport, StrataError> {
        let mut report = VerifyReport::default();
        for meta in &self.manifest.segments {
            let scan = scan_segment(&seg_path(&self.root, meta.id))?;
            for rec in &scan.records {
                report.records += 1;
                if rec.env.payload_hash == 0 {
                    report.corrupt_records.push((meta.id, rec.offset));
                }
            }
        }
        Ok(report)
    }

    /// 分配新段号：段表 + SegState 落位（不创建文件、不动活跃写入器）。
    pub(crate) fn alloc_segment(&mut self, bucket: Bucket) -> Result<u32, StrataError> {
        let id = self.manifest.next_seg_id;
        self.manifest.next_seg_id += 1;
        let epoch = self.manifest.epoch;
        self.manifest.segments.push(SegmentMeta {
            id,
            live_bytes: 0,
            total_bytes: 0,
            bucket,
            created_epoch: epoch,
            last_rewrite_epoch: epoch,
        });
        self.segs.insert(id, SegState::new());
        Ok(id)
    }

    /// 删除一个段：数据文件 + 索引页 + 内存状态 + 段表 + 缓存。
    pub(crate) fn remove_segment(&mut self, seg_id: u32) -> Result<(), StrataError> {
        let p = seg_path(&self.root, seg_id);
        if p.exists() {
            std::fs::remove_file(&p)?;
        }
        let ix = ix_path(&self.root, seg_id);
        if ix.exists() {
            std::fs::remove_file(&ix)?;
        }
        self.segs.remove(&seg_id);
        self.manifest.segments.retain(|m| m.id != seg_id);
        self.cache.evict(seg_id);
        Ok(())
    }

    /// 读磁盘索引页（不存在 → `None`）。`read`/`latest_index` 是 `&self`，不走缓存。
    pub(crate) fn load_page(&self, seg_id: u32) -> Result<Option<Arc<IndexPage>>, StrataError> {
        match std::fs::read(ix_path(&self.root, seg_id)) {
            Ok(bytes) => Ok(Some(Arc::new(IndexPage::deserialize(&bytes)?))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(StrataError::Io(e)),
        }
    }

    /// flush 收尾：以"每键最大 gen"的全局视图重算每段 live_bytes。
    fn recompute_live_bytes(&mut self) -> Result<(), StrataError> {
        let ids: Vec<u32> = self.manifest.segments.iter().map(|m| m.id).collect();
        let mut latest: HashMap<IndexKey, IndexVal> = HashMap::new();
        for id in ids {
            let page = match self.cache.get(id) {
                Some(p) => p,
                None => match self.load_page(id)? {
                    Some(p) => p,
                    None => continue,
                },
            };
            for (k, v) in page.iter() {
                match latest.get(k) {
                    Some(e) if e.gen >= v.gen => {}
                    _ => {
                        latest.insert(k.clone(), *v);
                    }
                }
            }
        }
        for m in self.manifest.segments.iter_mut() {
            m.live_bytes = 0;
        }
        for (_, v) in &latest {
            if let Some(m) = self.manifest.segments.iter_mut().find(|m| m.id == v.seg_id) {
                m.live_bytes += ENVELOPE_SIZE as u64 + v.payload_len as u64;
            }
        }
        Ok(())
    }
}
