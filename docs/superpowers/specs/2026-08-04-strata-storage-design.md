# Strata —— Minecraft 服务端 Rust 混合双层存储引擎设计

**状态**: 设计 v2（架构重写，整合 2024–2026 文献与全部已确认需求）
**日期**: 2026-08-04（v2 重写）
**目标基线**: Canvas (Folia fork) 26.2+，后续 Paper / Arclight 适配
**形态**: 进程内 Rust 原生库（JNI），嵌入 JVM 服务端
**默认状态**: **关闭**（`strata.enabled=false`），需显式启用

---

## 1. 背景与动机

Minecraft 服务端默认 Anvil (`.mca`) 格式存在三类结构性问题：

1. **空间浪费**：4KB 扇区寻址，而典型 chunk 压缩后远小于 4KB，空间效率仅 ~74%（SectorFile 实测）。
2. **头部脆弱**：region 头部损坏即丢失全部定位信息，原版无恢复逻辑。
3. **写放大与同步 fsync**：就地更新 + 逐 chunk 同步写，IO 路径不友好。

本项目基于 2024–2026 存储/数据库/文件系统顶会成果，设计一套全新容器格式，目标：

- **体积最小**：混合双层（热层段日志 + 冷层分块固态归档），预期整体 45–55%。
- **内存有界**：存储层内存 **与世界大小无关**（原版 Paper 在 TB 级存档上的性质，严格保持）——这是支持 2b2t 级几十 TB 存档的硬前提。
- **性能最优**：顺序追加写 + 生命周期分组 + 三档 GC + 异步批量 IO。
- **长期兼容**：信封式记录，Rust 永不解析 NBT 内部，未来版本自动透传。
- **插件透明**：Bukkit/Paper API 层完全无感知。
- **崩溃安全**：与原版等价的 autosave 原子性 + epoch 回放恢复。

### 学术依据（2024–2026，含采纳/不采纳决定）

| 文献 | 会议/年份 | 结论 | 决定 |
|---|---|---|---|
| DOGI | FAST'26 | 生命周期感知数据放置，GC 写放大 ↓23% | **采纳**：热层按年龄/热度分桶 |
| DisCoGC | FAST'26（字节生产） | discard 式 GC 不搬移存活数据，TCO ↓20% | **采纳**：hole-punch 三档 GC |
| ArceKV / ElasticLSM | VLDB'26 | 打分制在线压实决策，动态负载快 2.17–2.92× | **采纳简化版**：打分选受害者（不做多层弹性结构） |
| SIEVE | NSDI'24 | FIFO 变体淘汰：命中率 ≥LRU、访问近乎免锁、O(1) 扫描 | **采纳**：索引页缓存淘汰（Folia 多线程友好） |
| S3-FIFO | SOSP'23 | 三 FIFO 队列达 LRU 级命中率 | 备选，SIEVE 更简单 |
| CARMI | VLDB'22 | 缓存感知学习索引，内存可调 | 理念采纳：`cache-mb` 可调上界 |
| Bourbon | OSDI'20 | LSM 学习索引内存 ↓3× | **不采纳**：region 占用位图对 MC 坐标键空间是 O(1) 精确查询，优于任何学习索引 |
| RocksDB partitioned index + block cache | 生产实践 | 索引落盘 + 有界缓存 → PB 级可跑 | **采纳**：三层索引模型 |
| SlimDB | CMU | 紧凑前缀共享索引 | 参考：索引页前缀压缩 |
| AutoCSF | arXiv 2026-03 | 偏斜负载内存最优索引 | **不采纳**：位图已精确且更省 |
| RubikFS | FAST'26 | 相似度聚类压缩 +42.6% | **采纳轻量版**：superfeatures sketch 排序（不建相似度图） |
| EROFS pcluster / zstd seekable | Linux 内核 2024 / 官方 | 块级压缩 + 块索引随机访问 | **采纳**：.varc 分块格式 |
| Ananke | FAST'25（Best Paper） | 自然事务边界崩溃恢复 | **采纳**：epoch 对齐 autosave |
| LavaStore | VLDB'24（字节生产） | 专用引擎 > 通用引擎；严格内存上界铁律 | 印证：不走 LMDB 路线 |
| Meterstick | ICPE'23 | MC 类负载高波动、尾延迟敏感 | 印证：GC 默认自适应节流 |
| F2FS node footer（源码实证） | FAST'15+ | 24B 自描述页脚 + checkpoint 版本链 | 印证：信封瘦身 52B→40B |
| RocksDB WAL 7B 头 | 生产 | 头不含键、恢复靠上层 | 对照：我们保留自描述（扫描重建需要） |
| sbk 2026 实测 | 社区基准 | zstd-9 48.8%、lzma2 45.6% 但慢 50× | 热 zstd-3 / 冷 zstd-9 的依据 |
| NTFS FSCTL_SET_ZERO_DATA | Microsoft 文档 | Windows 打洞等效：稀疏置零，粒度 64KB | **采纳**：GC 挖洞跨平台双实现 |

