# Strata Core (Phase 1) Implementation Plan — v2

> ✅ **状态：已全部完成**（2026-08-04）。17 个任务全部落地并验证：70+ 测试全绿、clippy -D warnings 零告警、CI 双平台（Windows + Linux）全绿、基准见 `benches/RESULTS.md`（体积 0.097×、读 p99 11µs）。后续 Phase 2（FFI/JNI/Canvas 集成）与多世界/多维度扩展见设计规格 §11 与提交历史；文中 `- [ ]` 复选框为执行时原样保留，不代表未完成。

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 实现 Strata 存储引擎纯 Rust 核心（`strata-core`）与离线工具（`strata-cli`）：40B 信封、段日志引擎、三层有界索引（位图+SIEVE+磁盘索引页）、epoch 崩溃恢复、影子双副本 manifest、三档 GC（hole-punch/删除/压实打分）、分块冷归档（superfeatures 排序 + per-type 字典）、Cesium 式双向转换（覆盖、保留源、进度恢复）、`strata.properties` 配置矩阵——全部独立于 JVM，可测试、可基准，**内存与世界大小无关**。

**Architecture:** 每 dimension 一个存储池：追加式段文件（regionizer 分区分片）+ 三层索引（L0 位图常驻 / L1 SIEVE 有界缓存 / L2 磁盘索引页）+ epoch 日志（对齐 autosave）+ 影子双副本 manifest；稳定 region 晋升为分块固态归档 `.varc`。GC 三档：hole-punch（Linux fallocate / Windows FSCTL_SET_ZERO_DATA）→ 整段删除 → 打分压实。Rust 永不解析 NBT 负载。

**Tech Stack:** Rust stable（edition 2021）；`zstd`、`xxhash-rust`（xxh64）、`thiserror`、`clap`（CLI）、`criterion`（基准）、`proptest`（属性测试）、`tempfile`；Windows 下 `windows-sys`（FSCTL_SET_ZERO_DATA）。

## Global Constraints

- 所有磁盘整数**小端**；信封头定长 **40 字节**，magic `b"VSEG"`（规格 §7）。
- Rust 侧**永不解析 NBT 负载内容**——负载是不透明 `&[u8]`。
- `type_id` 未知值必须原样透传。
- 崩溃一致性 = 原版等价：最多丢一个未 fsync 的 epoch 周期；epoch 边界由 `flush()` 触发。
- 每条记录带 `xxhash64(压缩负载)`，单条损坏只隔离该记录。
- **启动只读 manifest + 位图，O(段数)；全扫描仅为恢复/verify 路径。**
- **内存上界**：L1 索引页缓存与冷块缓存共享 `strata.index.cache-mb` 预算（默认 512）。
- GC hole-punch 最小洞尺寸 64KB（NTFS 稀疏粒度）。
- 转换**绝不删除源格式文件**；重复执行同方向转换直接覆盖目标（Cesium wiki 行为）；转换结束打印保留提示。
- 配置载体 `strata.properties`；非法值报错指明行号；组合合法性按规格 §9 矩阵（含 WARN 语义）。
- `strata.enabled` 默认 **false**。
- 测试可在 Windows stable Rust 上运行；每个任务结束必须 commit；TDD。
- 目标平台：`x86_64-pc-windows-msvc` 与 `x86_64-unknown-linux-gnu`。

---

## File Structure

```
Cargo.toml                      # workspace: crates/strata-core, crates/strata-cli
crates/strata-core/
  src/lib.rs                    # re-exports
  src/error.rs                  # 统一错误类型
  src/envelope.rs               # 40B 信封编解码
  src/codec.rs                  # 压缩注册表（None/Zstd，comp_id 双语义）
  src/dict.rs                   # zstd 字典训练与槽管理（Phase 1 仅训练+加载）
  src/segment.rs                # 段文件追加 + 扫描
  src/index.rs                  # 三层索引：位图 + SIEVE 缓存 + 磁盘索引页
  src/punch.rs                  # hole-punch 跨平台抽象
  src/epoch.rs                  # epoch 日志写入 + 回放
  src/manifest.rs               # 影子双副本 manifest（含 region 位图快照）
  src/store.rs                  # Store 门面（open/read/write/flush/recover/verify）
  src/gc.rs                     # 分桶 + 三档 GC（打分选受害者）
  src/cold.rs                   # .varc 分块归档（superfeatures 排序）
  src/tier.rs                   # 热↔冷迁移策略
  tests/roundtrip.rs            # 端到端集成测试
crates/strata-cli/
  src/main.rs                   # clap 子命令
  src/anvil.rs                  # Anvil .mca/.mcc 读写
  src/config.rs                 # strata.properties 加载器 + 组合矩阵
  tests/cli.rs
benches/vs_anvil.rs             # criterion 基准
```

---

### Task 1: Workspace 脚手架

**Files:**
- Create: `Cargo.toml`, `crates/strata-core/Cargo.toml`, `crates/strata-core/src/lib.rs`, `crates/strata-core/src/error.rs`, `crates/strata-cli/Cargo.toml`, `crates/strata-cli/src/main.rs`, `.gitignore`

**Interfaces:**
- Produces: 可编译 workspace；`strata_core::StrataError` 枚举（`Io`, `Envelope(String)`, `Codec(String)`, `Corrupt { path, detail }`, `Manifest(String)`, `Config { file: String, line: u32, detail: String }`）

- [ ] **Step 1: 安装 Rust 工具链（若缺失）**

Run: `rustup default stable && rustc --version`
Expected: `rustc 1.xx.x`（Windows 先运行 rustup-init.exe）

- [ ] **Step 2: 创建 workspace 与两个 crate**

`Cargo.toml`:
```toml
[workspace]
resolver = "2"
members = ["crates/strata-core", "crates/strata-cli"]

[workspace.dependencies]
zstd = "0.13"
xxhash-rust = { version = "0.8", features = ["xxh64"] }
thiserror = "2"
```

`crates/strata-core/Cargo.toml`:
```toml
[package]
name = "strata-core"
version = "0.2.0"
edition = "2021"

[dependencies]
zstd = { workspace = true }
xxhash-rust = { workspace = true }
thiserror = { workspace = true }

[target.'cfg(windows)'.dependencies]
windows-sys = { version = "0.59", features = ["Win32_Storage_FileSystem", "Win32_Foundation"] }

[dev-dependencies]
tempfile = "3"
proptest = "1"
```

`crates/strata-cli/Cargo.toml`:
```toml
[package]
name = "strata-cli"
version = "0.2.0"
edition = "2021"

[dependencies]
strata-core = { path = "../strata-core" }
clap = { version = "4", features = ["derive"] }
anyhow = "1"
zstd = { workspace = true }
flate2 = "1"
lz4_flex = "0.11"

[dev-dependencies]
criterion = { version = "0.5", features = ["html_reports"] }
tempfile = "3"

[[bench]]
name = "vs_anvil"
harness = false
```

