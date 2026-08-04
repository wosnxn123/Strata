plugins {
    `java-library`
}

group = "dev.strata"
version = "0.1.0"

java {
    toolchain {
        languageVersion.set(JavaLanguageVersion.of(21))
    }
    withSourcesJar()
}

repositories {
    mavenCentral()
}

// Pure JNI bridge: zero external dependencies.

// copyNatives — native library staging slot.
//
// The strata-ffi shared library is cross-compiled on CI (see the
// `java-bridge` job in .github/workflows/ci.yml) and dropped into
// src/main/resources/natives/ under the exact filename StrataNative.load()
// probes for:
//
//   linux/amd64   -> strata_ffi.so        (target/release/libstrata_ffi.so)
//   windows/amd64 -> strata_ffi.dll       (target/release/strata_ffi.dll)
//   mac/aarch64   -> libstrata_ffi.dylib  (target/release/libstrata_ffi.dylib)
//
// The jar task packs whatever sits in that directory, so no wiring is needed.
// Registering an empty placeholder keeps the task discoverable and lets a
// local `gradle copyNatives` run stay a no-op.
tasks.register("copyNatives") {
    group = "build"
    description = "Placeholder: CI copies cross-compiled strata-ffi binaries into " +
            "src/main/resources/natives/ before `gradle jar` (see docs/BUILD_GUIDE.md step 3)."
}