---

## 2. 关键决策（全部已与需求方确认）

| 决策项 | 选择 |
|---|---|
| 集成形态 | 进程内 JNI（Rust 原生库嵌入 JVM） |
| 容器形态 | 全新容器格式（非 Anvil 外壳改造） |
| 数据覆盖 | 全部世界持久化数据（region/entities/poi + playerdata/stats/advancements + savedata/地图等），不含服务端配置文件 |
| 一致性模型 | 原版等价 + 原子记录（epoch 对齐 autosave） |
| 架构方案 | 混合双层：热层段日志 + 冷层分块固态归档 |
| 目标基线 | Canvas (Folia) 26.2+ 优先 |
| 默认启用 | **false**（`strata.enabled=false`，显式启用） |
| 转换行为 | Cesium 式：启动参数/CLI 触发、原地覆盖、**源格式保留需手动删除** |
| 配置载体 | 世界根目录 `strata.properties`（Java properties 格式） |
| 内存模型 | 有界：常驻 ~几十 MB + `cache-mb` 可配缓存，与世界大小无关 |

### 存储引擎形态说明

Strata 的热层段日志**就是存储引擎本体**（自带索引/GC/崩溃恢复，等价于专用数据库）。不引入通用嵌入式 KV（LMDB/RocksDB/redb）——通用引擎的 B+ 树 CoW 写放大、单写者事务、冷热混排均不适配 MC chunk 负载（大 blob、空间聚集、region 对齐）。不存在"关闭两层只用数据库"的状态：关引擎即 `strata.enabled=false` 回 Anvil。

---

## 3. 总体架构

```mermaid
graph TB
    subgraph JVM["JVM（Canvas/Folia 26.2+）"]
        MC["Moonrise chunk 系统 + Folia regionizer"]
        SHIM["集成层 Java 插件（RegionStorage SPI hook）"]
        MC --> SHIM
    end
    subgraph NATIVE["Rust 原生库（进程内）"]
        FFI["FFI 边界（C ABI + catch_unwind）"]
        HOT["热层：段日志引擎 + 三层索引 + 三档 GC"]
        COLD["冷层：分块固态归档 .varc"]
        RECOV["恢复/校验（三级）"]
        FFI --> HOT
        HOT --> COLD
        FFI --> RECOV
    end
    subgraph TOOLING["工具链"]
        CLI["strata-cli：Cesium 式转换 / verify / compact / stats"]
    end
    SHIM -- "JNI 零拷贝 NBT blob" --> FFI
    CLI --> FFI
```

### 代码组织（cargo workspace）

| crate | 职责 |
|---|---|
| `strata-core` | 容器格式、段引擎、三层索引、GC、冷归档、恢复——纯 Rust，零 JVM 依赖 |
| `strata-ffi` | C ABI + JNI 桥，所有跨边界调用 `catch_unwind` 包裹（Phase 2） |
| `strata-cli` | Cesium 式双向转换器、`verify`、`compact`、`stats`、`strata.properties` 加载器 |
| `strata-plugin-canvas`（Java） | Canvas/Folia 集成 shim（Phase 2） |

### 磁盘布局（每 dimension 一个存储池）

