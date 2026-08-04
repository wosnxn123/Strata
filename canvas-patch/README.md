# Strata Storage — Canvas Integration Patch

Status: **complete and build-verified** (applyAllPatches + compileJava + createPaperclipJar all green on CNB, JDK 25, weaver-patcher 2.4.5, MC 26.2). Round 4: adversarial-audit remediation of all Java support files + patches (strata repo fe72867): `--strataForce` guard, fail-closed startup, online maintenance, read-error escalation.

## Contents

minecraft-patches/features/0004-Strata-Storage.patch   weaver feature patch (6 files; multi-world ServerLevel hook)
paper-patches/features/0001-Strata-Storage.patch       weaver feature patch (CraftBukkit Main flags incl. --strataForce)
GlobalConfiguration.diff                                /strata command registration (main repo)
fork-commit-audit-final.patch                           git format-patch of the full fork commit 087b2f3 (push blocked: no GitHub creds on CNB; apply with git am)
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

- **Config**: `<worldRoot>/strata.properties` (finalized 14+1 key set; template
  auto-created on first start of the overworld). `strata.enabled` defaults to
  `false`; `strata.compression.threads` (default 1 = serial; 0 = auto; N ≥ 2 =
  bounded batch-write compression workers); `strata.force-anvil` (default
  `false`) is the emergency escape hatch for the fail-closed guard below.
  Disabled => every code path is byte-identical to upstream (all hooks
  null-check the per-level store handle, which is only opened when enabled) —
  unless a vstore already exists, in which case fail-closed applies.
- **Scope**: multi-world — every level (overworld/nether/end and
  plugin-created worlds) resolves its own dimension directory and gets its
  own vstore at `<dimDir>/vstore`; the shared config (`strata.properties`)
  is read from the world root. One vstore serves all three record types
  (chunk=0, entity=1, poi=2), matching strata-cli's layout.
- **Read routing** (`readData` in Chunk/Entity/Poi DataController): vstore
  first -> HIT returns `SYNC_READ` with the parsed CompoundTag; DELETED
  (empty-payload marker) returns `NO_DATA`; MISS falls through to the original
  Anvil read. Errors do **not** fall through: store failures and corrupt
  records are thrown as `IOException` (wrapped `StrataException`) — the Anvil
  copy of a vstore-managed chunk has been deleted, so silently "falling back"
  would serve stale or no data.
- **Write routing** (`startWrite`): writes CompoundTag -> compressed NBT bytes
  into the vstore via `strata_write` — **durable on return** (`write_durable`,
  two fsyncs per record); on success returns `WriteData(compound, DELETE,
  null, null)` which clears the now-stale Anvil record (no-op if the region
  file doesn't exist) — that DELETE makes the vstore the only copy, hence the
  per-record durability. On any failure falls through to the original Anvil
  write — data is never lost.
- **Lifecycle**: store opened in the ServerLevel constructor (every level:
  overworld/nether/end and plugin-created worlds), flushed on every
  `saveAllChunks(flush=true)`, closed in `stopPart2()` before the level lock
  is released. Online maintenance runs with the autosave cycle: every
  `strata.tiering.stable-flushes` successful flushes trigger one GC + tiering
  pass (`maybeRunMaintenance`); startup never scans.
- **Native bridge**: vendored `dev.strata.bridge.StrataNative`; at runtime it
  extracts `/natives/strata_ffi.{so,dll,dylib}` from the classpath into a temp
  dir and `System.load`s it. Missing/broken native => one warning and Anvil
  fallback only where no vstore exists; with an existing vstore the level
  refuses to start (fail-closed) unless `strata.force-anvil=true`. Never a
  JVM crash.
- **Fail-closed startup**: a vstore with data (`vstore/manifest.vsm` present)
  that Strata will not serve this run (disabled, native failure, open failure)
  refuses to start the level — booting on Anvil would hide the vstore's
  records. `strata.force-anvil=true` explicitly accepts that risk (vstore data
  invisible until converted back; loud warning on every start).

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
  Anvil data first; keeps the source side, prints verify-then-delete reminder).
  Guarded: a dimension that already has a vstore is refused unless
  `-f`/`--force` is given (the existing vstore may hold newer runtime data);
  convert-to-anvil additionally warns that vstore-only records are not
  enumerated by the Anvil manifest — use `strata-cli convert --to-anvil` for a
  lossless full export