`crates/strata-core/src/error.rs`:
```rust
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StrataError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("envelope: {0}")]
    Envelope(String),
    #[error("codec: {0}")]
    Codec(String),
    #[error("corrupt at {path}: {detail}")]
    Corrupt { path: String, detail: String },
    #[error("manifest: {0}")]
    Manifest(String),
    #[error("config {file}:{line}: {detail}")]
    Config { file: String, line: u32, detail: String },
}
```

`crates/strata-core/src/lib.rs`:
```rust
pub mod error;
pub use error::StrataError;
```

`crates/strata-cli/src/main.rs`:
```rust
fn main() {
    println!("strata-cli stub");
}
```

`.gitignore`: `/target`

- [ ] **Step 3: 构建验证**

Run: `cargo build && cargo test`
Expected: BUILD OK，0 测试

- [ ] **Step 4: Commit**

```bash
git add . && git commit -m "feat: strata workspace scaffolding (v2)"
```

---

### Task 2: 信封格式（40B 定长头）

**Files:**
- Create: `crates/strata-core/src/envelope.rs`
- Test: 内嵌 `#[cfg(test)]` + `tests/envelope_proptest.rs`

**Interfaces:**
- Produces: `ENVELOPE_SIZE: usize = 40`, `MAGIC: [u8;4]`, `Envelope` 与 `encode/decode`——后续所有任务的记录外壳。

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Envelope {
        Envelope { record_ver: 1, type_id: 7, comp_id: 0x10, chunk_x: -33, chunk_z: 4095,
                   gen: 42, epoch_ts: 1_234_567, payload_len: 1234, payload_hash: 0xDEAD_BEEF }
    }

    #[test]
    fn roundtrip() {
        let mut buf = [0u8; ENVELOPE_SIZE];
        sample().encode(&mut buf);
        assert_eq!(&buf[0..4], b"VSEG");
        assert_eq!(Envelope::decode(&buf).unwrap(), sample());
    }

    #[test]
    fn bad_magic_rejected() {
        let mut buf = [0u8; ENVELOPE_SIZE];
        sample().encode(&mut buf);
        buf[0] = b'X';
        assert!(Envelope::decode(&buf).is_err());
    }

    #[test]
    fn field_offsets_little_endian() {
        let mut buf = [0u8; ENVELOPE_SIZE];
        sample().encode(&mut buf);
        assert_eq!(buf[4], 1);                                             // record_ver
        assert_eq!(u16::from_le_bytes([buf[5], buf[6]]), 7);               // type_id
        assert_eq!(buf[7], 0x10);                                          // comp_id
        assert_eq!(i32::from_le_bytes(buf[8..12].try_into().unwrap()), -33); // chunk_x
        assert_eq!(u64::from_le_bytes(buf[16..24].try_into().unwrap()), 42); // gen
        assert_eq!(u32::from_le_bytes(buf[24..28].try_into().unwrap()), 1_234_567); // epoch_ts
        assert_eq!(u32::from_le_bytes(buf[28..32].try_into().unwrap()), 1234); // payload_len
        assert_eq!(u64::from_le_bytes(buf[32..40].try_into().unwrap()), 0xDEAD_BEEF); // hash
    }
}
```

- [ ] **Step 2: 确认失败** — Run: `cargo test -p strata-core envelope`（编译错误）

- [ ] **Step 3: 实现**

```rust
use crate::StrataError;

pub const ENVELOPE_SIZE: usize = 40;
pub const MAGIC: [u8; 4] = *b"VSEG";

/// 记录外壳。负载（NBT）永不解析。comp_id: 低4位 codec，高4位字典槽。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    pub record_ver: u8,
    pub type_id: u16,
    pub comp_id: u8,
    pub chunk_x: i32,
    pub chunk_z: i32,
    pub gen: u64,
    pub epoch_ts: u32,
    pub payload_len: u32,
    pub payload_hash: u64,
}

impl Envelope {
    pub fn encode(&self, out: &mut [u8; ENVELOPE_SIZE]) {
        out[0..4].copy_from_slice(&MAGIC);
        out[4] = self.record_ver;
        out[5..7].copy_from_slice(&self.type_id.to_le_bytes());
        out[7] = self.comp_id;
        out[8..12].copy_from_slice(&self.chunk_x.to_le_bytes());
        out[12..16].copy_from_slice(&self.chunk_z.to_le_bytes());
        out[16..24].copy_from_slice(&self.gen.to_le_bytes());
        out[24..28].copy_from_slice(&self.epoch_ts.to_le_bytes());
        out[28..32].copy_from_slice(&self.payload_len.to_le_bytes());
        out[32..40].copy_from_slice(&self.payload_hash.to_le_bytes());
    }

    pub fn decode(b: &[u8; ENVELOPE_SIZE]) -> Result<Self, StrataError> {
        if b[0..4] != MAGIC {
            return Err(StrataError::Envelope("bad magic".into()));
        }
        Ok(Self {
            record_ver: b[4],
            type_id: u16::from_le_bytes([b[5], b[6]]),
            comp_id: b[7],
            chunk_x: i32::from_le_bytes(b[8..12].try_into().unwrap()),
            chunk_z: i32::from_le_bytes(b[12..16].try_into().unwrap()),
            gen: u64::from_le_bytes(b[16..24].try_into().unwrap()),
            epoch_ts: u32::from_le_bytes(b[24..28].try_into().unwrap()),
            payload_len: u32::from_le_bytes(b[28..32].try_into().unwrap()),
            payload_hash: u64::from_le_bytes(b[32..40].try_into().unwrap()),
        })
    }
}
```

`lib.rs` 追加 `pub mod envelope;`。

- [ ] **Step 4: 确认通过** — Run: `cargo test -p strata-core envelope`，3 PASS

- [ ] **Step 5: 属性测试** `tests/envelope_proptest.rs`（任意字段往返 + 单比特翻转必检出，同 v1 计划的 proptest 模板，字段按新结构）

- [ ] **Step 6: 运行并 commit**

```bash
git add crates/strata-core && git commit -m "feat(core): 40-byte envelope format with proptest coverage"
```

---

### Task 3: 压缩编解码注册表（comp_id 双语义）

**Files:**
- Create: `crates/strata-core/src/codec.rs`, `crates/strata-core/src/dict.rs`
- Test: 内嵌 `#[cfg(test)]`

