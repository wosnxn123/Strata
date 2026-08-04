//! Store 门面：段写入、三层索引、epoch 日志与 manifest 的统一入口。
//!
//! 目录布局（`root` = vstore 目录）：
//!
//! ```text
//! root/
//! ├─ .strata.lock                   # 会话锁（SessionLock，进程独占）
//! ├─ manifest.vsm (+ .bak)          # Manifest::save/load
//! ├─ segments/seg-XXXX.vseg         # 段数据（4 位零填充编号）
//! ├─ segments/seg-XXXX.vix          # 每段的磁盘索引页（IndexPage::serialize）
//! ├─ cold/r.{rx}.{rz}.varc (+ .inv) # 冷归档（tier 晋升产物）
//! └─ epoch/current.velog            # EpochLog::open(root/epoch)
//! ```
//!
//! 崩溃一致性原则：**替代物持久化之前绝不删旧物；日志不得先于数据**。
//! - write：段数据 sync → epoch 条目 flush+sync（返回即持久）；
//! - alloc：段文件头落盘 → manifest 登记（epoch 条目引用的段必在盘上 manifest 中）；
//! - compact：新段数据+索引页落盘 → manifest 换段 → 才删旧段；
//! - 回放：未知 seg_id 按段头认领；越界条目丢弃；next_gen 与回放观察值对齐。

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use xxhash_rust::xxh64::xxh64;

use crate::codec::{codec_for, dict_slot, make_comp_id, CODEC_NONE, CODEC_ZSTD};
use crate::cold::ArchiveReader;
use crate::envelope::{Envelope, ENVELOPE_SIZE};
use crate::epoch::{EpochEntry, EpochLog};
use crate::gc;
use crate::index::{IndexKey, IndexPage, IndexVal, RegionBitmap, SieveCache};
use crate::lock::SessionLock;
use crate::manifest::{
    rename_replace, Bucket, ColdMeta, Manifest, RegionKey, SegmentMeta, FORMAT_VERSION,
};
use crate::segment::{scan_segment, segment_header_ok, SegmentWriter};
use crate::StrataError;

/// 段文件子目录。
pub(crate) const SEGMENTS_DIR: &str = "segments";
/// 冷归档子目录。
pub(crate) const COLD_DIR: &str = "cold";
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

/// 冷归档文件路径：`cold/r.{rx}.{rz}.varc`。
pub(crate) fn cold_path(root: &Path, region_x: i32, region_z: i32) -> PathBuf {
    root.join(COLD_DIR).join(format!("r.{region_x}.{region_z}.varc"))
}