```
world/dimensions/minecraft/overworld/
├─ vstore/
│  ├─ manifest.vsm (+ .bak)      # 影子双副本：格式版本/epoch/段表/冷索引/region 位图
│  ├─ segments/seg-0001.vseg     # 热层追加段（regionizer 分区对齐分片）
│  ├─ segments/seg-0001.vix      # 每段一个磁盘索引页（排序数组 + 前缀压缩）
│  ├─ cold/r.X.Z.varc            # 冷层分块归档（region 对齐，1024 chunk/个）
│  ├─ epoch/current.velog        # 当前 autosave 周期日志
│  ├─ dict/                      # per-type zstd 字典（字典槽 0–15）
│  └─ .convert-progress          # 转换进度（仅转换期间存在）
├─ level.dat / data/ ...         # 未纳入部分保持原样
```

### 长期兼容承诺

1. **信封式记录**：记录外壳与 NBT 负载解耦，Rust 永不解析 NBT 内部 → 未来任意版本 NBT 结构变化自动透传。
2. **未知类型透传**：manifest 与信封保留未知 type id，未来新数据类型不破坏旧引擎。
3. **混存自由**：每条记录自带 codec/字典槽/代际 → 任意时刻改配置，新旧记录共存，读取永远不依赖当前配置。

---

## 4. 内存模型（支持 TB 级存档的核心）

**原则：内存占用取决于活跃区域与配置上界，与世界总大小无关**（与原版 Paper 同性质）。

### 三层索引

```
┌─ L0 常驻（~几十 MB，与世界大小弱相关）────────────────────┐
│  段表：每段 ID/文件/桶/打分统计（KB/段）                     │
│  每段 region 占用位图：128B/region/type                     │
│    → 未生成 chunk 负查询 O(1) 位操作，零磁盘 IO（MC 领域特化，│
│      优于任何布隆/学习过滤器：位图精确且 1 bit/槽）           │
├─ L1 有界缓存（strata.index.cache-mb，默认 512）─────────────┤
│  热点索引页 —— SIEVE 淘汰（访问只置位、淘汰手扫描，           │
│  Folia 多线程近乎免锁；NSDI'24 命中率 ≥LRU）                │
├─ L2 磁盘（无界）─────────────────────────────────────────────┤
│  每段一个 .vix 索引页文件：排序数组 + 键前缀压缩（SlimDB 式）  │
│  GC 压实时随段重写；崩溃后按需从信封扫描重建                   │
└──────────────────────────────────────────────────────────────┘
```

**读路径**：位图判存在（不存在直接返回，零 IO）→ L1 查 → miss 读磁盘索引页（1–2 次 IO，与原版打开 region 读 8KB 头同量级）→ 段文件取记录。

**内存上界测算**：

| 存档规模 | 常驻 | 缓存上界（可配） | 合计上界 |
|---|---|---|---|
| 10 GB 小服 | ~5 MB | 512 MB | ~520 MB |
| 10 TB（2b2t 级，~5 万 region×3 类型） | ~25 MB（位图 19 MB + 段表） | 512 MB | **~540 MB** |

**启动**：只读 manifest + 位图，O(段数) 秒开，**不扫描段文件**；全扫描降级为恢复/`verify` 专用路径。

---

## 5. 热层段引擎

### 写入路径（Folia 并发安全）

```
chunk 脏数据 (NBT blob)
   │ Moonrise IO 线程提交（regionizer 分区）
   ▼
按分区哈希 → 选段分片（shard-per-region，无锁追加）
   ▼
压缩（当前热层 codec/级别/字典槽）→ 分配 gen
   ▼
追加段尾（顺序写）+ 记 epoch 日志 + 更新内存索引
   ▼
autosave 时机 → fsync 段 → manifest.epoch++ 并保存 → epoch rotate（原子）
```

- **段滚动**：当前段超过 `segment-max-bytes`（默认 64 MiB）滚动新段。
- **代际戳 gen**：单调递增；读路径只认最新代，GC 时旧代自然失效。
- **epoch 边界 = autosave/flush 时机**：与原版保存节奏完全一致，崩溃最多丢一个 autosave 周期。

### 生命周期分桶（DOGI）

| 桶 | 判据 | 放置 |
|---|---|---|
| Young | 首次写入、无旧代 | 独立年轻段（失效率最高，最先回收） |
| Active | 近期多次改写 | 混合段 |
| Stable | 长期未改写（`stable-flushes` 周期） | 冷层晋升候选 |

### 三档 GC（DisCoGC 启发 + ArceKV 打分）