**Interfaces:**
- Consumes: `StrataError::Codec`
- Produces: `CODEC_NONE: u8 = 0`, `CODEC_ZSTD: u8 = 1`；`codec_id(comp_id) -> u8`（低 4 位）、`dict_slot(comp_id) -> u8`（高 4 位）、`make_comp_id(codec, slot) -> u8`；`Codec` trait 与 `codec_for(comp_id, zstd_level, dict: Option<&[u8]>) -> Result<Box<dyn Codec>>`；`dict.rs`: `train_dictionary(samples: &[&[u8]]) -> Result<Vec<u8>>`（zstd 训练，samples ≥100 条且总 ≥100KB，不足返回空字典）

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comp_id_bit_semantics() {
        let id = make_comp_id(CODEC_ZSTD, 3);
        assert_eq!(codec_id(id), CODEC_ZSTD);
        assert_eq!(dict_slot(id), 3);
    }

    #[test]
    fn zstd_roundtrip() {
        let data = vec![7u8; 64 * 1024];
        let c = codec_for(make_comp_id(CODEC_ZSTD, 0), 3, None).unwrap();
        let mut comp = Vec::new();
        c.compress(&data, &mut comp).unwrap();
        assert!(comp.len() < data.len() / 10);
        let mut out = Vec::new();
        c.decompress(&comp, &mut out).unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn dictionary_improves_small_objects() {
        // 100 条结构相似的小样本训练字典，压缩同构小对象应比无字典小
        let samples: Vec<Vec<u8>> = (0..120).map(|i| {
            let mut v = vec![0xAB; 40];
            v.extend_from_slice(&i.to_le_bytes());
            v.extend_from_slice(b"minecraft:stone");
            v
        }).collect();
        let refs: Vec<&[u8]> = samples.iter().map(|v| v.as_slice()).collect();
        let dict = train_dictionary(&refs).unwrap();
        assert!(!dict.is_empty());
        let c = codec_for(make_comp_id(CODEC_ZSTD, 0), 3, Some(&dict)).unwrap();
        let mut comp = Vec::new();
        c.compress(&samples[0], &mut comp).unwrap();
        let c2 = codec_for(make_comp_id(CODEC_ZSTD, 0), 3, None).unwrap();
        let mut comp2 = Vec::new();
        c2.compress(&samples[0], &mut comp2).unwrap();
        assert!(comp.len() < comp2.len());
    }

    #[test]
    fn unknown_codec_rejected() {
        assert!(codec_for(make_comp_id(15, 0), 3, None).is_err());
    }
}
```

- [ ] **Step 2: 确认失败** — Run: `cargo test -p strata-core codec dict`

- [ ] **Step 3: 实现**
- `codec.rs`：`NoneCodec`（透传）；`ZstdCodec { level, dict: Option<Vec<u8>> }`——有字典时用 `zstd::dict::EncoderDictionary`/`DecoderDictionary`；`codec_for` 按 codec_id 分发，字典为 None 时 slot 必须为 0。
- `dict.rs`：`zstd::dict::from_samples`（字典大小 32KB），样本不足返回 `Ok(vec![])`。

- [ ] **Step 4: 确认通过** — Run: `cargo test -p strata-core`，4 PASS

- [ ] **Step 5: Commit**

```bash
git add crates/strata-core && git commit -m "feat(core): codec registry with comp_id dual semantics + dictionary training"
```

---

### Task 4: 段文件写入器

**Files:**
- Create: `crates/strata-core/src/segment.rs`（writer）
- Test: 内嵌 `#[cfg(test)]`

**Interfaces:**
- Consumes: `Envelope`
- Produces: `SEG_HEADER_SIZE: u64 = 16`, `SEG_MAGIC = b"VS01"`；`SegmentWriter::{create(path, seg_id), append(env, payload) -> offset, fsync, offset, close}`。格式 = 16B 文件头（magic + seg_id LE + 8B reserved）+ 信封序列。

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn append_returns_correct_offsets() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("seg-0001.vseg");
    let mut w = SegmentWriter::create(&path, 1).unwrap();
    assert_eq!(w.offset(), 16);
    let o1 = w.append(&env(5), b"AAAAA").unwrap();
    assert_eq!(o1, 16);
    let o2 = w.append(&env(3), b"BBB").unwrap();
    assert_eq!(o2, 16 + ENVELOPE_SIZE as u64 + 5);
    w.close().unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 16 + 2 * ENVELOPE_SIZE as u64 + 8);
}
```
（`env(len)` 辅助函数构造 Envelope，`payload_hash` 由调用方填。）

- [ ] **Step 2: 确认失败** — Run: `cargo test -p strata-core segment`

- [ ] **Step 3: 实现**（`BufWriter` 追加；`append` 返回头偏移；`fsync` = flush + `sync_all`）

- [ ] **Step 4: 确认通过** — Run: `cargo test -p strata-core segment`，PASS

- [ ] **Step 5: Commit**

```bash
git add crates/strata-core && git commit -m "feat(core): segment file writer"
```

---

### Task 5: 段文件扫描器（损坏隔离 + 重同步）

**Files:**
- Modify: `crates/strata-core/src/segment.rs`（scan）

**Interfaces:**
- Produces: `ScannedRecord { env, offset, payload }`；`scan_segment(path) -> Result<ScanResult { records, truncated_tail: bool }>`。契约：尾部坏/不足 → `truncated_tail=true` 停止；中部坏 → `StrataError::Corrupt`；hash 不符 → 该记录 `payload_hash=0` 保留继续；**magic 扫描重同步**：遇到坏字节时向前找下一个 `b"VSEG"`（最多 64KB），找到则继续扫描并计入 `resync_count`。

- [ ] **Step 1: 写失败测试**（roundtrip / truncated_tail_tolerated / hash_mismatch_flagged / resync_after_garbage 四例——构造：两条好记录中间插入 100B 垃圾 + 一条好记录，断言 resync_count==1 且 3 条记录全找回）

- [ ] **Step 2: 确认失败** — Run: `cargo test -p strata-core scan`

- [ ] **Step 3: 实现 `scan_segment`**（顺序读 40B 头→decode→读 payload→校验；decode 失败走重同步窗口搜索）

- [ ] **Step 4: 确认通过** — Run: `cargo test -p strata-core segment`，4 PASS

- [ ] **Step 5: Commit**

```bash
git add crates/strata-core && git commit -m "feat(core): segment scanner with corruption isolation and magic resync"
```

---

### Task 6: 三层索引（位图 + SIEVE + 磁盘索引页）

**Files:**
- Create: `crates/strata-core/src/index.rs`
- Test: 内嵌 `#[cfg(test)]` + `tests/index_memory_bound.rs`