/// Windows 上新建/刚写过的文件可能被杀软或索引器短暂锁定：删除遇到
/// `ERROR_ACCESS_DENIED (5)` / `ERROR_SHARING_VIOLATION (32)` 时 sleep 50ms
/// 重试（共 3 次），与 strata-cli 同策略。其他错误码不重试，直接上抛。
pub(crate) fn remove_file_with_retry(path: &Path) -> std::io::Result<()> {
    let mut last = None;
    for _ in 0..3 {
        match std::fs::remove_file(path) {
            Ok(()) => return Ok(()),
            Err(e) if retryable_remove_error(&e) => {
                last = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("retry exhausted")))
}

#[cfg(windows)]
fn retryable_remove_error(e: &std::io::Error) -> bool {
    // ERROR_ACCESS_DENIED = 5, ERROR_SHARING_VIOLATION = 32
    matches!(e.raw_os_error(), Some(5) | Some(32))
}

#[cfg(not(windows))]
fn retryable_remove_error(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::PermissionDenied
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
    /// 批量写入的压缩工作线程数：`0` = 自动（全部可用核心），
    /// `1` = 串行（默认，游戏服 TPS 优先），`N ≥ 2` = 限 N 线程。
    pub compression_threads: u32,
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
            compression_threads: 1,
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

/// 单条批量写入项。
#[derive(Debug, Clone)]
pub struct BatchItem {
    pub x: i32,
    pub z: i32,
    pub type_id: u16,
    pub nbt: Vec<u8>,
}

/// [`Store::write_batch`] 的结果。
#[derive(Debug, Default, Clone, Copy)]
pub struct BatchWriteResult {
    /// 成功落盘的记录数。
    pub written: u64,
}

/// 单个段的内存状态：L0 位图 + 磁盘索引页（在 [`Store::cache`] 中）+ 未落盘增量。
pub(crate) struct SegState {
    /// 段内记录的存在性位图（坐标按 32×32 折叠，跨 region 的超集过滤器）。
    /// vstore v3 起这是唯一位图：manifest 不再持久化任何 region 位图快照。
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
///
/// 线程模型：全部字段均为 `Send + Sync`（内部可变性只通过 `Mutex`），
/// 编译器自动派生，无需 unsafe 断言。`SyncStore` 在此之上用 `RwLock`
/// 串行化 `&mut self` 写路径，读路径（`read`/`verify`/`touch_stats`）
/// 可多线程并发：索引页缓存与冷归档读取器各自以 `Mutex` 短临界区共享。
pub struct Store {
    pub(crate) root: PathBuf,
    pub(crate) cfg: StoreConfig,
    pub(crate) manifest: Manifest,
    /// 每段的位图 + 未落盘增量（磁盘索引页在 `cache` 中）。
    pub(crate) segs: HashMap<u32, SegState>,
    /// L1 索引页缓存（SIEVE）。`Mutex` 包装使 `&self` 读路径也能命中缓存。
    pub(crate) cache: Mutex<SieveCache>,
    pub(crate) epoch: EpochLog,
    /// 当前活跃段写入器；`None` 时下次 write 按需创建新段。
    pub(crate) writer: Option<SegmentWriter>,
    pub(crate) active_seg: u32,
    /// open 以来的 flush 次数（分桶晋升用）。
    pub(crate) epoch_flush_count: u64,
    /// 冷归档懒加载读取器：外层 map 锁只做查表/插入（取走 `Arc` 即放锁），
    /// 每个归档一把内层锁串行其上的块缓存与文件定位。
    pub(crate) cold_readers: Mutex<HashMap<RegionKey, Arc<Mutex<ArchiveReader>>>>,
    /// region → `manifest.cold` 槽位下标：O(1) 判定"该 region 是否有冷归档"
    /// （读回落、回放跳过、失效记账、晋升查重共用）。
    pub(crate) cold_lookup: HashMap<RegionKey, usize>,
    /// demote 回写热层期间置位：暂停冷槽失效记账（见
    /// [`Store::invalidate_cold_slot`] 的崩溃窗口说明）。
    pub(crate) demote_in_progress: bool,
    /// 会话独占锁；随 Store 存活，drop 时释放。
    _lock: SessionLock,
}

/// 空 manifest（新 store 或 manifest 损坏重建时用）。
fn empty_manifest() -> Manifest {
    Manifest {
        format_version: FORMAT_VERSION,
        next_seg_id: 1,
        ..Manifest::default()
    }
}

/// `LockResult` 松弛解包：持锁线程 panic 导致的中毒锁取回内层数据继续使用
/// （锁保护的结构自身没有不变量被 panic 破坏的风险点，宁可用也不崩）。
pub(crate) trait MutexExt<'a, T> {
    fn unwrap_or_poisoned(self) -> MutexGuard<'a, T>;
}

impl<'a, T> MutexExt<'a, T> for std::sync::LockResult<MutexGuard<'a, T>> {
    fn unwrap_or_poisoned(self) -> MutexGuard<'a, T> {
        self.unwrap_or_else(|p| p.into_inner())
    }
}

impl Store {
    /// 打开（或创建）`root` 处的 vstore。
    ///
    /// manifest 缺失或损坏时按段文件扫描重建索引；正常打开路径**不**扫描段文件。
    /// 打开顺序即恢复顺序：会话锁 → manifest → 重建（如需）→ 索引页 →
    /// 冷区对账 → epoch 回放（认领孤儿段、丢弃越界条目、对齐 next_gen）。
    pub fn open(root: &Path, cfg: StoreConfig) -> Result<Self, StrataError> {
        std::fs::create_dir_all(root)?;
        std::fs::create_dir_all(root.join(SEGMENTS_DIR))?;
        std::fs::create_dir_all(root.join(EPOCH_DIR))?;

        // 会话锁：先于任何数据文件触碰；被占用直接带持有者信息报错。
        let lock = SessionLock::acquire(root)?;

        // manifest 缺失（None）或损坏/旧版本（Err）都需要扫描重建：
        // 缺失可能是"从未保存就崩溃"，此时段文件是唯一的真相来源；
        // 旧版本（v2 及以下）无迁移负担——段扫描 + 冷区对账即完整重建。
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
            cache: Mutex::new(SieveCache::new(cache_budget)),
            epoch,
            writer: None,
            active_seg: 0,
            epoch_flush_count: 0,
            cold_readers: Mutex::new(HashMap::new()),
            cold_lookup: HashMap::new(),
            demote_in_progress: false,
            _lock: lock,
        };
        store.rebuild_cold_lookup();

