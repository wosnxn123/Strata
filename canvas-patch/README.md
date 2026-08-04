# Strata Storage — Canvas Integration Patch

Status: **complete and build-verified** (applyAllPatches + compileJava + createPaperclipJar all green on CNB, JDK 25, weaver-patcher 2.4.5, MC 26.2). Round 3: multi-world/multi-dimension extension (per-level vstore at `<dimDir>/vstore`, shared config at world root); 9-parameter `StrataNative.open` with `compressionThreads`.

## Contents

minecraft-patches/features/0004-Strata-Storage.patch   weaver feature patch (6 files; multi-world ServerLevel hook)
paper-patches/features/0001-Strata-Storage.patch       weaver feature patch (CraftBukkit Main flags, unchanged)
GlobalConfiguration.diff                                /strata command registration (main repo)
fork-commit-multiworld.patch                            git format-patch of the full fork commit 3812aae (push blocked: no GitHub creds on CNB; apply with git am)
canvas-server/src/main/java/                            plain sources for the main canvas repo:
  dev/strata/bridge/StrataNative.java                   vendored JNI bridge
  dev/strata/bridge/StrataException.java
  io/canvasmc/canvas/strata/StrataWorld.java            per-dimension vstore owner + registry (multi-world)
  io/canvasmc/canvas/strata/StrataConfig.java           strata.properties parser (CLI parity)
  io/canvasmc/canvas/strata/StrataConverter.java        Anvil<->vstore converter
  io/canvasmc/canvas/strata/StrataStartup.java          startup-flag conversion runner
  io/canvasmc/canvas/subcommands/StrataSubCommand.java  /strata command