**Interfaces:**
- Produces:
```rust
pub struct IndexKey { pub x: i32, pub z: i32, pub type_id: u16 } // Hash+Eq+Ord
pub struct IndexVal { pub seg_id: u32, pub offset: u64, pub payload_len: u32, pub gen: u64, pub comp_id: u8 }

pub struct RegionBitmap { /* [u8; 128*3] 每槽 3 字节位图，type 0..2；位 idx = (z&31)*32+(x&31) */ }
impl RegionBitmap { pub fn new() -> Self; pub fn set(&mut self, x: i32, z: i32, t: u16); pub fn has(&self, x: i32, z: i32, t: u16) -> bool; }

pub struct IndexPage { /* 排序 Vec<(IndexKey, IndexVal)>，序列化：条目数 + 键前缀压缩（相邻条目共享 x） */ }
impl IndexPage {
    pub fn from_entries(entries: Vec<(IndexKey, IndexVal)>) -> Self; // 排序 + 去重（同键留最大 gen）
    pub fn serialize(&self) -> Vec<u8>;
    pub fn deserialize(b: &[u8]) -> Result<Self, StrataError>;
    pub fn lookup(&self, k: &IndexKey) -> Option<&IndexVal>; // 二分
}

pub struct SieveCache { /* 双向链表 + hand 指针，容量字节数上界 */ }
impl SieveCache {
    pub fn new(max_bytes: u64) -> Self;
    pub fn get(&mut self, seg_id: u32) -> Option<std::sync::Arc<IndexPage>>; // 只置 visited 位
    pub fn put(&mut self, seg_id: u32, page: std::sync::Arc<IndexPage>);     // 超容量按 hand 扫描淘汰
    pub fn evict(&mut self, seg_id: u32);                                   // 段删除时失效
    pub fn len_bytes(&self) -> u64;
}

pub struct RegionIndex { /* Store 持有：每段 RegionBitmap + SegMeta；SieveCache 共享预算 */ }
impl RegionIndex {
    pub fn contains(&self, seg_id: u32, x: i32, z: i32, t: u16) -> bool; // O(1) 位图
    // Store 层组合：位图判存在 → sieve.get(seg) → miss 时读 .vix 文件并 put
}
```
SIEVE 语义（NSDI'24）：访问只置 visited；淘汰时 hand 从某点扫描，visited=1 清 0 跳过，visited=0 驱逐；容量按索引页字节计费。

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn bitmap_set_has_o1() {
    let mut bm = RegionBitmap::new();
    assert!(!bm.has(3, 7, 0));
    bm.set(3, 7, 0); bm.set(-1, -1, 2); // 负坐标经 &31 归一
    assert!(bm.has(3, 7, 0));
    assert!(!bm.has(3, 7, 1));           // type 隔离
    assert!(bm.has(-1, -1, 2));
}

#[test]
fn index_page_roundtrip_and_latest_gen() {
    let entries = vec![
        (IndexKey { x: 1, z: 1, type_id: 0 }, IndexVal { seg_id: 1, offset: 16, payload_len: 5, gen: 1, comp_id: 1 }),
        (IndexKey { x: 1, z: 1, type_id: 0 }, IndexVal { seg_id: 2, offset: 99, payload_len: 5, gen: 3, comp_id: 1 }),
        (IndexKey { x: 2, z: 9, type_id: 1 }, IndexVal { seg_id: 1, offset: 200, payload_len: 9, gen: 2, comp_id: 1 }),
    ];
    let page = IndexPage::from_entries(entries);
    let bytes = page.serialize();
    let page2 = IndexPage::deserialize(&bytes).unwrap();
    let v = page2.lookup(&IndexKey { x: 1, z: 1, type_id: 0 }).unwrap();
    assert_eq!(v.gen, 3); // 同键最新代胜出
    assert!(bytes.len() < 3 * 40); // 前缀压缩有效（上限宽松）
}

#[test]
fn sieve_evicts_unvisited_keeps_visited_bounded() {
    let mut c = SieveCache::new(1000);
    for i in 0..10u32 { c.put(i, std::sync::Arc::new(sample_page(200))); } // 10×200 > 1000
    c.get(&0); // 标记 visited
    while c.len_bytes() > 1000 { c.evict_one(); }
    assert!(c.len_bytes() <= 1000);
    assert!(c.get(&0).is_some()); // visited 页被保留（若尚未轮到它则至少断言容量上界成立）
}
```

- [ ] **Step 2: 确认失败** — Run: `cargo test -p strata-core index sieve bitmap`

- [ ] **Step 3: 实现**
- 位图：固定 `[u8; 384]`（3 类型 × 1024 bit）。
- IndexPage 序列化：`u32 count` + 逐条 `(x_delta varint, z i32, type u16, seg u32, offset u64, len u32, gen u64, comp u8)`。
- SieveCache：`Vec<Node>` + 逻辑双向链（数组实现），hand usize；`put` 前检查容量，循环调内部 `evict_one` 直到装下。

- [ ] **Step 4: 内存上界属性测试** `tests/index_memory_bound.rs`：插入 10 万条目（100 个段页）进 `SieveCache::new(512*1024)`，断言 `len_bytes() <= 512KB` 且 lookup 正确性不丢（miss 仅因淘汰）。

- [ ] **Step 5: Commit**

```bash
git add crates/strata-core && git commit -m "feat(core): three-tier index (bitmap + SIEVE cache + disk index pages)"
```

---

### Task 7: hole-punch 跨平台抽象

**Files:**
- Create: `crates/strata-core/src/punch.rs`
- Test: 内嵌 `#[cfg(test)]`

**Interfaces:**
- Produces: `pub fn punch_hole(file: &mut File, offset: u64, len: u64) -> Result<PunchOutcome, StrataError>`；`PunchOutcome::{Done, Unsupported}`。契约：len < 64KB → 直接返回 Unsupported（调用方改走压实）；Linux `libc::fallocate(FALLOC_FL_PUNCH_HOLE|FALLOC_FL_KEEP_SIZE)`；Windows `DeviceIoControl(FSCTL_SET_ZERO_DATA)`（需 sparse file 属性，失败则 Unsupported）；Unsupported 时文件内容不变。

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn punch_makes_range_zero_or_reports_unsupported() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.bin");
    std::fs::write(&path, vec![0xAAu8; 256 * 1024]).unwrap();
    let mut f = std::fs::OpenOptions::new().read(true).write(true).open(&path).unwrap();
    let r = punch_hole(&mut f, 64 * 1024, 64 * 1024).unwrap();
    let data = std::fs::read(&path).unwrap();
    match r {
        PunchOutcome::Done => assert!(data[64*1024..128*1024].iter().all(|&b| b == 0)),
        PunchOutcome::Unsupported => assert!(data.iter().all(|&b| b == 0xAA)), // 内容不变
    }
}

#[test]
fn too_small_hole_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.bin");
    std::fs::write(&path, vec![0u8; 128 * 1024]).unwrap();
    let mut f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    assert!(matches!(punch_hole(&mut f, 0, 4096).unwrap(), PunchOutcome::Unsupported));
}
```

- [ ] **Step 2: 确认失败** — Run: `cargo test -p strata-core punch`

- [ ] **Step 3: 实现**（`#[cfg(unix)]` / `#[cfg(windows)]` 双实现；Windows 需先 `FSCTL_SET_SPARSE`；`Cargo.toml` unix 侧加 `libc = "0.2"`）

