# 服务端支持状态

Strata 对 Minecraft 服务端各端的支持状态一览。

| 服务端 | 状态 | 描述 |
| --- | --- | --- |
| strata-core 存储引擎（Rust） | ✅ 已完成 | 段日志热层 + 分块冷归档 + 三层索引 + 三档 GC，CI 双平台全绿（Windows + Linux） |
| strata-cli 转换器 | ✅ 已完成 | Cesium 式双向转换（覆盖目标、保留源、进度恢复、**多维度遍历**）、verify/compact/stats/**recompress** |
| strata-ffi C ABI | ✅ 已完成 | Rust cdylib/staticlib，catch_unwind 全函数，CI 双平台全绿（Linux + Windows 交叉编译 DLL） |
| JNI 桥接层 | ✅ 已完成 | `Java_dev_strata_bridge_StrataNative_*` 符号层（手写 JNI FFI，零依赖）。**Windows 实服烟雾验证通过**：native bridge 加载、**三维度各自路由 vstore**、硬杀崩溃恢复、RCON 优雅停机、二次启动读回全绿 |
| 多世界 / 多维度 | ✅ 已完成 | 主世界/下界/末地各自独立 `<维度目录>/vstore`；Multiverse 等插件创建的世界各自读自己的 `strata.properties` 自动接管 |
| Canvas（自魔改 fork） | ✅ 已完成 | **主集成目标**：[wosnxn123/Canvas](https://github.com/wosnxn123/Canvas)。weaver feature patch（`0004-Strata-Storage` + paper `0001`）已合入 fork main 并推送；applyAllPatches + compileJava + createPaperclipJar 全绿，实服烟雾通过；`/strata` 命令、启动转换钩子、压缩线程开关齐备；**fail-closed 启动**（vstore 存在但 Strata 未接管时拒绝启动该 level，`strata.force-anvil` 应急逃生）、**转换守卫**（目标已有 vstore 默认拒绝，需 `--strataForce`/`-f`）、**在线 GC/tier**（随 autosave 周期，每 `stable-flushes` 次 flush 一轮）、vstore **会话锁**（`.strata.lock`） |
| Paper / Folia（上游） | 📋 计划中 | 提供**源代码 + 构建嵌入教程**，供自行 patch 嵌入；运行期效果与 Canvas fork 一致 |
| Fabric / NeoForge 模组 | 📋 计划中 | 远期模组端适配：复用 vanilla IO 适配层，对标 Cesium 形态 |
| 其他插件端（Spigot/Arclight 等） | 📋 计划中 | 提供**自构建 + 源代码嵌入构建教程**；Arclight 走 vanilla IO 路径（不支持 Folia，与 modloader 互斥） |
| 外部 .mca 编辑工具（Amulet/MCA Editor） | ❌ 不支持 | 新格式非 Anvil 外壳；经 `strata-cli convert --to-anvil` 转回兜底 |
| 原版单人/客户端 | 📋 计划中 | 内置服务端同一套 IO 路径，远期目标 |

## 集成形态说明

Strata 的存储后端替换**无法**通过纯插件 API 实现（Paper 未公开存储后端替换接口，`RegionFileVersion` 仅能换压缩算法、仍受 Anvil 4KB 容器限制）。因此采用**源码级 patch 内嵌**形态（钩在 Moonrise `DataController` 字节边界 + `ServerLevel` per-level store 解析）：

- **Canvas（用户 fork）**：以 weaver feature patch 内嵌进 fork 构建，随 fork 既有补丁一同 rebase，升级维护成本最低。**已合入**。
- **Paper/Folia 上游**：提供源代码与构建嵌入教程（`BUILD_GUIDE.md`），供维护者自行 patch；Canvas 无 Mixin 运行时，故不走 Mixin 形态。
- **模组端**：远期目标，复用同一 vanilla IO 适配层。

## 运维边界

- **仅本地文件系统**：vstore 有独占会话锁（`vstore/.strata.lock`，flock/LockFileEx）；NFS、网络文件系统、多机共享卷**不支持**（锁与 rename/fsync/挖洞语义依赖本地文件系统）。
- **CLI 需停服**：对运行中服务的 vstore 跑 CLI 会报"另一个进程正在使用"；`compact`/`recompress`/`convert` 均离线执行。在线 GC/tier 随 autosave 周期自动运行（每 `stable-flushes` 次成功 flush 一轮），启动不做全量扫描。
- **备份与稀疏文件**：挖洞产生稀疏文件，不感知稀疏的备份工具会膨胀回逻辑大小——用 `tar --sparse` 或支持稀疏的工具，或直接复制段文件。
- **防病毒**：建议将 vstore 目录排除在实时扫描之外，避免锁竞争与延迟抖动。

## 状态含义

- ✅ **已完成**：代码合入且 CI/实机验证通过。
- 🚧 **开发中**：进行中。
- 📋 **计划中**：设计已定（见设计文档），实施待排期。
- ❌ **不支持**：架构性不支持并给出兜底方案。

## 参考

- [设计文档：Strata 混合双层存储引擎](superpowers/specs/2026-08-04-strata-storage-design.md)
- [基准测试结果](../benches/RESULTS.md)