        if needs_rebuild {
            store.rebuild_index_from_scan()?;
        }

        // 每段：磁盘索引页 + 位图（缺页→空页；位图从段内条目恢复）。
        let ids: Vec<u32> = store.manifest.segments.iter().map(|m| m.id).collect();
        for id in ids {
            let page = match std::fs::read(ix_path(root, id)) {
                Ok(bytes) => IndexPage::deserialize(&bytes)
                    .unwrap_or_else(|_| IndexPage::from_entries(Vec::new())),
                Err(_) => IndexPage::from_entries(Vec::new()),
            };
            let st = store.segs.entry(id).or_insert_with(SegState::new);
            for (k, _) in page.iter() {
                st.bitmap.set(k.x, k.z, k.type_id);
            }
            store.cache.lock().unwrap_or_poisoned().put(id, Arc::new(page));
        }

        // 冷区对账先于回放：重建路径下回放需要冷区清单跳过已晋升键。
        let mut dirty = store.reconcile_cold()?;

        // epoch 回放：日志里的记录可能比 .vix 新（崩溃前未 flush）。
        // 已晋升冷区的键不得被回放复活（否则热层重新索引到已搬走的数据）。
        let mut max_gen: Option<u64> = None;
        let mut seg_lens: HashMap<u32, u64> = HashMap::new();
        for e in store.epoch.replay()? {
            // 回放观察到的最大 gen（含被跳过/丢弃的条目：gen 已被消耗过，
            // 复用会让新旧版本 gen 冲突）。
            max_gen = Some(max_gen.map_or(e.env.gen, |g| g.max(e.env.gen)));

            let rk = RegionKey {
                x: e.env.chunk_x >> 5,
                z: e.env.chunk_z >> 5,
            };
            if store.cold_lookup.contains_key(&rk) {
                continue;
            }

            // 幽灵条目防御：offset 超出段文件当前长度 = 数据未落盘
            // （WAL 顺序保证数据先于日志，这里只是兜底）。
            let seg_len = *seg_lens.entry(e.seg_id).or_insert_with(|| {
                std::fs::metadata(seg_path(root, e.seg_id))
                    .map(|m| m.len())
                    .unwrap_or(0)
            });
            let need = ENVELOPE_SIZE as u64 + e.env.payload_len as u64;
            if e.offset >= seg_len || e.offset + need > seg_len {
                continue;
            }

            // 孤儿段认领：manifest 不认识该 seg_id 但文件存在且段头有效 →
            // 按段头登记（而不是丢弃条目造成静默数据丢失）。
            if !store.segs.contains_key(&e.seg_id) && !store.claim_segment(e.seg_id)? {
                continue;
            }
            let st = store.segs.get_mut(&e.seg_id).expect("known or claimed above");
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
            dirty = true;
        }

        // next_gen 回卷防御：与 manifest 持久值取 max（与 rebuild 同规则）。
        if let Some(g) = max_gen {
            if store.manifest.next_gen <= g {
                store.manifest.next_gen = g + 1;
                dirty = true;
            }
        }