按失效分布自动选择，预算（`gc.budget-bytes`）节流，tick 空闲期运行：

1. **hole-punch 挖洞**：稀疏失效区域直接回收，不搬移数据、零写放大。跨平台：Linux `fallocate(PUNCH_HOLE)` / Windows `FSCTL_SET_ZERO_DATA`（稀疏置零，NTFS 粒度 64KB）→ 最小洞尺寸阈值 64KB，小于阈值不挖。
2. **整段删除**：段失效比例 ≥95% → 直接删文件。
3. **压实重写**：打分选受害者 `score = 失效比例 × 段大小 / 段年龄`，预算内选分最高；存活记录原样搬迁（不重压——GC 不做重压缩）。

Meterstick 证实 MC 负载高波动 → GC 默认按前台写压力自适应节流。

---

## 6. 冷层分块固态归档（.varc）

**触发**：region 对齐块（32×32=1024 chunk）全部类型均 Stable（连续 `stable-flushes` 个周期未改写）→ 晋升候选。

**构建**：提取该 region 全部记录 → **superfeatures sketch 排序**（去重文献的轻量相似度特征：对每条负载计算滚动哈希 min/max 特征值对，按特征排序使相似 chunk 相邻——替代 RubikFS 的重量级相似度图，O(1)/条、不解析负载）→ **按 64 chunk 一块分块压缩**（EROFS pcluster 式：每块独立 zstd，可选 per-type 字典）→ 写只读 `.varc`：头 + 块索引表（块号→文件偏移/解压后范围）+ 槽位索引（坐标+type→块内偏移）。

**冷读**：命中 → 读块索引 → **只解压目标块**（~64 chunk 的压缩块，内存上界 ~1–2 MB）→ 块级解压缓存（SIEVE，共享 cache-mb 预算）→ 返回单条。

**回读回填**：玩家走回冷区 → 冷读透明返回；改写该 chunk → 写热层 + 归档失效位图（`.varc.inv` 旁路文件）+1；失效比例超 `invalid-demote-ratio`（默认 0.25）→ 整归档降级回热层。

**体积**：冷数据 zstd-9 + 字典 + 排序收益 ~40%；热 zstd-3 ~60%；混合整体 45–55%（Anvil ~75%）。

---

## 7. 记录信封（40 字节定长头）

```
偏移 0   [ magic 4B = "VSEG" ][ record_ver 1B ][ type_id 2B ][ comp_id 1B ]
偏移 8   [ chunk_x 4B ][ chunk_z 4B ]
偏移 16  [ gen 8B ][ epoch_ts 4B ]
偏移 28  [ payload_len 4B ][ xxhash64_payload 8B ]
偏移 40  [ ... 压缩后的 NBT 负载（payload_len 字节）... ]
```

所有整数小端。相对早期 52B 版的瘦身依据（F2FS/RocksDB/SectorFile 文献对照）：

- 删 `dim_hash`（-4B）：存储池按维度分目录，维度在路径中。
- `timestamp` 8B→4B（`epoch_ts`，u32 epoch 计数，F2FS `cp_ver` 同款）：墙钟时间由索引页承载，不入信封。
- `comp_id` 1B 双语义：低 4 位 codec（0=NONE 1=ZSTD），高 4 位字典槽（0=无字典，16 槽/维度）。
- 保留 `magic`：头部全毁时的重同步扫描锚点（SectorFile 32B 头没有，靠它我们的三级恢复第 2 级更强）。
- 保留 `gen`：最新代语义权威来源。
- 保留 `xxhash`：逐条校验 + 扫描重建验证（ZFS 式父指针存子校验会破坏单条独立可验证性，不采纳）。

小记录开销对比：poi（~500B 压缩后）元数据占比 52B 时 10.4% → 40B 时 8.0%；chunk（~30KB）<0.15%，忽略。

---

## 8. Manifest、恢复与崩溃一致性

### Manifest（manifest.vsm，影子双副本）

记录：格式版本、当前 epoch、next_gen/next_seg_id、段表（ID/文件/桶/live/total/打分统计）、冷索引（region→varc/失效计数）、region 占用位图快照。

- 双副本 + 原子 rename 切换；损坏时取较新且校验通过的副本。
- **不用 RocksDB 式日志 MANIFEST 的理由**：manifest 变更频率低（flush/GC/tier 时），日志式的增量回放收益为零，快照双副本更简单且正确性等同。