- [ ] **Step 4: 确认通过** — Run: `cargo test -p strata-core punch`，2 PASS

- [ ] **Step 5: Commit**

```bash
git add crates/strata-core && git commit -m "feat(core): cross-platform hole punch (fallocate / FSCTL_SET_ZERO_DATA)"
```

---

### Task 8: epoch 日志与回放

**Files:**
- Create: `crates/strata-core/src/epoch.rs`
- Test: 内嵌 `#[cfg(test)]`

**Interfaces:**
- Consumes: `Envelope`
- Produces: `EpochEntry { seg_id: u32, env: Envelope, offset: u64 }`（64B/条：4+40+8+12B pad 对齐）；`EpochLog::{open(dir), record, rotate, replay}`；replay 尾部坏条目截断（崩溃容忍）。

- [ ] **Step 1: 写失败测试**（record_rotate_replay_cycle + torn_tail_entry_dropped，同 v1 计划）

- [ ] **Step 2: 确认失败** — Run: `cargo test -p strata-core epoch`

- [ ] **Step 3: 实现**（64B 定长条目追加；rotate = flush+sync_all+set_len(0)+sync）

- [ ] **Step 4: 确认通过** — Run: `cargo test -p strata-core epoch`，2 PASS

- [ ] **Step 5: Commit**

```bash
git add crates/strata-core && git commit -m "feat(core): epoch log with crash-tolerant replay"
```

---

### Task 9: Manifest 影子双副本

**Files:**
- Create: `crates/strata-core/src/manifest.rs`
- Test: 内嵌 `#[cfg(test)]`

**Interfaces:**
- Produces:
```rust
pub enum Bucket { Young, Active, Stable }
pub struct SegmentMeta { pub id: u32, pub live_bytes: u64, pub total_bytes: u64, pub bucket: Bucket,
                         pub created_epoch: u64, pub last_rewrite_epoch: u64 }
pub struct ColdMeta { pub region_x: i32, pub region_z: i32, pub invalid_count: u32, pub total_slots: u32 }
pub struct RegionKey { pub x: i32, pub z: i32 }
pub struct Manifest {
    pub format_version: u32,       // = 2
    pub epoch: u64,
    pub next_gen: u64,
    pub next_seg_id: u32,
    pub segments: Vec<SegmentMeta>,
    pub cold: Vec<ColdMeta>,
    pub region_bitmaps: Vec<(RegionKey, Vec<u8>)>, // 每段 384B 位图快照
    pub dict_slots: Vec<(u16, Vec<u8>)>,           // (type_id, 字典内容)，≤16 槽
}
impl Manifest {
    pub fn save(&self, dir: &Path) -> Result<(), StrataError>; // tmp+fsync → rename .bak → rename 主 → 目录 fsync
    pub fn load(dir: &Path) -> Result<Option<Manifest>, StrataError>; // 主坏切 .bak，都坏 Err(Corrupt)，都无 Ok(None)
}
```

- [ ] **Step 1: 写失败测试**（save_load_roundtrip_and_failover + empty_dir_loads_none，同 v1）

- [ ] **Step 2: 确认失败** — Run: `cargo test -p strata-core manifest`

- [ ] **Step 3: 实现**（手写小端序列化；body 前置 8B xxh64）

- [ ] **Step 4: 确认通过** — Run: `cargo test -p strata-core manifest`，2 PASS

- [ ] **Step 5: Commit**

```bash
git add crates/strata-core && git commit -m "feat(core): shadow dual-copy manifest with region bitmaps"
```

---

### Task 10: Store 门面（三层索引读路径 + 启动不扫段）

**Files:**
- Create: `crates/strata-core/src/store.rs`
- Test: `crates/strata-core/tests/roundtrip.rs`

**Interfaces:**
- Consumes: Tasks 2–9
- Produces:
```rust
pub struct StoreConfig {
    pub hot_level: i32,            // 默认 3
    pub hot_enabled: bool,         // 默认 true
    pub cold_level: i32,           // 默认 9
    pub cold_enabled: bool,        // 默认 true
    pub dictionary: bool,          // 默认 true
    pub cache_mb: u64,             // 默认 512（L1 索引页 + 冷块共享）
    pub segment_max_bytes: u64,    // 默认 64 MiB
}
impl Store {
    pub fn open(root: &Path, cfg: StoreConfig) -> Result<Store, StrataError>;
    // 启动：load manifest（None→新建）→ 装载段表/位图/dict_slots → 就绪。不扫描段文件。
    pub fn write(&mut self, x: i32, z: i32, type_id: u16, nbt: &[u8]) -> Result<(), StrataError>;
    // 压缩（当前 hot 配置 + 字典槽）→ gen → 段写（滚动）→ epoch.record → 位图 set → 索引 put
    pub fn read(&self, x: i32, z: i32, type_id: u16) -> Result<Option<Vec<u8>>, StrataError>;
    // 逐段位图判存在（无→None，零 IO）→ sieve/磁盘索引页定位 → 段读 → hash 校验 → 解压
    pub fn flush(&mut self) -> Result<(), StrataError>; // fsync 段 → epoch++ → manifest.save → epoch.rotate
    pub fn rebuild_index_from_scan(&mut self) -> Result<u64, StrataError>; // 恢复专用：全扫描重建，返回记录数
    pub fn verify(&self) -> Result<VerifyReport, StrataError>;
}
```

- [ ] **Step 1: 写失败测试** `tests/roundtrip.rs`

```rust
#[test]
fn write_read_roundtrip_and_startup_without_scan() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut s = Store::open(dir.path(), StoreConfig::default()).unwrap();
        s.write(10, -10, 0, &[1, 2, 3]).unwrap();
        s.write(10, -10, 1, &[9, 9]).unwrap();
        s.flush().unwrap();
    }
    // 重新打开：manifest 完好 → 不扫段也能读（断言读正确即证明）
    let s = Store::open(dir.path(), StoreConfig::default()).unwrap();
    assert_eq!(s.read(10, -10, 0).unwrap().unwrap(), vec![1, 2, 3]);
    assert!(s.read(11, -10, 0).unwrap().is_none()); // 位图负查询
}

#[test]
fn latest_write_wins_across_reopen() { /* 同 v1 */ }

#[test]
fn crash_between_write_and_flush_recovers_via_epoch() {
    // write → flush → write → drop（不 flush）→ 重开：open 时 manifest 无该记录，
    // Store::open 检测到 epoch 日志非空 → 自动回放补索引（两级：manifest + epoch）
    // 断言两条数据都可读
}

#[test]
fn corrupted_manifest_triggers_scan_rebuild() {
    // 删 manifest.vsm 与 .bak → open 返回 Err(Manifest) 或自动降级：
    // 契约：open 检测双副本坏 → 调 rebuild_index_from_scan → 数据全找回并 save 新 manifest
}
```

