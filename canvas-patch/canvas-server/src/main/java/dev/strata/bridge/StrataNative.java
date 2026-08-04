package dev.strata.bridge;

import java.io.IOException;
import java.io.InputStream;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardCopyOption;

/**
 * JNI bridge to the {@code strata-ffi} C ABI
 * ({@code crates/strata-ffi/include/strata_ffi.h}).
 *
 * <p>Every public wrapper maps 1:1 onto a C entry point named
 * {@code strata_<method>} via a JNI symbol
 * {@code Java_dev_strata_bridge_StrataNative_<nativeMethod>}, and follows the
 * C ABI error-code contract:
 *
 * <ul>
 *   <li>{@code 0} — success</li>
 *   <li>{@code 1} — failure; {@link #lastError()} has the details</li>
 *   <li>{@code 2} — Rust-side panic caught at the boundary; {@link #lastError()} has the details</li>
 *   <li>{@code 3} — {@link #read} only: the key has no record (the wrapper returns {@code null})</li>
 * </ul>
 *
 * <p>Lifecycle: {@link #load()} once per JVM, then {@link #open} returns an
 * opaque {@code long} handle (the C-side {@code void*}) that every other call
 * takes as its first argument; finish with {@link #close}. All handles are
 * thread-safe on the Rust side (RwLock-serialized).
 *
 * <p>This class is static-only; never instantiate it.
 */
public final class StrataNative {

    /** Record type ids used by the server integration (see strata-cli). */
    public static final int TYPE_CHUNK = 0;
    public static final int TYPE_ENTITY = 1;
    public static final int TYPE_POI = 2;

    /** Native-library load state, guarded by the double-checked lock below. */
    private static volatile boolean loaded;

    private StrataNative() {
        throw new UnsupportedOperationException("static-only class");
    }

    /**
     * Extracts the platform-appropriate {@code strata-ffi} native library from
     * the classpath ({@code /natives/<file>}) into a private temporary
     * directory and links it via {@link System#load}.
     *
     * <p>Idempotent: safe to call from multiple threads; the library is loaded
     * at most once per JVM.
     *
     * @throws StrataException on an unsupported platform or when the bundled
     *         native library is missing from the jar
     */
    public static void load() {
        if (loaded) {
            return;
        }
        synchronized (StrataNative.class) {
            if (loaded) {
                return;
            }
            String resource = libraryResource();
            try {
                Path dir = Files.createTempDirectory("strata-native-");
                Path lib = dir.resolve(resource);
                try (InputStream in = StrataNative.class.getResourceAsStream("/natives/" + resource)) {
                    if (in == null) {
                        throw new StrataException(
                                "native library not bundled: /natives/" + resource
                                        + " is missing from the classpath"
                                        + " (CI fills this slot; see docs/BUILD_GUIDE.md step 3)");
                    }
                    Files.copy(in, lib, StandardCopyOption.REPLACE_EXISTING);
                }
                System.load(lib.toAbsolutePath().toString());
                loaded = true;
            } catch (IOException e) {
                throw new StrataException("failed to extract strata-ffi native library: " + e.getMessage());
            }
        }
    }

    /** Classifies {@code os.name} + {@code os.arch} into the bundled library name. */
    private static String libraryResource() {
        String os = System.getProperty("os.name", "").toLowerCase(java.util.Locale.ROOT);
        String arch = System.getProperty("os.arch", "").toLowerCase(java.util.Locale.ROOT);
        boolean amd64 = arch.equals("amd64") || arch.equals("x86_64");
        boolean aarch64 = arch.equals("aarch64") || arch.equals("arm64");
        if (os.contains("linux") && amd64) {
            return "strata_ffi.so";
        }
        if (os.contains("windows") && amd64) {
            return "strata_ffi.dll";
        }
        if ((os.contains("mac") || os.contains("darwin")) && aarch64) {
            return "libstrata_ffi.dylib";
        }
        throw new StrataException("unsupported platform for strata-ffi: os.name="
                + System.getProperty("os.name") + ", os.arch=" + System.getProperty("os.arch")
                + " (supported: linux/amd64, windows/amd64, mac/aarch64)");
    }

