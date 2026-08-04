# 服务端支持状态

Strata 对 Minecraft 服务端各端的支持状态一览。

| 服务端 | 状态 | 描述 |
| --- | --- | --- |
| strata-core 存储引擎（Rust） | ✅ 已完成 | 段日志热层 + 分块冷归档 + 三层索引 + 三档 GC，CI 双平台全绿（Windows + Linux） |
| strata-cli 转换器 | ✅ 已完成 | Cesium 式双向转换（覆盖目标、保留源、进度恢复）、verify/compact/stats |
| Java FFI 插件（JNI 桥） | 🚧 开发中 | Phase 2：strata-ffi C ABI + SyncStore 线程安全门面 |
| Canvas（自魔改 fork） | 🚧 开发中 | **主集成目标**：[wosnxn123/Canvas](https://github.com/wosnxn123/Canvas)。以内嵌 patch 形式嵌入 fork 构建（与命令方块修复等补丁同一 rebase 流程），Mixin hook Moonrise IO 字节边界 + regionizer 分区多 Store 无锁并发写 |
| Paper / Folia（上游） | 📋 计划中 | 提供**源代码 + 构建嵌入教程**，供自行 patch 嵌入；运行期效果与 Canvas fork 一致 |
| Fabric / NeoForge 模组 | 📋 计划中 | 远期模组端适配：复用 vanilla IO 适配层，对标 Cesium 形态 |
| 其他插件端（Spigot/Arclight 等） | 📋 计划中 | 提供**自构建 + 源代码嵌入构建教程**；Arclight 走 vanilla IO 路径（不支持 Folia，与 modloader 互斥） |
| 外部 .mca 编辑工具（Amulet/MCA Editor） | ❌ 不支持 | 新格式非 Anvil 外壳；经 `strata-cli convert --to-anvil` 转回兜底 |
| 原版单人/客户端 | 📋 计划中 | 内置服务端同一套 IO 路径，远期目标 |

## 集成形态说明

Strata 的存储后端替换**无法**通过纯插件 API 实现（Paper 未公开存储后端替换接口，`RegionFileVersion` 仅能换压缩算法、仍受 Anvil 4KB 容器限制）。因此采用**字节边界 Mixin hook + 内嵌 fork patch** 形态：

- **Canvas（用户 fork）**：以 patch 形式内嵌进 fork 构建，随 fork 既有补丁一同 rebase，升级维护成本最低。
- **Paper/Folia 上游**：提供源代码与构建嵌入教程，供维护者自行 patch。
- **模组端**：远期目标，复用同一 vanilla IO 适配层。

## 状态含义

- ✅ **已完成**：代码合入且 CI/实机验证通过。
- 🚧 **开发中**：进行中。
- 📋 **计划中**：设计已定（见设计文档），实施待排期。
- ❌ **不支持**：架构性不支持并给出兜底方案。

## 参考

- [设计文档：Strata 混合双层存储引擎](superpowers/specs/2026-08-04-strata-storage-design.md)
- [基准测试结果](../benches/RESULTS.md)