- [ ] **Step 2: 确认失败** — Run: `cargo test -p strata-core --test roundtrip`

- [ ] **Step 3: 实现 `store.rs`**
要点：
- `open` 顺序：load manifest → `Err(Corrupt)` 时自动 `rebuild_index_from_scan` + save；`Ok(None)` → 空 manifest。
- 索引页：每段一个内存增量缓冲（未落 `.vix` 的写入）+ 磁盘页合并视图；flush 时把增量并入磁盘页并写 `.vix`。
- `write` 字典：`dictionary=true` 且该 type 有字典槽 → comp_id 带槽位；无字典 → slot 0。
- `read` 命中 `payload_hash==0`（损坏标记）→ `Ok(None)`。

- [ ] **Step 4: 确认通过** — Run: `cargo test -p strata-core`，全部 PASS

- [ ] **Step 5: Commit**

```bash
git add crates/strata-core && git commit -m "feat(core): Store facade with three-tier index and no-scan startup"
```

---

### Task 11: 三档 GC（hole-punch / 删除 / 打分压实）

**Files:**
- Create: `crates/strata-core/src/gc.rs`
- Test: `crates/strata-core/tests/gc.rs`

**Interfaces:**
- Consumes: Store、punch、Index
- Produces:
```rust
pub struct GcConfig { pub invalid_threshold: f64, pub budget_bytes: u64, pub min_hole_bytes: u64 }
// 默认 0.6 / 32MiB / 64KB
pub struct GcStats { pub reclaimed_bytes: u64, pub segments_removed: u32,
                     pub holes_punched: u32, pub records_moved: u64 }
impl Store {
    pub fn gc_pass(&mut self, cfg: &GcConfig) -> Result<GcStats, StrataError>;
    pub fn touch_stats(&self) -> (u64 /*live*/, u64 /*total*/);
}
```
GC 语义（按段依次判定，预算节流）：
1. 段失效比例 ≥0.95 → 整段删除（含 `.vix`），索引 `evict`。
2. 段内连续死区间 ≥`min_hole_bytes` → `punch_hole`；返回 Unsupported → 该段转入第 3 档候选。
3. 打分 `score = 失效比例 × total_bytes / max(1, epoch - created_epoch)`，预算内选分最高的段压实：存活记录**原样搬迁**到新段（不重压），逐条记 epoch，旧段删，新段 bucket 继承。
- 分桶晋升：`flush` 时更新——Young 第二次 flush 起 → Active；连续 `stable_flush_gap`（10_000 gen）无重写 → Stable。

- [ ] **Step 1: 写失败测试** `tests/gc.rs`

```rust
#[test]
fn gc_compacts_dead_records_and_preserves_live() {
    // 小段（segment_max_bytes=4096）写 20 个 chunk → 覆盖 18 个 → flush
    // gc_pass(invalid_threshold=0.3)：reclaimed>0，live 不变，total 变小，20 条数据完整
}

#[test]
fn hole_punch_or_fallback_reclaims_sparse_dead_spans() {
    // 构造：大段写 100 条，删除中间连续 20 条（用新记录覆盖后使其失效且空间连续）
    // gc_pass 后：records_moved 少（稀疏回收不搬移）或 holes_punched>0；
    // 若平台 Unsupported 则允许 fallback 压实——断言 reclaimed>0 且数据完整
}

#[test]
fn nearly_dead_segment_fully_removed() {
    // 段内 99% 失效 → gc_pass → segments_removed>=1，文件不存在
}
```

- [ ] **Step 2: 确认失败** — Run: `cargo test -p strata-core --test gc`

- [ ] **Step 3: 实现 `gc.rs`**（live 统计：遍历索引按 seg_id 聚合；死区间图：压实扫描时顺手构建每段 valid bitset）

- [ ] **Step 4: 确认通过** — Run: `cargo test -p strata-core`，全部 PASS

- [ ] **Step 5: Commit**

```bash
git add crates/strata-core && git commit -m "feat(core): three-tier GC (hole punch / drop / scored compaction)"
```

---

### Task 12: 冷层分块固态归档（superfeatures + 字典）

**Files:**
- Create: `crates/strata-core/src/cold.rs`
- Test: `crates/strata-core/tests/cold.rs`

**Interfaces:**
- Consumes: codec、dict、Envelope
- Produces:
```rust
pub const COLD_BLOCK_CHUNKS: usize = 64;

pub struct ArchiveBuilder { /* 收集 region 记录，superfeatures 排序 */ }
impl ArchiveBuilder {
    pub fn new(region_x: i32, region_z: i32, level: i32, dict: Option<Vec<u8>>) -> Self;
    pub fn add(&mut self, env: Envelope, nbt: Vec<u8>);
    pub fn finish(self, path: &Path) -> Result<ArchiveSummary { blocks: u32, compressed_bytes: u64, plain_bytes: u64 }, StrataError>;
}
pub struct ArchiveReader { /* 块级读取 + SIEVE 块缓存钩子 */ }
impl ArchiveReader {
    pub fn open(path: &Path) -> Result<Self, StrataError>;
    pub fn get(&mut self, x: i32, z: i32, type_id: u16) -> Result<Option<Vec<u8>>, StrataError>; // 只解压目标块
    pub fn invalidate(&mut self, x: i32, z: i32, type_id: u16) -> Result<bool, StrataError>;    // .varc.inv 位图
    pub fn invalid_count(&self) -> u32;
    pub fn total_slots(&self) -> u32;
    pub fn extract_all(&mut self) -> Result<Vec<(Envelope, Vec<u8>)>, StrataError>;
    pub fn max_block_plain_bytes(&self) -> u64; // 冷读内存上界观测
}
```
布局：`"VARC"` + region_x/z + 块数 + 块表（每块：文件偏移 u64、plain_len u32、槽数 u16）+ 槽表（相对坐标 x:u16, z:u16, type u16, block u16, offset u32, len u32，按坐标排序）+ 压缩块序列。superfeatures：对每条 NBT 计算滚动哈希的 (min, max) 特征对，按 (min, max) 排序后再分块——相似负载相邻，zstd 收益接近聚类（替代 RubikFS 相似度图）。

- [ ] **Step 1: 写失败测试** `tests/cold.rs`

```rust
#[test]
fn archive_block_read_bound_and_invalidate() {
    // 1024 条记录（region 满）→ finish → 块数 == ceil(1024/64)=16
    // get(10,0,0) 返回正确 NBT；max_block_plain_bytes ≤ 64×最大单条
    // get(99,0,0) None；invalidate → invalid_count==1 → get 返回 None
    // extract_all 返回 1024 条且内容逐条匹配
}

#[test]
fn superfeatures_ordering_improves_compression() {
    // 两组同构样本：乱序 vs superfeatures 排序，各 build 归档
    // 断言排序版 compressed_bytes < 乱序版（同构数据下字典+局部性收益）
}
```

