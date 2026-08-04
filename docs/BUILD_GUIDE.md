# Strata 构建与嵌入指南

面向**其他插件端 / 自构建用户**：从源码构建 Strata native 库与 Java 桥 jar，并把它嵌入你自己的 Minecraft 服务端（Canvas fork / Paper 上游 / 模组端）。

桥 jar（`java-bridge`）本身只是薄 JNI 层：`dev.strata.bridge.StrataNative` 声明 native 方法、做错误码→异常映射，并把 `strata-ffi` 共享库从 jar 资源解出加载。存储引擎本体全部在 native 库（Rust）里，Java 侧零依赖。

---

## 1. 前置条件

| 工具 | 版本要求 | 用途 |
| --- | --- | --- |
| Rust | stable（建议最新 stable） | 构建 `strata-ffi` native 库 |
| JDK | 21+ | 构建桥 jar（Gradle toolchain 固定 21）；Canvas 主线为 Java 25，桥库按 21 字节码编译，兼容 |
| Gradle | 8+ | `cd java-bridge && gradle jar` |
| Git | 任意 | 拉取源码 |

Windows 构建 native 库需要 MSVC 工具链（`x86_64-pc-windows-msvc` 默认 target）；Linux 需要常规 gcc/clang 链接器。

---

## 2. 构建 Rust native 库

在仓库根目录：

```bash
cargo build --release -p strata-ffi --features jni
```

> ⚠️ **必须带 `--features jni`**：不带该 feature 的产物只有 C ABI（`strata_*`），不导出 `Java_dev_strata_bridge_StrataNative_*` JNI 符号，Java 侧 `load()` 后调用 native 方法会 `UnsatisfiedLinkError` 并回退 Anvil。

产物位置与文件名（**注意各平台 cargo 输出名不同**）：

| 平台 | cargo 产物 |
| --- | --- |
| Linux x86_64 | `target/release/libstrata_ffi.so` |
| Windows x86_64 | `target/release/strata_ffi.dll` |
| macOS aarch64 | `target/release/libstrata_ffi.dylib` |

交叉编译（在本机装好对应 target 与链接器后）：

```bash
# Linux x86_64（target 通常即本机）
rustup target add x86_64-unknown-linux-gnu
cargo build --release --features jni --target x86_64-unknown-linux-gnu -p strata-ffi

# Windows x86_64（Windows 本机构建，MSVC）
rustup target add x86_64-pc-windows-msvc
cargo build --release --features jni --target x86_64-pc-windows-msvc -p strata-ffi

# Windows x86_64（Linux 交叉编译，已验证路线：CNB/CI 上用）
sudo apt-get install -y gcc-mingw-w64-x86-64
rustup target add x86_64-pc-windows-gnu
cargo build --release --features jni --target x86_64-pc-windows-gnu -p strata-ffi

# macOS Apple Silicon
rustup target add aarch64-apple-darwin
cargo build --release --features jni --target aarch64-apple-darwin -p strata-ffi
```

交叉产物在 `target/<target>/release/` 下，文件名同上表。

> CI 参考实现：`.github/workflows/ci.yml` 的 `java-bridge` job 在 ubuntu-latest 上执行 `cargo build --release -p strata-ffi` 并把 `libstrata_ffi.so` 复制进桥 jar 后上传 artifact，可直接下载复用，跳过本地编译。

---

## 3. 放置 native 库

把构建产物复制到 `java-bridge/src/main/resources/natives/`，**文件名必须与 `StrataNative.load()` 的探测名逐字一致**：

| 平台 | jar 内资源名（必须精确） | 来源文件 |
| --- | --- | --- |
| Linux amd64 | `natives/strata_ffi.so` | `target/release/libstrata_ffi.so`（去掉 `lib` 前缀） |
| Windows amd64 | `natives/strata_ffi.dll` | `target/release/strata_ffi.dll` |
| macOS aarch64 | `natives/libstrata_ffi.dylib` | `target/release/libstrata_ffi.dylib` |

示例（Linux）：

```bash
cp target/release/libstrata_ffi.so java-bridge/src/main/resources/natives/strata_ffi.so
```

只放你目标平台的文件即可；`load()` 按当前 JVM 的 `os.name`/`os.arch` 只探测自己那一个。仓库内该目录默认只有 `.gitkeep` 占位，由 CI（或你）在打 jar 前填充。

---

## 4. 构建桥 jar

```bash
cd java-bridge
gradle jar
```

产物：`java-bridge/build/libs/java-bridge.jar`（含 `dev.strata.bridge.*` 类与第 3 步放入的 native 资源）。无外部依赖，直接可嵌入。

---

## 5. 嵌入服务端

### 5.1 Canvas / Folia fork（推荐：source patch）

1. 把 `java-bridge.jar` 加入 server classpath。paperweight/Gradle 构建里任选其一：
   - buildscript 依赖：`additionalRuntimeClasspath("dev.strata:java-bridge:0.1.0")`（或把 jar 放进 `libs/` 后 `runtimeOnly(fileTree("libs"))`）；
   - 或直接把 jar 放进服务器运行目录的 `libraries/` 目录（Paper 启动器会扫描 `libraries/` 下按 maven 布局组织的 jar）。