### 恢复（三级）

1. **epoch 回放**：崩溃后读 `current.velog` 重建索引——恢复最近 autosave 之后的数据。
2. **manifest 双副本全坏** → 扫描段文件信封重建段表与索引（每条信封自带坐标/类型/gen，magic 用于重同步定位）——此即"启动不扫段、恢复才扫段"的恢复路径。
3. **单条记录 hash 不匹配** → 仅该记录标记损坏并告警（索引条目 payload_hash=0），不传播。

`Store::verify` 提供全量体检报告（记录数 + 损坏清单）。

---

## 9. 配置（strata.properties）

世界根目录（与 level.dat 同级），Java properties 格式（`key=value`，`#` 注释）。CLI 与服务端 shim 共用同一加载器。**启动时加载**（不做热重载——存储参数中途切换易生歧义）；缺失文件 → 生成带注释模板 + 全默认。

```properties
# 总开关（默认 false——必须显式启用）
strata.enabled=false
# 冷层（轴 1：容器分层）
strata.tiering.enabled=true
strata.tiering.stable-flushes=30
strata.tiering.invalid-demote-ratio=0.25
# 压缩（轴 2：与轴 1 正交）
strata.compression.hot-enabled=true
strata.compression.cold-enabled=true
strata.compression.hot=zstd-3
strata.compression.cold=zstd-9
strata.compression.dictionary=true
# 批量压缩工作线程：0=自动(全核) 1=串行(默认,TPS优先) N≥2=限N线程
strata.compression.threads=1
# 索引内存上界
strata.index.cache-mb=512
# GC
strata.gc.enabled=true
strata.gc.invalid-threshold=0.6
strata.gc.budget-bytes=33554432
```

**级别范围**：zstd -10 ~ 22（0 非法——"库默认"语义歧义；越界报行号错误）。
**批量压缩并行策略**：`write_batch` 的压缩默认**串行**（`threads=1`），因游戏服 CPU 稀缺、TPS 优先于压缩吞吐；显式 `threads=0`（全部可用核）或 `N≥2` 时，用**有界 `std::thread::scope`** 派生 worker 分块压缩——不用 rayon 全局池（全局单例无法按 Store 限流，且会与游戏线程抢核）。结果与输入同序，任一 worker 出错即整批失败（追加尚未发生）。
**非法值** → 启动报错并指明 `strata.properties:<行号>: <原因>`，不静默回退。

### 组合合法性矩阵

| enabled | tiering | 热压缩 | 冷压缩 | 行为 |
|---|---|---|---|---|
| false | * | * | * | 回退 Anvil，忽略其余全部配置 |
| true | false | off | off | 合法 + WARN（全关退化：纯段日志零压缩，体积≈Anvil none） |
| true | false | on | * | 纯热模式，冷配置忽略并提示 |
| true | true | off | on | 合法（热 NONE，晋升时才压） |
| true | true | on | off | 合法 + WARN（冷不压反而更大） |

语义要点：
- **只开热关冷**：退化为纯段日志，GC/索引/恢复照常。
- **只开冷关热**：架构不可能——冷层只读，写入必须经热层基底；该组合归约到 enabled=false。
- **准冷模式**：`stable-flushes=1` → 稳定 region 尽快晋升，热层只剩活跃写缓冲。
- 关闭压缩不损失引擎骨架收益（自愈/原子性/有界内存/GC 照常），只损失体积。

### 配置变更语义

- 压缩级别/codec/字典：**随时可改**，只影响之后的写入；旧记录按自身 comp_id 读取（混存合法）。读取永不回写 → 路过旧区域不触发重压；只有"改动后保存 / 冷层晋升降级 / 显式转换"三条路径用新配置。
- GC/tiering 参数：运行时调优，影响后续轮次。
- `enabled=false` 前提：先跑 `--strataConvertToAnvil` 转回 Anvil，否则加载到旧快照（转换器章节说明）。

---

## 10. 迁移与转换器（Cesium 式行为）