- [ ] **Step 2: 确认失败** — Run: `cargo test -p strata-core --test cold`

- [ ] **Step 3: 实现 `cold.rs`**
- `finish`：superfeatures 排序 → 按 64 条一块切分 → 每块内 `信封序列 + NBT` 拼接后 zstd（带字典可选）→ 写文件（先写槽表再写块，块偏移回填两次 pass 或先缓冲）。
- `get`：槽表二分（坐标+type）→ 块号 → 读块 → 解压到块缓存（`Vec<u8>`，容量由 Store 的 cache 预算管理，Phase 1 简化为单块最近缓存）→ 切片。

- [ ] **Step 4: 确认通过** — Run: `cargo test -p strata-core --test cold`，2 PASS

- [ ] **Step 5: Commit**

```bash
git add crates/strata-core && git commit -m "feat(core): blocked cold archive with superfeatures ordering"
```

---

### Task 13: 热↔冷迁移策略

**Files:**
- Create: `crates/strata-core/src/tier.rs`
- Test: `crates/strata-core/tests/tier.rs`

**Interfaces:**
- Consumes: Tasks 10/11/12
- Produces:
```rust
pub struct TierConfig { pub enabled: bool, pub stable_flushes: u32, pub invalid_demote_ratio: f64 }
// 默认 true / 30 / 0.25
pub struct TierStats { pub promoted: u32, pub demoted: u32, pub bytes_cold: u64 }
impl Store {
    pub fn tier_pass(&mut self, cfg: &TierConfig) -> Result<TierStats, StrataError>;
    // cfg.enabled=false → 直接返回空 stats（纯热模式）
    // 晋升：region 全部槽位在热索引/位图中均 Stable → ArchiveBuilder → fsync → 注册 ColdMeta + 位图/索引清理 → epoch 记录
    // 降级：invalid_count/total_slots > ratio → extract_all → 重写热层 → 删归档与 ColdMeta
}
```
`read` 路径扩展（热 miss）：查 ColdMeta → `ArchiveReader::get`。`write` 命中冷槽位：写热层 + invalidate + ColdMeta.invalid_count++。

- [ ] **Step 1: 写失败测试** `tests/tier.rs`（stable_region_promotes_to_cold_and_reads_back / rewrite_backfills_hot_and_counts_invalid / heavy_invalidation_demotes_archive / tiering_disabled_never_promotes 四例）

- [ ] **Step 2: 确认失败** — Run: `cargo test -p strata-core --test tier`

- [ ] **Step 3: 实现 `tier.rs`**（晋升事务性：先归档+fsync，再改 manifest；失败回滚删归档）

- [ ] **Step 4: 确认通过** — Run: `cargo test -p strata-core`，全部 PASS

- [ ] **Step 5: Commit**

```bash
git add crates/strata-core && git commit -m "feat(core): hot/cold tier migration with demotion"
```

---

### Task 14: Anvil 读写器（CLI 依赖）

**Files:**
- Create: `crates/strata-cli/src/anvil.rs`
- Test: 内嵌 `#[cfg(test)]`

**Interfaces:**
- Produces: `ChunkLoc { x: u8, z: u8, nbt: Vec<u8>, timestamp: u32 }`；`read_region(path) -> anyhow::Result<Vec<ChunkLoc>>`；`write_region(path, &[ChunkLoc])`（4KB 扇区 DEFLATE，外部 chunk |128 报 unsupported）。格式要点：8KB 头（1024×u32 BE 位置 + 1024×u32 BE 时间戳）；记录 = u32 BE 长 + 1B 版本 + 压缩 NBT。

- [ ] **Step 1: 写失败测试**（anvil_write_read_roundtrip + empty_slots_skipped，同 v1）

- [ ] **Step 2: 确认失败** — Run: `cargo test -p strata-cli anvil`

- [ ] **Step 3: 实现**（位置表分配从扇区 2 起；版本 1/2/3/4 = gzip/deflate/none/lz4 用 flate2/lz4_flex）

- [ ] **Step 4: 确认通过** — Run: `cargo test -p strata-cli`，2 PASS

- [ ] **Step 5: Commit**

```bash
git add crates/strata-cli && git commit -m "feat(cli): anvil .mca reader/writer"
```

---

### Task 15: strata.properties 配置加载器 + 组合矩阵

**Files:**
- Create: `crates/strata-cli/src/config.rs`
- Test: 内嵌 `#[cfg(test)]`

**Interfaces:**
- Consumes: strata-core `StoreConfig`/`GcConfig`/`TierConfig`
- Produces:
```rust
#[derive(Debug, Clone, PartialEq)]
pub struct StrataConfig {
    pub enabled: bool,               // 默认 false
    pub tier: TierConfig,
    pub store: StoreConfig,          // hot_level/hot_enabled/cold_level/cold_enabled/dictionary/cache_mb
    pub gc: GcConfig,
}
pub fn load_or_create_template(world_root: &Path) -> Result<StrataConfig, StrataError>;
// 无文件 → 写注释模板 + 返回默认；有文件 → 解析（'#'/'!' 注释、'\' 续行、首个 '=' 分割）
// 未知 key 告警忽略；非法值 → StrataError::Config{file, line, detail}
pub fn validate_matrix(cfg: &StrataConfig) -> Vec<String>; // 返回 WARN 列表（规格 §9 矩阵）
// enabled=false → 忽略其余；tiering=false → 忽略 cold 并提示；压缩全关 → WARN 体积；冷关热开 → WARN
```
模板（与规格 §9 完全一致，含注释）。级别校验：zstd 级别 -10..=22，0 非法。

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn missing_file_creates_template_with_disabled_default() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = load_or_create_template(dir.path()).unwrap();
    assert!(!cfg.enabled); // 默认关
    let text = std::fs::read_to_string(dir.path().join("strata.properties")).unwrap();
    assert!(text.contains("strata.enabled=false"));
}

#[test]
fn level_bounds_enforced() {
    for bad in ["zstd-0", "zstd-23", "zstd--11", "lz4"] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("strata.properties"),
            format!("strata.compression.hot={bad}\n")).unwrap();
        let e = load_or_create_template(dir.path()).unwrap_err();
        assert!(matches!(e, StrataError::Config { line: 1, .. }));
    }
}

#[test]
fn matrix_warns_on_all_compression_off() {
    let cfg = StrataConfig { enabled: true, store: StoreConfig { hot_enabled: false, .. }, .. };
    let warns = validate_matrix(&cfg);
    assert!(warns.iter().any(|w| w.contains("压缩全部关闭")));
}