2. 在 fork 的源码 patch 中直接调用 `dev.strata.bridge.StrataNative`（参考 `canvas-patch/` 下的成品集成）：
   - 每个 ServerLevel 构造期解析**自己的**世界根与维度目录（`LevelStorageAccess.getLevelPath(ROOT)` / `getDimensionPath(dimension)`），对维度目录调一次 `StrataNative.open(dimDir + "/vstore", ..., compressionThreads)` 拿 handle——主世界/下界/末地/插件创建的世界各自独立 store；
   - 在 Moonrise chunk IO 的字节边界 hook 点（`ChunkDataController`/`EntityDataController`/`PoiDataController` 的 `startWrite`/`readData`）把 NBT blob 交给 `StrataNative.write(handle, x, z, typeId, nbt)`；写成功返回 DELETE 结果清除旧 Anvil 副本，读取 `null`（无记录）回退 Anvil 读；
   - 定期/关服时对全部打开的 store `flush` / `gc` / `tier` / `close`；
   - `open` 第 9 参 `compressionThreads`：批量压缩线程数（0=自动全核 / 1=串行默认，TPS 优先 / N≥2 限流），读世界根 `strata.properties` 的 `strata.compression.threads`。
3. Canvas 的集成 patch 由 Strata 项目维护（见 `docs/SERVER_SUPPORT.md`），随 fork 既有补丁同一 rebase 流程维护。

### 5.2 Paper 上游（自行 patch）

classpath 接入方式与 5.1 相同（`additionalRuntimeClasspath` 或 `libraries/` 目录），但 hook 点需要自己写 patch。Hook 目标类清单（Moonrise IO 控制器，均为字节边界）：

| 目标类（net.minecraft 服务端，按 Paper/Folia 源码布局） | 要 hook 的方法 |
| --- | --- |
| `ChunkDataController`（chunk 数据，type_id = 0） | `startWrite` / `readData` |
| `EntityDataController`（entity 数据，type_id = 1） | 同上（构造器补 `ServerLevel` 引用） |
| `PoiDataController`（poi 数据，type_id = 2） | 同上 |
| `ServerLevel` | 构造器尾部：per-level 解析世界根/维度目录并 `openFor`（字段 + accessor） |
| `MinecraftServer` | `saveAllChunks` 后 flush 全部 store；关服路径 `closeAll` |
| `net.minecraft.server.Main` / `org.bukkit.craftbukkit.Main` | 启动转换参数（`--strataConvertToStrata` / `--strataConvertToAnvil`） |

接入语义（已在 Canvas 验证）：写路径在 `startWrite` 拦截序列化后的 NBT 字节交给 `StrataNative.write`，成功则返回 `WriteResult.DELETE`（清除旧 Anvil 副本，region 文件不存在时无操作）；读路径在 `readData` 先查 vstore——命中返回 `SYNC_READ`、已删除返回 `NO_DATA`、未命中**落回 Anvil 原路径**（混存迁移期关键）。shim 挂在 Moonrise chunk IO 线程之下（regionizer 下游），不触碰 tick 线程模型——详见设计文档 §11（`docs/superpowers/specs/2026-08-04-strata-storage-design.md`）。完整可参考实现：本仓 `canvas-patch/` 目录（weaver patch + Java 源码，随 Canvas fork 实服验证过）。

### 5.3 模组端（Fabric / NeoForge）

把 `java-bridge.jar` 作为 mod 依赖嵌入：

- **Fabric**：mod jar 内嵌（`fabric.mod.json` 的 `jars` 字段声明内嵌 jar），或放入 `mods/` 目录 alongside；初始化入口（`ModInitializer.onInitialize`）调用 `StrataNative.load()`。
- **NeoForge/Forge**：`mods.toml`/`neoforge.mods.toml` 声明 jar-in-jar 依赖，`FMLJavaModLoadingContext` 初始化钩子里 `load()`。
- 注意：模组端复用 vanilla IO 适配层（对标 Cesium 形态），存储替换 hook 仍需模组自行实现；桥 jar 只负责 native 加载与调用转发。

---

## 6. 验证

服务端启动后（任意早期钩子），打印版本号确认 native 链路打通：

```java
StrataNative.load();                       // 幂等，重复调用无副作用
System.out.println("[Strata] " + StrataNative.version());   // 例：strata-ffi 0.1.0
```

启动日志出现 `strata-ffi <version>` 即成功；随后可对真实世界目录 `open` → `write`/`read` 一轮自测。

---

## 7. 故障排查

### `UnsatisfiedLinkError`

| 现象 | 原因 | 处理 |
| --- | --- | --- |
| `no native library found` 类错误 / 我们的 `StrataException: native library not bundled` | jar 里没有当前平台的 `natives/<file>` | 按第 3 步核对文件名（精确到大小写与 `lib` 前缀），重新 `gradle jar` |
| `System.load` 失败，提示架构/映像不匹配 | native 库与 JVM 架构不一致（如 x64 JVM 加载 aarch64 库） | 用与 JVM 相同架构的 target 重新交叉编译 |
| 调 native 方法时才抛（类已加载但符号缺失） | jar 里的 native 库版本过旧，缺新符号 | native 库与桥 jar 必须同一 commit 构建 |

不支持的平台（linux/aarch64、windows/aarch64、mac/amd64 等）会在 `load()` 直接抛 `StrataException: unsupported platform`——当前仅支持 linux/amd64、windows/amd64、mac/aarch64。

### `StrataException`

所有 native 失败都转成 `StrataException`（`RuntimeException`），消息体里带 Rust 侧 `strata_last_error` 文本：

- `strata_open failed: ...`：检查 root 路径权限/磁盘空间；也可显式 `StrataNative.lastError()` 取当前线程最后一次错误。
- `strata_write/strata_flush/... failed (code 1)`：常规失败，细节在消息内。
- `... (code 2)`：Rust 侧 panic 已在边界被 `catch_unwind` 捕获——不会炸 JVM，但说明触发了引擎 bug，请带 `lastError` 文本上报。
- `read` 返回 `null` **不是**错误：仅表示该键无记录（C 码 3 已在桥内消化）。
