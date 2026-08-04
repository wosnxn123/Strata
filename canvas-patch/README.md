# Strata Storage — Canvas (weaver) Integration Patch

Status: **partial delivery**. The complete Java support layer (config, converter,
startup hooks, `/strata` command, vendored JNI bridge) is delivered below as
`canvas-server/src/main/java/` sources. The weaver-format patch for the
Moonrise DataController/ServerLevel/Main hooks was designed but NOT yet
generated/compiled on CNB (the CNB instance was recreated mid-run; repo +
JDK25 re-provisioned, applyAllPatches/build not re-run before budget cap).

## What's in this directory

```
canvas-server/src/main/java/
  dev/strata/bridge/
    StrataNative.java      vendored JNI bridge (open/read/write/flush/gc/tier/close/version)
    StrataException.java
  io/canvasmc/canvas/strata/
    StrataWorld.java       per-world-root vstore owner + registry; Anvil-safe fallbacks
    StrataConfig.java      <worldRoot>/strata.properties parser (CLI-parity keys, default disabled)
    StrataConverter.java   Anvil<->vstore converter (region/entities/poi, DEFLATE/GZIP/NONE)
    StrataStartup.java     --strataConvertToStrata/--strataConvertToAnvil one-shot runner
  io/canvasmc/canvas/subcommands/
    StrataSubCommand.java  /strata version|stats|flush|convert-to-strata|convert-to-anvil
```

## Design (decided, not yet applied)

- Config: `strata.properties` at world root, `strata.enabled=false` default;
  off => zero deviation from vanilla paths.
- One vstore per world root at `<worldRoot>/vstore` (CLI-compatible), serving
  TYPE_CHUNK/TYPE_ENTITY/TYPE_POI.
- Read: `readData` tries Strata first -> SYNC_READ (parsed tag) or empty-tag
  deletion marker -> NO_DATA; falls through to Anvil on miss/error.
- Write: `startWrite` writes to Strata; on success returns
  `WriteData(compound, WriteResult.DELETE, null, null)` to drop the stale
  Anvil copy; on failure falls through to the Anvil path.
- Hooks: ChunkDataController/EntityDataController/PoiDataController
  (startWrite/readData), ServerLevel (open/close via `canvas$strataWorld`
  style field), MinecraftServer.saveAllChunks (flush),
  net/minecraft/server/Main + CraftBukkit Main (launch flags).
- Native: extracted from classpath `/natives/strata_ffi.{so,dll,dylib}` at
  runtime; missing native => permanent Anvil fallback, never fatal.

## Remaining steps (on a fresh CNB: ssh cnb-m7g-...-lcu@cnb.space, repo /root/canvas, JDK /opt/jdk25)

1. `JAVA_HOME=/opt/jdk25 ./gradlew applyAllPatches --no-daemon`
2. Copy `canvas-server/src/main/java/**` from this package into
   `/root/canvas/canvas-server/src/main/java/`; register `StrataSubCommand`
   in GlobalConfiguration postLoad.
3. Edit generated sources (DataControllers, ServerLevel, Main), commit inside
   `canvas-server/src/minecraft/java/.git`, then `rebuildMinecraftBasePatches`
   -> `minecraft-patches/base/0008-Strata-Storage.patch`; same for CraftBukkit
   Main -> `paper-patches/base/0007-Strata-Storage.patch`.
4. `JAVA_HOME=/opt/jdk25 ./gradlew :canvas-server:compileJava --no-daemon`
   and iterate until green; createPaperclipJar for the final artifact.

## Native wiring

java-bridge.jar is not needed (bridge is vendored). Place the built native at
`natives/strata_ffi.so` (linux-amd64) / `strata_ffi.dll` (win-amd64) /
`libstrata_ffi.dylib` (mac-aarch64) on the canvas-server runtime classpath,
e.g. `paperweight additionalRuntimeClasspath` or a `libraries/` jar
containing `/natives/<lib>`.

## Known risk

`convert-to-anvil` uses existing Anvil region files as the key manifest (FFI
exposes no segment enumeration), so records present only in the vstore are not
materialized. Full export requires strata-cli.