#[test]
fn disabled_ignores_rest() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("strata.properties"),
        "strata.enabled=false\nstrata.compression.hot=not-a-level\n").unwrap();
    let cfg = load_or_create_template(dir.path()).unwrap(); // 不报错：enabled=false 忽略其余
    assert!(!cfg.enabled);
}
```

- [ ] **Step 2: 确认失败** — Run: `cargo test -p strata-cli config`

- [ ] **Step 3: 实现 `config.rs`**（解析器 + 映射层 + 矩阵）

- [ ] **Step 4: 确认通过** — Run: `cargo test -p strata-cli config`，4 PASS

- [ ] **Step 5: Commit**

```bash
git add crates/strata-cli && git commit -m "feat(cli): strata.properties loader with validation matrix"
```

---

### Task 16: strata-cli（Cesium 式转换/verify/compact/stats）

**Files:**
- Modify: `crates/strata-cli/src/main.rs`
- Create: `crates/strata-cli/tests/cli.rs`

**Interfaces:**
- Consumes: Store、anvil、config
- Produces: `strata_cli::run(&[&str]) -> anyhow::Result<()>`；子命令：
  - `convert --to-strata <world>` / `convert --to-anvil <world>`：Cesium 式覆盖、保留源、进度恢复（`vstore/.convert-progress`，每 region 一行 + fsync，完成删除）
  - `verify <world>` / `compact <world>`（gc_pass+tier_pass 直到收敛）/ `stats <world>`
- 行为契约：覆盖前先删 `vstore/`（to-strata）或逐 `.mca` 临时文件 rename（to-anvil）；结束打印"源格式文件已保留：<列表>，请验证后手动删除"；Phase 1 仅 overworld，遇 DIM-1 报暂不支持；配置经 `load_or_create_template`；`validate_matrix` 的 WARN 打到 stderr；type_id 映射 0=chunk 1=entities 2=poi。

- [ ] **Step 1: 写失败测试** `tests/cli.rs`

```rust
fn synth_anvil_world(world: &std::path::Path) {
    for dir in ["region", "entities", "poi"] { std::fs::create_dir_all(world.join(dir)).unwrap(); }
    let chunks: Vec<_> = (0..10).map(|i| ChunkLoc { x: i, z: 0, nbt: vec![i as u8; 200], timestamp: i as u32 }).collect();
    crate::anvil::write_region(&world.join("region/r.0.0.mca"), &chunks).unwrap();
    crate::anvil::write_region(&world.join("entities/r.0.0.mca"), &chunks[..3]).unwrap();
    crate::anvil::write_region(&world.join("poi/r.0.0.mca"), &chunks[..2]).unwrap();
}

#[test]
fn convert_to_strata_preserves_anvil_and_overwrites() { /* 同 v1：源文件保留 + vstore 生成 + 重复执行幂等 */ }

#[test]
fn convert_roundtrip_preserves_all_types() { /* 同 v1：删 region/ 再转回验证来自 vstore */ }

#[test]
fn interrupted_conversion_resumes() { /* 预置进度文件 → 已完成 region 跳过 → 最终清理 */ }

#[test]
fn convert_requires_enabled_config() {
    // 世界根 strata.properties 缺省（默认 enabled=false）→ convert 命令仍执行（转换不依赖 enabled，
    // 但结束时 WARN："strata.enabled=false，转换后记得在 strata.properties 中启用"）
}
```

- [ ] **Step 2: 确认失败** — Run: `cargo test -p strata-cli --test cli`

- [ ] **Step 3: 实现**（main.rs 用 clap derive；`lib.rs` 暴露 `run` 与 `pub mod anvil`）

- [ ] **Step 4: 确认通过** — Run: `cargo test`，全部 PASS

- [ ] **Step 5: Commit**

```bash
git add crates/strata-cli && git commit -m "feat(cli): Cesium-style in-place conversion preserving source format"
```

---

### Task 17: 基准（vs Anvil，含内存曲线）

**Files:**
- Create: `crates/strata-cli/benches/vs_anvil.rs`, `benches/RESULTS.md`

**Interfaces:**
- Consumes: 全部
- Produces: criterion 报告：
  1. **体积**：合成世界 4096 chunk → Anvil 字节 vs `convert --to-strata` + `compact` 后 `vstore/` 字节（目标 ≤0.65×）
  2. **写吞吐**：10k 次随机 write+flush
  3. **读延迟**：1k 次随机 read p50/p99（含冷读）
  4. **内存曲线**：`cache-mb=64` 下插入 10 万条目后 `SieveCache::len_bytes` 上界断言 + 进程 RSS 采样（Windows 用 GetProcessMemoryInfo）——验证与世界大小无关

- [ ] **Step 1: 写基准代码**（synth_world 生成 .mca；bench_footprint/bench_write_throughput/bench_read_latency/bench_memory_bound 四组；footprint 用 `convert --to-strata` 路径）

- [ ] **Step 2: 运行基准**

Run: `cargo bench -p strata-cli`
Expected: 体积比 ≤0.65×、无 panic、RSS 曲线平坦

- [ ] **Step 3: 结果记录到 `benches/RESULTS.md`**（机器信息、日期、四组数字）

- [ ] **Step 4: Commit**

```bash
git add crates/strata-cli/benches benches/RESULTS.md && git commit -m "bench: strata vs anvil footprint/throughput/latency/memory"
```

---

## Self-Review 记录（v2）

1. **规格覆盖**：信封 40B（§7）→ Task 2；comp_id 双语义+字典（§1/§6）→ Task 3；段写/扫描重同步（§5/§8）→ Tasks 4/5；三层索引+内存上界（§4）→ Task 6；hole-punch 跨平台（§5）→ Task 7；epoch（§8）→ Task 8；manifest 双副本+位图快照（§8）→ Task 9；Store+启动不扫段+三级恢复（§4/§8）→ Task 10；三档 GC+打分+分桶（§5）→ Task 11；分块冷归档+superfeatures（§6）→ Task 12；迁移+准冷+关闭冷层（§6/§9）→ Task 13；Anvil 读写（§10）→ Task 14；properties+矩阵+默认关+级别范围（§9）→ Task 15；Cesium 转换全契约（§10）→ Task 16；基准含内存曲线（§4/§12）→ Task 17。Folia shim/JNI/启动参数转换属 Phase 2（§11），本计划不覆盖。
2. **占位符扫描**：无 TBD/TODO；所有代码步骤含可运行代码或明确算法契约。
3. **类型一致性**：`Envelope`（9 字段 40B）、`IndexKey/IndexVal`、`comp_id` 位语义函数、`StoreConfig`（hot/cold 双开关 + cache_mb）、`TierConfig.enabled`、`GcConfig.min_hole_bytes`、`COLD_BLOCK_CHUNKS=64` 在各任务间一致。