```

## Applying to the fork

1. Copy both `.patch` files into `canvas-server/minecraft-patches/features/` and
   `canvas-server/paper-patches/features/` of the Canvas fork (numbering
   continues the existing series; verified non-conflicting at fork HEAD 3f18522).
2. Copy `canvas-server/src/main/java/**` over the repo's same paths (new dirs
   `dev/strata/bridge/`, `io/canvasmc/canvas/strata/`; new file
   `io/canvasmc/canvas/subcommands/StrataSubCommand.java`).
3. Apply `GlobalConfiguration.diff` (`git apply`) — registers `/strata` in the
   Canvas command tree.
4. `JAVA_HOME=<jdk25> ./gradlew applyAllPatches :canvas-server:compileJava --no-daemon`

Note on placement: the Strata hooks live in the **feature** patch layer, not
base, because weaver's file-patch layer (the squashed `canvas File Patches`
commit) also modifies ServerLevel's constructor region and MinecraftServer;
a base patch would sit under those edits. Feature layer = applied last = the
exact tree verified to compile.

## What the patch does

- **Config**: `<worldRoot>/strata.properties`, key `strata.enabled` (default
  `false`, template auto-created on first start of the overworld). Additional
  key `strata.compression.threads` (default 1 = serial; 0 = auto; N ≥ 2 =
  bounded batch-write compression workers). Disabled =>
  every code path is byte-identical to upstream (all hooks null-check the
  per-level store handle, which is only opened when enabled).
- **Scope**: multi-world — every level (overworld/nether/end and
  plugin-created worlds) resolves its own dimension directory and gets its
  own vstore at `<dimDir>/vstore`; the shared config (`strata.properties`)
  is read from the world root. One vstore serves all three record types
  (chunk=0, entity=1, poi=2), matching strata-cli's layout.
- **Read routing** (`readData` in Chunk/Entity/Poi DataController): vstore
  first -> HIT returns `SYNC_READ` with the parsed CompoundTag; DELETED
  (empty-payload marker) returns `NO_DATA`; MISS or any error falls through
  to the original Anvil read.
- **Write routing** (`startWrite`): writes CompoundTag -> compressed NBT bytes
  into the vstore; on success returns `WriteData(compound, DELETE, null,
  null)` which clears the now-stale Anvil record (no-op if the region file
  doesn't exist); on any failure falls through to the original Anvil write —
  data is never lost.
- **Lifecycle**: store opened in the ServerLevel constructor (overworld only),
  flushed on every `saveAllChunks(flush=true)`, closed in `stopPart2()` before
  the level lock is released.
- **Native bridge**: vendored `dev.strata.bridge.StrataNative`; at runtime it
  extracts `/natives/strata_ffi.{so,dll,dylib}` from the classpath into a temp
  dir and `System.load`s it. Missing/broken native => one warning, permanent
  Anvil fallback, never a crash.

## Native wiring (deployment)

Place the built native library into a classpath resource slot the server loads:

- linux/amd64 -> `/natives/strata_ffi.so`
- windows/amd64 -> `/natives/strata_ffi.dll`
- mac/aarch64 -> `/natives/libstrata_ffi.dylib`

Easiest: jar the native as `/natives/<file>` and drop it into Paper's
`libraries/` dir (auto-added to the runtime classpath) or wire it via
paperweight's `additionalRuntimeClasspath`.

## /strata command

`/strata` (also `/canvas strata`):

- `/strata version` — bridge load state + native version
- `/strata stats` — per-level Anvil/Strata status + vstore size
- `/strata flush` — force-flush every open vstore
- `/strata convert-to-strata` / `/strata convert-to-anvil` — synchronous
  conversion (refuses while Strata is active or players are online; saves all
  Anvil data first; keeps the source side, prints verify-then-delete reminder)

Permission nodes follow the Canvas scheme `canvas.command.strata.<sub>`.

## Startup conversion flags

```
java -jar canvas-paperclip-26.2.local-SNAPSHOT.jar --strataConvertToStrata
java -jar canvas-paperclip-26.2.local-SNAPSHOT.jar --strataConvertToAnvil
```

Runs the conversion **before** world load, prints the keep-the-source
reminder, then boots normally (Cesium-style). A failed conversion aborts
startup to avoid a mixed-format world. Remove the flag afterwards.

## Build evidence (CNB, 2026-08-04/05)

Round 3 (multi-world/dimension extension, strata 7564046):

```
weaver rebuild (both):       BUILD SUCCESSFUL in 6s (GRADLE_RC=0)
  0004 vs local reference:   byte-identical hunks; only the git index-hash line
                             differs (weaver format artifact, not content)
  0001:                      unchanged (md5 41837d8cb1beb06fe320b64e1532cba6)
applyAllPatches:             BUILD SUCCESSFUL in 24s (APPLY_RC=0)
:canvas-server:compileJava:  BUILD SUCCESSFUL in 33s (COMPILE_RC=0)  ← key gate
:canvas-server:createPaperclipJar: BUILD SUCCESSFUL in 46s (JAR_RC=0)
  -> /root/canvas/canvas-server/build/libs/canvas-paperclip-26.2.local-SNAPSHOT.jar (62,947,874 bytes)
```

Fork commit `3812aae4c4d51883e72200079ed4651ecd590a07` on CNB /root/canvas main
(10 files: 2 patches + GlobalConfiguration + 7 sources). Push to GitHub
**blocked** (no usable GitHub credentials on CNB); shipped here as
`fork-commit-multiworld.patch` (apply with `git am`).

Round 2 history: 9-param `open` (compressionThreads) rework, patches
byte-identical. Round 1: 1 compile error fixed (multi-catch subclassing,
`StrataException` already extends `RuntimeException`).

## Known limitations / risks

- `convert-to-anvil` uses the existing Anvil region files as the key manifest
  (the FFI exposes no segment enumeration), so records that exist **only** in
  the vstore are not materialized back to Anvil. Full export requires
  strata-cli.
- A chunk deleted while Strata is active writes an empty-payload marker to the
  vstore and clears the Anvil copy; if Strata is later disabled without
  converting back, that chunk appears gone to Anvil-only readers.
- strata-cli itself still converts overworld only; the server-side converter
  here walks all dimensions of the world root.
- Mojang manifest downloads from CNB needed a hosts pin (`piston-meta.mojang.com`);
  unrelated to the patch itself.