    /**
     * Opens (or creates) a vstore rooted at {@code root}.
     *
     * @param root            world storage directory (must be valid UTF-8)
     * @param hotLevel        hot-tier log level (0–22)
     * @param hotEnabled      enable the hot segment-log tier
     * @param coldLevel       cold-tier level (2–22; ignored when cold disabled)
     * @param coldEnabled     enable the cold zstd-archive tier
     * @param dictionary      enable per-type zstd dictionary compression
     * @param cacheMb         page cache budget in MiB
     * @param segmentMaxBytes segment roll size in bytes
     * @param compressionThreads batch-write compression workers (0 = auto, 1 = serial default, N ≥ 2 = bounded)
     * @return opaque store handle; pass it unchanged to every other call
     * @throws StrataException when the C side returns NULL (see {@link #lastError})
     */
    public static long open(String root, int hotLevel, boolean hotEnabled,
                            int coldLevel, boolean coldEnabled,
                            boolean dictionary, long cacheMb, long segmentMaxBytes,
                            int compressionThreads) {
        if (root == null) {
            throw new StrataException("root must not be null");
        }
        long handle = openNative(root, hotLevel, hotEnabled ? 1 : 0,
                coldLevel, coldEnabled ? 1 : 0, dictionary ? 1 : 0,
                cacheMb, segmentMaxBytes, compressionThreads);
        if (handle == 0L) {
            throw new StrataException("strata_open failed: " + lastError());
        }
        return handle;
    }

    /**
     * Writes one record (compressed internally).
     *
     * @param handle store handle from {@link #open}
     * @param x      chunk X coordinate
     * @param z      chunk Z coordinate
     * @param typeId record type id (0–65535, unsigned)
     * @param nbt    payload bytes; {@code null} is treated as empty
     * @return C status code, {@code 0} on success
     * @throws StrataException on codes 1 (failure) and 2 (caught Rust panic)
     */
    public static int write(long handle, int x, int z, int typeId, byte[] nbt) {
        if (typeId < 0 || typeId > 0xFFFF) {
            throw new StrataException("typeId out of uint16 range: " + typeId);
        }
        byte[] payload = (nbt == null) ? EMPTY_BYTES : nbt;
        return check(writeNative(handle, x, z, (short) typeId, payload), "strata_write");
    }

    /**
     * Reads the latest version of a record.
     *
     * @param handle store handle from {@link #open}
     * @param x      chunk X coordinate
     * @param z      chunk Z coordinate
     * @param typeId record type id (0–65535, unsigned)
     * @return payload bytes, or {@code null} when the key has no record
     * @throws StrataException on codes 1 (failure) and 2 (caught Rust panic)
     */
    public static byte[] read(long handle, int x, int z, int typeId) {
        if (typeId < 0 || typeId > 0xFFFF) {
            throw new StrataException("typeId out of uint16 range: " + typeId);
        }
        return readNative(handle, x, z, (short) typeId);
    }

    /**
     * Flushes buffered data, merges incremental indexes, advances the epoch.
     *
     * @param handle store handle from {@link #open}
     * @return C status code, {@code 0} on success
     * @throws StrataException on codes 1 and 2
     */
    public static int flush(long handle) {
        return check(flushNative(handle), "strata_flush");
    }

    /**
     * Runs one GC pass (whole-segment drop, hole punching, compaction).
     *
     * @param handle       store handle from {@link #open}
     * @param threshold    invalid-fraction threshold (0.0–1.0) that triggers work
     * @param budgetBytes  per-pass IO budget in bytes
     * @param minHoleBytes minimum hole size worth punching
     * @return C status code, {@code 0} on success
     * @throws StrataException on codes 1 and 2
     */
    public static int gc(long handle, double threshold, long budgetBytes, long minHoleBytes) {
        return check(gcNative(handle, threshold, budgetBytes, minHoleBytes), "strata_gc");
    }