- **触发**：服务端启动参数 `--strataConvertToStrata` / `--strataConvertToAnvil`（Phase 2 shim 解析）；CLI 等价命令 `strata-cli convert --to-strata <world>` / `--to-anvil <world>`。转换在启动前同步执行，完成后继续启动。
- **原地覆盖**：输出写到同一世界目录目标存储；目标已存在**直接覆盖重建**（与 Cesium wiki 一致）。重复执行同方向转换 = 重新生成；转换后应移除启动参数，否则下次启动再次覆盖。
- **保留源格式**：转换**绝不删除**源（`region/`/`entities/`/`poi/` 或 `vstore/`），运维验证后**手动删除**；结束时打印醒目提示与源目录列表。
- **可恢复进度**：每 region 完成追加 `vstore/.convert-progress` 一行 + fsync；中断重跑跳过已完成；全部完成后删除进度文件。
- **全量 + 当前配置**：遍历所有 region/entities/poi `.mca`，按当前 `strata.properties` 压缩级别写 vstore；反向转 Anvil 固定 DEFLATE（Anvil 不支持 zstd）。中断期间改配置会造成混级别（合法但不一致）。
- **多维度 + 多世界**：CLI 与服务端均按维度根遍历——overworld（世界根本身）、vanilla 布局 `DIM-1`/`DIM1`、Canvas/Paper 布局 `dimensions/minecraft/<dim>`；每个维度根一个独立 vstore（`<dimroot>/vstore`）。多世界插件（Multiverse 等）创建的世界是普通世界根，逐世界执行即全兼容。

---

## 11. FFI/JNI 集成与 Folia 兼容（Phase 2，接口约定）

- **零拷贝**：NBT blob 经 DirectByteBuffer 传递，Rust 侧拿 `&[u8]` 直接压缩。
- **每个跨边界调用 `catch_unwind`**：Rust panic → Java 异常 + 日志，绝不炸 JVM。
- shim 挂在 Moonrise chunk IO 线程之下（regionizer 下游），不触碰 tick 线程模型；段分片按 regionizer 分区哈希，写入天然无锁。
- 已核实：Spottedleaf SectorFile 实验开过 Folia 分支，存储层替换在 Folia 可行。
- 插件透明：Bukkit/Paper API 无感知；外部 `.mca` 工具不可直读新格式，CLI 双向转换兜底。

---

## 12. 测试策略与风险

### 测试

- `strata-core` 单元 + property-based fuzz（proptest；信封编解码、段追加/扫描、索引、GC、恢复）。
- 崩溃注入：随机在写入/fsync/epoch 切换点 kill，验证三级恢复。
- 基准（criterion）：vs Anvil 体积（目标 ≤0.65×）/写吞吐/读延迟 p50·p99；合成世界 4096 chunk。
- 集成（Phase 2）：Canvas 实服跑图 + 插件兼容性矩阵（Multiverse 等）。

### 风险与缓解

| 风险 | 缓解 |
|---|---|
| Rust panic 炸服务器 | catch_unwind 全覆盖 + CI fuzz |
| 冷区回读延迟抖动 | 分块解压上界 1–2 MB + 块缓存；zstd 解压 ~50µs/chunk |
| Canvas 26.2 API 漂移 | 集成层薄 shim + 版本探测；core 与 JVM 解耦 |
| Windows hole-punch 语义差异 | FSCTL_SET_ZERO_DATA 双实现 + 64KB 最小洞阈值；基准覆盖两平台 |
| 体积收益不达预期 | 字典 + superfeatures 排序已是最优组合；可回退热层级别 |
| 双格式并存复杂度 | CLI 双向转换 + 源格式保留回滚 + 明确弃用路径 |
| TB 级内存失控 | 三层索引 + 启动不扫段 + 基准用 10TB 合成负载验证内存曲线平坦 |

---

## 13. 演进路线（明确不做，留给未来）

- 学习索引（Bourbon/CARMI 完整版）：键空间泛化时再评估
- per-type 之外的多字典策略 / lzma2 冷层 codec（注册表已留扩展位）
- `strata-cli recompress <world>` 全量重压维护命令
- ~~Nether/End 维度转换~~ ✅ 已实现（多维度遍历 + 每维度独立 vstore）；多世界（插件创建的世界）同步支持
- playerdata/stats/advancements/savedata 纳入（Phase 1 仅三大件，信封 type_id 已预留）