Permission nodes follow the Canvas scheme `canvas.command.strata.<sub>`.

## Startup conversion flags

```
java -jar canvas-paperclip-26.2.local-SNAPSHOT.jar --strataConvertToStrata
java -jar canvas-paperclip-26.2.local-SNAPSHOT.jar --strataConvertToAnvil
java -jar canvas-paperclip-26.2.local-SNAPSHOT.jar --strataConvertToAnvil --strataForce
```

Runs the conversion **before** world load, prints the keep-the-source
reminder, then boots normally (Cesium-style). A failed conversion aborts
startup to avoid a mixed-format world. Remove the flag afterwards.

Both conversion flags refuse by default when the target dimension already has
a vstore (which may hold newer runtime data); add `--strataForce` to confirm
the overwrite/export. For `--strataConvertToAnvil` note that records written
at runtime exist only in the vstore and are not enumerated by the Anvil key
manifest — for a lossless full export use `strata-cli convert --to-anvil`.

## Build evidence (CNB, 2026-08-04/05)

Round 4 (audit remediation, strata fe72867):

```
applyAllPatches (from scratch, new audit hunks): BUILD SUCCESSFUL in 38s (APPLY_RC=0)
:canvas-server:compileJava:  BUILD SUCCESSFUL in 32s (COMPILE_RC=0) after 2 audit-patch fixes:
  - ServerLevel fail-closed message: dimension.location() -> dimension.identifier()
    (ResourceKey has no location() accessor in this codebase)
  - StrataConverter: blank-final `final byte[] nbt` assigned in try/catch
    -> javac definite-assignment violation; dropped the `final` (semantics unchanged)
:canvas-server:createPaperclipJar: BUILD SUCCESSFUL in 50s (JAR_RC=0)
  -> /root/canvas/canvas-server/build/libs/canvas-paperclip-26.2.local-SNAPSHOT.jar
weaver rebuild: 0004 content vs audit reference = exactly the identifier() fix (1 line);
                0001 content byte-identical to audit reference (index-hash-only deltas otherwise)
```

Fork commit `087b2f392e0593b2d656a90242779583a2ddd8aa` on CNB /root/canvas main
(10 files, Strata-only). Shipped as `fork-commit-audit-final.patch`.

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

Fork commit `3812aae4c4d51883e72200079ed4651ecd590a07` (10 files: 2 patches +
GlobalConfiguration + 7 sources) — **merged into [wosnxn123/Canvas](https://github.com/wosnxn123/Canvas)
`main` and pushed** (applied locally via `git am`, commit `b44177c5`).
`fork-commit-multiworld.patch` kept here as the reproducible artifact.

Round 2 history: 9-param `open` (compressionThreads) rework, patches
byte-identical. Round 1: 1 compile error fixed (multi-catch subclassing,
`StrataException` already extends `RuntimeException`).

## Known limitations / risks

- The in-server `convert-to-anvil` uses the existing Anvil region files as the
  key manifest, so records written at runtime (which exist **only** in the
  vstore) are not materialized back to Anvil. It is therefore **guarded**:
  refuses without `-f`/`--force` (command) or `--strataForce` (startup flag).
  For a lossless full export use `strata-cli convert --to-anvil`.
- A chunk deleted while Strata is active writes an empty-payload marker to the
  vstore and clears the Anvil copy. Disabling Strata afterwards no longer
  silently boots on the stale Anvil side: fail-closed refuses to start the
  level unless `strata.force-anvil=true`, and a full rollback requires
  converting the vstore back to Anvil.
- strata-cli walks all dimensions too (overworld root, `DIM-1`/`DIM1`,
  `dimensions/minecraft/*`), matching the server-side converter.