        store.active_seg = store.manifest.next_seg_id;
        if dirty {
            store.manifest.save(&store.root)?;
        }
        Ok(store)
    }

    /// 写入一条记录（压缩 → 追加段 → epoch 日志 → 内存索引）。
    ///
    /// 持久化语义：vanilla 等价——记录立即可见（同会话可读），落盘强度为
    /// "OS 缓存 + 日志 flush"；崩溃最多丢自上次 [`Store::flush`] 以来的记录
    /// （回放按段文件实际长度兜底）。需要"返回即持久"的调用方（如服务端
    /// 在写成功后删除主副本）用 [`Store::write_durable`]。
    pub fn write(&mut self, x: i32, z: i32, type_id: u16, nbt: &[u8]) -> Result<(), StrataError> {
        let (compressed, comp_id) = self.compress_payload(type_id, nbt)?;
        self.append_compressed(x, z, type_id, compressed, comp_id, false)
    }

    /// 写入一条记录并**确保持久**：返回 `Ok` 时段数据与 epoch 日志均已 sync。
    /// 代价是每条两次 fsync——仅用于"成功后即删除唯一主副本"的调用路径。
    pub fn write_durable(
        &mut self,
        x: i32,
        z: i32,
        type_id: u16,
        nbt: &[u8],
    ) -> Result<(), StrataError> {
        let (compressed, comp_id) = self.compress_payload(type_id, nbt)?;
        self.append_compressed(x, z, type_id, compressed, comp_id, true)
    }

    /// 热层压缩：开关 + 字典槽解析，与 [`Store::read`] 的解压规则严格对称。
    ///
    /// 返回 `(压缩字节, comp_id)`。只读 `cfg`/`manifest`（纯函数语义），
    /// [`Store::write_batch`] 依赖这一点在有界工作线程中并行调用。
    fn compress_payload(
        &self,
        type_id: u16,
        nbt: &[u8],
    ) -> Result<(Vec<u8>, u8), StrataError> {
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
        Ok((compressed, comp_id))
    }

    /// 已压缩记录的落盘子路径：gen 分配 → 段追加（WAL：数据先持久化）→
    /// epoch 日志 → 内存索引 → 段表记账 → 段滚动 → 冷区失效。
    ///
    /// [`Store::write`]/[`Store::write_durable`]/[`Store::write_batch`] 共用
    /// 此路径（同一条 gen/段/epoch/索引链路）。`durable=true`：段 `sync_data`
    /// 与日志 `sync_all` 逐条落盘（返回即持久）；`durable=false`：数据经
    /// `flush_buf` 到 OS、日志 flush 到 OS（组提交路径在批尾统一 sync）。
    fn append_compressed(
        &mut self,
        x: i32,
        z: i32,
        type_id: u16,
        compressed: Vec<u8>,
        comp_id: u8,
        durable: bool,
    ) -> Result<(), StrataError> {
        // 1. gen 分配。
        let gen = self.manifest.next_gen;
        self.manifest.next_gen += 1;

        // 2. 活跃段按需创建（alloc 内部：文件头落盘 → manifest 登记）。
        if self.writer.is_none() {
            let (id, w) = self.alloc_segment(Bucket::Young)?;
            self.writer = Some(w);
            self.active_seg = id;
        }

        // 3. 信封。
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

        // 4. 追加段文件并持久化数据——日志不得先于数据。
        let seg_id = self.active_seg;
        let writer = self.writer.as_mut().expect("writer ensured above");
        let offset = match writer.append(&env, &compressed) {
            Ok(o) => o,
            Err(e) => {
                // 写入器已做部分写恢复；丢弃它，下次写入开新段。
                // 该记录未进日志/索引，不可见，段内前缀完好。
                self.writer = None;
                return Err(e);
            }
        };
        if durable {
            if let Err(e) = writer.sync_data() {
                self.writer = None;
                return Err(e);
            }
        } else if let Err(e) = writer.flush_buf() {
            self.writer = None;
            return Err(e);
        }

        // 5. epoch 日志（durable 时逐条 sync；组提交批尾统一 sync）。
        self.epoch.record(
            &EpochEntry {
                seg_id,
                env: env.clone(),
                offset,
            },
            durable,
        )?;

        // 6. 内存索引。
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

        // 7. 段表记账。
        let rec_bytes = ENVELOPE_SIZE as u64 + compressed.len() as u64;
        if let Some(m) = self.manifest.segments.iter_mut().find(|m| m.id == seg_id) {
            m.total_bytes += rec_bytes;
            m.live_bytes += rec_bytes;
        }

        // 8. 段滚动。
        if self
            .writer
            .as_ref()
            .is_some_and(|w| w.offset() >= self.cfg.segment_max_bytes)
        {
            let mut w = self.writer.take().expect("checked Some above");
            w.fsync()?;
            w.close()?;
        }

        // 9. 冷区失效：覆盖已晋升 region 的键时，归档槽位作废并记账。
        self.invalidate_cold_slot(x, z, type_id)?;

        Ok(())
    }

    /// 批量写入：压缩（串行或有界并行）+ 串行追加 + **组提交**。
    ///
    /// 压缩是纯函数（只读 `cfg` 与 `manifest` 字典槽），并发度由
    /// [`StoreConfig::compression_threads`] 控制：`1`（默认）或单条走串行；
    /// `0`（全部可用核心）/`N ≥ 2` 在 [`std::thread::scope`] 内派生有界
    /// worker 线程分块压缩——不用 rayon 全局池，避免与游戏线程抢核且可按
    /// Store 配置限流。随后串行循环走 [`Store::append_compressed`]（非持久
    /// 变体），批尾**一次**段 `sync_data` + 一次日志 `sync_all`（group
    /// commit）：返回 `Ok` 时整批持久，且 fsync 成本摊薄为 O(1)/批。
    ///
    /// **前缀提交语义**：任一失败不回滚已提交部分，错误为
    /// [`StrataError::BatchPartial`]（`committed` = 已追加条数）。空批量不做
    /// 任何 IO，返回 `written = 0`。
    pub fn write_batch(&mut self, items: &[BatchItem]) -> Result<BatchWriteResult, StrataError> {
        if items.is_empty() {
            return Ok(BatchWriteResult { written: 0 });
        }

        let compressed = self.compress_batch(items).map_err(|e| StrataError::BatchPartial {
            committed: 0,
            source: Box::new(e),
        })?;

        let mut committed = 0u64;
        for (item, (bytes, comp_id)) in items.iter().zip(compressed) {
            if let Err(e) =
                self.append_compressed(item.x, item.z, item.type_id, bytes, comp_id, false)
            {
                return Err(StrataError::BatchPartial {
                    committed,
                    source: Box::new(e),
                });
            }
            committed += 1;
        }

        // 组提交批尾：段数据与日志一次性持久化（WAL 顺序：数据先于日志）。
        if committed > 0 {
            if let Some(w) = self.writer.as_mut() {
                if let Err(e) = w.sync_data() {
                    self.writer = None;
                    return Err(StrataError::BatchPartial {
                        committed,
                        source: Box::new(e),
                    });
                }
            }
            if let Err(e) = self.epoch.sync() {
                return Err(StrataError::BatchPartial {
                    committed,
                    source: Box::new(e),
                });
            }
        }

        Ok(BatchWriteResult {
            written: items.len() as u64,
        })
    }

    /// [`Store::write_batch`] 的压缩前置：按 [`StoreConfig::compression_threads`]
    /// 串行或有界并行压缩全部条目，结果与 `items` 同序。
    fn compress_batch(&self, items: &[BatchItem]) -> Result<Vec<(Vec<u8>, u8)>, StrataError> {
        let threads = self.cfg.compression_threads;
        // 串行（默认）或单条：不派生线程。
        if threads == 1 || items.len() <= 1 {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(self.compress_payload(item.type_id, &item.nbt)?);
            }
            return Ok(out);
        }

        // 并行：`0` = 全部可用核心；否则限 N。worker 数不超过条目数。
        let want = if threads == 0 {
            std::thread::available_parallelism().map_or(1, |n| n.get())
        } else {
            threads as usize
        };
        let workers = want.min(items.len());
        let chunk = items.len().div_ceil(workers);

        // compress_payload 纯函数（&self）；Store: Sync，共享引用可跨线程。
        let this: &Store = self;
        let mut compressed: Vec<(Vec<u8>, u8)> = Vec::with_capacity(items.len());
        std::thread::scope(|scope| -> Result<(), StrataError> {
            let handles: Vec<_> = items
                .chunks(chunk)
                .map(|piece| {
                    scope.spawn(move || {
                        let mut out = Vec::with_capacity(piece.len());
                        for item in piece {
                            out.push(this.compress_payload(item.type_id, &item.nbt)?);
                        }
                        Ok::<_, StrataError>(out)
                    })
                })
                .collect();
            for handle in handles {
                // worker panic 视为批量失败（compress_payload 只返回错误，不 panic）。
                let piece = handle
                    .join()
                    .map_err(|_| StrataError::Codec("压缩工作线程 panic".to_string()))?;
                compressed.extend(piece?);
            }
            Ok(())
        })?;
        Ok(compressed)
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
                if best.is_none_or(|b| v.gen > b.gen) {
                    best = Some(v);
                }
            }
        }

        let val = match best {
            Some(v) => v,
            None => {
                // 热路径全 miss：回落到冷归档（懒加载 reader）。
                let rk = RegionKey {
                    x: x >> 5,
                    z: z >> 5,
                };
                if self.cold_lookup.contains_key(&rk) {
                    // 外层 map 锁内取走 Arc 即放锁；归档本体锁内完成读取。
                    let reader = self.cold_reader(&rk)?;
                    return reader.lock().unwrap_or_poisoned().get(x, z, type_id);
                }
                return Ok(None);
            }
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
            if self.segs.get(&id).is_none_or(|s| s.incremental.is_empty()) {
                continue;
            }
            // 旧页：优先缓存，回落磁盘。
            let cached = self.cache.lock().unwrap_or_poisoned().get(id);
            let old = match cached {
                Some(p) => p,
                None => self
                    .load_page(id)?
                    .unwrap_or_else(|| Arc::new(IndexPage::from_entries(Vec::new()))),
            };
            let mut entries: Vec<(IndexKey, IndexVal)> = old.iter().cloned().collect();
            {
                let st = self.segs.get_mut(&id).expect("iterated from segs");
                entries.append(&mut st.incremental);
            }
            // from_entries 排序并对同键保留最大 gen；覆盖式 rename 原子替换。
            let page = Arc::new(IndexPage::from_entries(entries));
            write_index_page(&self.root, id, &page)?;
            self.cache.lock().unwrap_or_poisoned().put(id, page);
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

    /// 分配新段：段文件创建（头落盘）→ manifest 立即登记并保存 → 返回写入器。
    ///
    /// 崩溃任何时刻都不会留下"epoch 条目引用 manifest 不认识的段"的洞：
    /// 登记紧随文件持久化。同名残留孤儿文件（上次崩溃在旧代码"创建后未登记"
    /// 窗口留下）必然不可达——可达段都已被回放/rebuild 认领——直接删除重建。
    pub(crate) fn alloc_segment(
        &mut self,
        bucket: Bucket,
    ) -> Result<(u32, SegmentWriter), StrataError> {
        let id = self.manifest.next_seg_id;
        let path = seg_path(&self.root, id);
        if path.exists() {
            remove_file_with_retry(&path).map_err(|e| {
                StrataError::Manifest(format!("删除孤儿段文件 `{}` 失败: {e}", path.display()))
            })?;
            // 孤儿段的索引页（若存在）必然陈旧——它指向的偏移属于已被删除的
            // 旧段内容；留下会被 flush 合并进新页，造成同键同 gen 的幽灵条目
            // 与真实记录非确定性竞争（审计 flaky：keep-a 读回 None）。随段同删。
            let ix = ix_path(&self.root, id);
            if ix.exists() {
                remove_file_with_retry(&ix).map_err(|e| {
                    StrataError::Manifest(format!("删除孤儿段索引 `{}` 失败: {e}", ix.display()))
                })?;
            }
        }
        let mut w = SegmentWriter::create(&path, id)?;
        // 文件头先持久化，再登记：盘上 manifest 永远认识已创建的段。
        w.fsync()?;

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
        self.manifest.save(&self.root)?;
        Ok((id, w))
    }

    /// 删除一个段：先除名并持久化 manifest，然后才删文件（含索引页）。
    ///
    /// 崩溃在文件删除中途也不会留下"manifest 引用已删段"的悬空；残留文件
    /// 会在下次 `alloc_segment` 撞号时按孤儿接管，或被 rebuild 路径清理。
    pub(crate) fn remove_segment(&mut self, seg_id: u32) -> Result<(), StrataError> {
        self.segs.remove(&seg_id);
        self.manifest.segments.retain(|m| m.id != seg_id);
        self.manifest.save(&self.root)?;
        self.cache.lock().unwrap_or_poisoned().evict(seg_id);

        let p = seg_path(&self.root, seg_id);
        if p.exists() {
            remove_file_with_retry(&p).map_err(|e| {
                StrataError::Manifest(format!("删除段文件 `{}` 失败: {e}", p.display()))
            })?;
        }
        let ix = ix_path(&self.root, seg_id);
        if ix.exists() {
            remove_file_with_retry(&ix).map_err(|e| {
                StrataError::Manifest(format!("删除段索引 `{}` 失败: {e}", ix.display()))
            })?;
        }
        Ok(())
    }

    /// 读磁盘索引页：缓存优先（SIEVE），未命中读盘后回填缓存。
    /// 不存在 → `None`。`&self` 读路径可并发（缓存在 `Mutex` 后）。
    pub(crate) fn load_page(&self, seg_id: u32) -> Result<Option<Arc<IndexPage>>, StrataError> {
        if let Some(p) = self.cache.lock().unwrap_or_poisoned().get(seg_id) {
            return Ok(Some(p));
        }
        match std::fs::read(ix_path(&self.root, seg_id)) {
            Ok(bytes) => {
                let page = Arc::new(IndexPage::deserialize(&bytes)?);
                self.cache
                    .lock()
                    .unwrap_or_poisoned()
                    .put(seg_id, Arc::clone(&page));
                Ok(Some(page))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(StrataError::Io(e)),
        }
    }

    /// flush 收尾：以"每键最大 gen"的全局视图重算每段 live_bytes。
    fn recompute_live_bytes(&mut self) -> Result<(), StrataError> {
        let ids: Vec<u32> = self.manifest.segments.iter().map(|m| m.id).collect();
        let mut latest: HashMap<IndexKey, IndexVal> = HashMap::new();
        for id in ids {
            let cached = self.cache.lock().unwrap_or_poisoned().get(id);
            let page = match cached {
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
        for v in latest.values() {
            if let Some(m) = self.manifest.segments.iter_mut().find(|m| m.id == v.seg_id) {
                m.live_bytes += ENVELOPE_SIZE as u64 + v.payload_len as u64;
            }
        }
        Ok(())
    }

    /// 若 `(x, z, type_id)` 所在 region 已有冷归档，失效其槽位并在 manifest 记账。
    ///
    /// `write` 收尾调用：热层新写覆盖冷槽后，冷副本不再是最新版本。
    ///
    /// demote 回写期间（[`Store::demote_in_progress`]）抑制：此时冷区即将
    /// 除名，若照常吃失效记账，崩溃在"回写已进日志、manifest 未除名"窗口时
    /// 回放会跳过冷区键（回写丢失）而槽位已失效——两个副本同时蒸发。
    pub(crate) fn invalidate_cold_slot(
        &mut self,
        x: i32,
        z: i32,
        type_id: u16,
    ) -> Result<(), StrataError> {
        if self.demote_in_progress {
            return Ok(());
        }
        let rk = RegionKey {
            x: x >> 5,
            z: z >> 5,
        };
        let Some(&idx) = self.cold_lookup.get(&rk) else {
            return Ok(());
        };
        let reader = self.cold_reader(&rk)?;
        let first = reader.lock().unwrap_or_poisoned().invalidate(x, z, type_id)?;
        if first {
            if let Some(c) = self.manifest.cold.get_mut(idx) {
                c.invalid_count += 1;
            }
        }
        Ok(())
    }

    /// 懒加载冷归档读取器（外层 map 锁内只做查表/插入，取走 `Arc` 即放锁）。
    pub(crate) fn cold_reader(
        &self,
        rk: &RegionKey,
    ) -> Result<Arc<Mutex<ArchiveReader>>, StrataError> {
        let mut readers = self.cold_readers.lock().unwrap_or_poisoned();
        if let Some(r) = readers.get(rk) {
            return Ok(Arc::clone(r));
        }
        let path = cold_path(&self.root, rk.x, rk.z);
        let reader = Arc::new(Mutex::new(ArchiveReader::open(&path)?));
        readers.insert(rk.clone(), Arc::clone(&reader));
        Ok(reader)
    }

    /// 由 `manifest.cold` 重建 [`Store::cold_lookup`]（cold 增删后调用）。
    pub(crate) fn rebuild_cold_lookup(&mut self) {
        self.cold_lookup = self
            .manifest
            .cold
            .iter()
            .enumerate()
            .map(|(i, c)| (RegionKey { x: c.region_x, z: c.region_z }, i))
            .collect();
    }

    /// epoch 回放认领孤儿段：manifest 不认识 `seg_id` 但段文件存在且段头
    /// 有效 → 按段头登记进段表。返回是否认领成功（文件缺失/头坏 → false，
    /// 调用方丢弃对应条目）。
    fn claim_segment(&mut self, seg_id: u32) -> Result<bool, StrataError> {
        let path = seg_path(&self.root, seg_id);
        if !segment_header_ok(&path, seg_id) {
            return Ok(false);
        }
        let epoch = self.manifest.epoch;
        self.manifest.segments.push(SegmentMeta {
            id: seg_id,
            live_bytes: 0,
            total_bytes: 0,
            bucket: Bucket::Young,
            created_epoch: epoch,
            last_rewrite_epoch: epoch,
        });
        self.segs.insert(seg_id, SegState::new());
        if seg_id >= self.manifest.next_seg_id {
            self.manifest.next_seg_id = seg_id + 1;
        }
        Ok(true)
    }

    /// 冷区对账（open 时调用，先于 epoch 回放）：
    /// - 未登记但可解析的 `.varc` → 重新注册（晋升在"文件落盘→登记"之间
    ///   崩溃时文件先于登记存在；读路径热层最新优先，重注册不遮蔽新写）；
    /// - 未登记且不可解析的半截 `.varc` → 删除（写归档中途崩溃的残留，
    ///   此时热层尚未 purge，数据在热层完整）；
    /// - 已登记但文件缺失 → 除名。
    ///
    /// 返回 manifest 是否被修改。
    fn reconcile_cold(&mut self) -> Result<bool, StrataError> {
        let mut dirty = false;
        let dir = self.root.join(COLD_DIR);
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for entry in rd.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                let Some(rest) = name.strip_suffix(".varc") else {
                    continue;
                };
                let Some(rest) = rest.strip_prefix("r.") else {
                    continue;
                };
                let Some((xs, zs)) = rest.split_once('.') else {
                    continue;
                };
                let (Ok(rx), Ok(rz)) = (xs.parse::<i32>(), zs.parse::<i32>()) else {
                    continue;
                };
                let rk = RegionKey { x: rx, z: rz };
                if self.cold_lookup.contains_key(&rk) {
                    continue;
                }
                match ArchiveReader::open(&entry.path()) {
                    Ok(r) => {
                        self.manifest.cold.push(ColdMeta {
                            region_x: rx,
                            region_z: rz,
                            invalid_count: r.invalid_count(),
                            total_slots: r.total_slots(),
                        });
                        self.cold_lookup
                            .insert(rk, self.manifest.cold.len() - 1);
                        dirty = true;
                    }
                    Err(_) => {
                        let inv = entry.path().with_extension("varc.inv");
                        let _ = remove_file_with_retry(&entry.path());
                        if inv.exists() {
                            let _ = remove_file_with_retry(&inv);
                        }
                    }
                }
            }
        }

        let before = self.manifest.cold.len();
        self.manifest
            .cold
            .retain(|c| cold_path(&self.root, c.region_x, c.region_z).exists());
        if self.manifest.cold.len() != before {
            dirty = true;
        }
        self.rebuild_cold_lookup();
        Ok(dirty)
    }
}

/// 段磁盘索引页原子落盘：tmp + fsync + 覆盖式 rename（不预删旧文件，
/// 杜绝"旧页已删、新页未就位"的崩溃窗口；rename 失败才走删除+重试兜底）。
pub(crate) fn write_index_page(root: &Path, seg_id: u32, page: &IndexPage) -> Result<(), StrataError> {
    let final_path = ix_path(root, seg_id);
    let tmp_path = final_path.with_extension("vix.tmp");
    let bytes = page.serialize();
    let mut f = File::create(&tmp_path)?;
    f.write_all(&bytes)?;
    f.sync_all()?;
    rename_replace(&tmp_path, &final_path)
}

#[cfg(test)]
mod sync_traits_tests {
    use super::Store;

    fn assert_send<T: Send>() {}
    fn assert_sync<T: Sync>() {}

    #[test]
    fn store_is_send_and_sync() {
        assert_send::<Store>();
        assert_sync::<Store>();
    }
}