    /**
     * Runs one tiering pass (hot → cold promotion / demotion).
     *
     * @param handle        store handle from {@link #open}
     * @param enabled       enable tiering on this pass
     * @param stableFlushes flushes required before a segment counts as stable
     *                      (unsigned 32-bit)
     * @param demoteRatio   invalid-fraction ratio below which cold data demotes
     * @return C status code, {@code 0} on success
     * @throws StrataException on codes 1 and 2
     */
    public static int tier(long handle, boolean enabled, int stableFlushes, double demoteRatio) {
        return check(tierNative(handle, enabled ? 1 : 0, stableFlushes, demoteRatio), "strata_tier");
    }

    /**
     * Drops the store. {@code handle} is invalid afterwards; a zero handle is
     * a no-op.
     *
     * @param handle store handle from {@link #open}
     */
    public static void close(long handle) {
        closeNative(handle);
    }

    /** Last error message for this thread; empty or {@code null} when none. */
    public static String lastError() {
        return lastErrorNative();
    }

    /** Native library version string, e.g. {@code "strata-ffi 0.1.0"}. */
    public static String version() {
        return versionNative();
    }

    /** Maps the C ABI status codes onto {@link StrataException}. */
    private static int check(int code, String op) {
        if (code == 0) {
            return code;
        }
        throw new StrataException(op + " failed (code " + code + "): " + lastError());
    }

    private static final byte[] EMPTY_BYTES = new byte[0];

    /* ----------------------------------------------------------------------
     * Native declarations. Each is `private static native` and binds to a
     * Rust-implemented JNI symbol named exactly
     * Java_dev_strata_bridge_StrataNative_<nativeMethodName>
     * (no overload mangling: native names are unique, the `Native` suffix
     * exists precisely to avoid overloading the public wrappers above).
     * Each symbol delegates to the matching strata_ffi.h entry point.
     * ----------------------------------------------------------------------
     *
     * openNative -> strata_open: C boolean flags are int32_t (nonzero =
     * enabled); the C void* handle travels as jlong (NULL comes back as 0);
     * compressionThreads: 0 = auto, 1 = serial (default), N >= 2 = bounded.
     */
    private static native long openNative(String root, int hotLevel, int hotEnabled,
                                          int coldLevel, int coldEnabled, int dictionary,
                                          long cacheMb, long segmentMaxBytes, int compressionThreads);

    /*
     * writeNative -> strata_write: C uint16_t type_id is carried by a Java
     * short (unsigned interpretation, range-checked by the wrapper);
     * byte[] nbt maps the C (const uint8_t* nbt, size_t len) pair.
     */
    private static native int writeNative(long handle, int x, int z, short typeId, byte[] nbt);

    /*
     * readNative -> strata_read: the native helper fills the C out-pointer
     * pair, copies the result into a fresh byte[], and releases the C buffer
     * with strata_read_free. Returns null for code 3 (no record); throws
     * StrataException on codes 1/2.
     */
    private static native byte[] readNative(long handle, int x, int z, short typeId);

    /* flushNative -> strata_flush: int32_t status code. */
    private static native int flushNative(long handle);

    /* gcNative -> strata_gc: uint64_t sizes travel as (unsigned) long. */
    private static native int gcNative(long handle, double threshold, long budgetBytes, long minHoleBytes);

    /* tierNative -> strata_tier: C int32_t enabled + uint32_t stable_flushes
     * (unsigned int on the Java side). */
    private static native int tierNative(long handle, int enabled, int stableFlushes, double demoteRatio);

    /* close: C void; NULL handle is a no-op. */
    private static native void closeNative(long handle);

    /* last_error / version: the native helpers copy into a bounded stack
     * buffer and return the resulting String. */
    private static native String lastErrorNative();

    private static native String versionNative();
}
