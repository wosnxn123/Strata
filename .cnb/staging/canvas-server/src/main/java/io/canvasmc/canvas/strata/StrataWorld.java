package io.canvasmc.canvas.strata;

import dev.strata.bridge.StrataException;
import dev.strata.bridge.StrataNative;
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;
import net.minecraft.nbt.CompoundTag;
import net.minecraft.nbt.NbtAccounter;
import net.minecraft.nbt.NbtIo;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;

/**
 * Owns one Strata virtual store (vstore) rooted at {@code <worldRoot>/vstore}
 * and the per-dimension registry of open stores.
 *
 * <p>One vstore serves all three record types (chunk/entity/poi) of a world
 * root — exactly the layout the strata-cli converter produces — so
 * overworld, nether and end stores live next to their Anvil directories and
 * remain CLI-compatible.</p>
 *
 * <p>Every failure path degrades to plain Anvil: a missing or broken native
 * library can never take the server down.</p>
 */
public final class StrataWorld {

    public static final Logger LOGGER = LoggerFactory.getLogger("Strata");

    private static final Object NATIVE_LOCK = new Object();
    private static boolean nativeLoadFailed;
    private static volatile boolean nativeLoaded;

    /** Open stores keyed by the absolute, normalized world root path. */
    private static final Map<Path, StrataWorld> REGISTRY = new ConcurrentHashMap<>();

    private final Path worldRoot;
    private final Path vstore;
    private final StrataConfig config;
    private volatile long handle;

    private StrataWorld(final Path worldRoot, final StrataConfig config, final long handle) {
        this.worldRoot = worldRoot;
        this.vstore = worldRoot.resolve("vstore");
        this.config = config;
        this.handle = handle;
    }

    /**
     * Loads the Strata native library (idempotent). Returns {@code false}
     * once it fails so every caller permanently stays on the Anvil path.
     */
    public static boolean ensureNative() {
        if (nativeLoaded) {
            return true;
        }
        synchronized (NATIVE_LOCK) {
            if (nativeLoaded) {
                return true;
            }
            if (nativeLoadFailed) {
                return false;
            }
            try {
                StrataNative.load();
                nativeLoaded = true;
                LOGGER.info("[strata] native bridge loaded, version {}", StrataNative.version());
                return true;
            } catch (final Throwable t) {
                nativeLoadFailed = true;
                LOGGER.warn("[strata] native bridge unavailable, storage stays on Anvil: {}", t.getMessage());
                return false;
            }
        }
    }

    public static String nativeVersion() {
        return nativeLoaded ? safeVersion() : "unloaded";
    }

    private static String safeVersion() {
        try {
            return StrataNative.version();
        } catch (final Throwable t) {
            return "error: " + t.getMessage();
        }
    }

    /**
     * Opens (or reuses) the vstore for {@code worldRoot}. Returns
     * {@code null} when Strata is disabled there or anything fails to open —
     * in both cases the caller keeps its original Anvil behavior untouched.
     *
     * @param writeTemplateIfMissing creates the CLI template
     *                               {@code strata.properties} on first start
     *                               (used for the overworld root only)
     */
    public static StrataWorld openFor(final Path worldRoot, final boolean writeTemplateIfMissing) {
        final Path root = worldRoot.toAbsolutePath().normalize();
        final StrataWorld existing = REGISTRY.get(root);
        if (existing != null) {
            return existing;
        }
        final StrataConfig config = StrataConfig.load(root, writeTemplateIfMissing);
        for (final String warning : config.warnings) {
            LOGGER.warn("Strata config: {}", warning);
        }
        if (!config.enabled) {
            return null;
        }
        if (!ensureNative()) {
            return null;
        }
        try {
            final Path vstore = root.resolve("vstore");
            Files.createDirectories(vstore);
            final long handle = StrataNative.open(
                vstore.toString(),
                config.hotLevel, config.hotEnabled,
                config.coldLevel, config.coldEnabled,
                config.dictionary,
                config.cacheMb,
                config.segmentMaxBytes,
                config.compressionThreads
            );
            if (handle == 0L) {
                LOGGER.warn("[strata] store open returned null handle for {} ({}), falling back to Anvil",
                    root, StrataNative.lastError());
                return null;
            }
            final StrataWorld store = new StrataWorld(root, config, handle);
            final StrataWorld race = REGISTRY.putIfAbsent(root, store);
            if (race != null) {
                StrataNative.close(handle);
                return race;
            }
            LOGGER.info("[strata] virtual store online for {} (root={})", root, vstore);
            if (config.tieringEnabled) {
                try {
                    StrataNative.tier(handle, true, config.tieringStableFlushes, config.tieringDemoteRatio);
                } catch (final StrataException e) {
                    LOGGER.warn("[strata] tiering enable failed for {}: {}", root, e.getMessage());
                }
            }
            return store;
        } catch (final StrataException | IOException | RuntimeException e) {
            LOGGER.warn("[strata] store unavailable for {}, falling back to Anvil: {}", root, e.getMessage());
            return null;
        }
    }

    /** Returns the open store for {@code worldRoot}, or {@code null}. */
    public static StrataWorld get(final Path worldRoot) {
        return REGISTRY.get(worldRoot.toAbsolutePath().normalize());
    }

    public boolean enabled() {
        return this.handle != 0L;
    }

    public Path worldRoot() {
        return this.worldRoot;
    }

    public Path vstoreRoot() {
        return this.vstore;
    }

    /** Best-effort total size of the vstore directory in bytes, or -1. */
    public long vstoreBytes() {
        try (final var walk = Files.walk(this.vstore)) {
            return walk.filter(Files::isRegularFile).mapToLong(p -> {
                try {
                    return Files.size(p);
                } catch (final IOException e) {
                    return 0L;
                }
            }).sum();
        } catch (final IOException e) {
            return -1L;
        }
    }

    /**
     * Reads one record. Returns {@code null} when the record is absent or
     * unreadable; an empty tag signals a deletion marker. Store/transport
     * errors are logged and treated as misses so callers can fall back to
     * Anvil instead of serving corrupted data.
     */
    public CompoundTag read(final int x, final int z, final int typeId) {
        final long handle = this.handle;
        if (handle == 0L) {
            return null;
        }
        final byte[] payload;
        try {
            payload = StrataNative.read(handle, x, z, typeId);
        } catch (final StrataException e) {
            LOGGER.error("[strata] read failed at ({}, {}) type {}: {}", x, z, typeId, e.getMessage());
            return null;
        }
        if (payload == null) {
            return null;
        }
        if (payload.length == 0) {
            return new CompoundTag(); // deletion marker
        }
        try {
            return NbtIo.readCompressed(new ByteArrayInputStream(payload), NbtAccounter.unlimitedHeap());
        } catch (final IOException e) {
            LOGGER.error("[strata] corrupt record at ({}, {}) type {}: {}", x, z, typeId, e.getMessage());
            return null;
        }
    }

    /**
     * Writes one record. Returns {@code true} when the record is now owned
     * by the vstore (and the Anvil copy may be dropped), {@code false} when
     * the write failed and the caller must keep the Anvil path.
     */
    public boolean write(final int x, final int z, final int typeId, final CompoundTag compound) {
        final long handle = this.handle;
        if (handle == 0L) {
            return false;
        }
        try {
            final byte[] payload = serialize(compound);
            final int rc = StrataNative.write(handle, x, z, typeId, payload);
            if (rc != 0) {
                LOGGER.error("[strata] write failed at ({}, {}) type {}: {} (rc={})",
                    x, z, typeId, StrataNative.lastError(), rc);
                return false;
            }
            return true;
        } catch (final StrataException e) {
            LOGGER.error("[strata] write failed at ({}, {}) type {}: {}", x, z, typeId, e.getMessage());
            return false;
        }
    }

    private static byte[] serialize(final CompoundTag compound) {
        final ByteArrayOutputStream out = new ByteArrayOutputStream();
        try {
            NbtIo.writeCompressed(compound, out);
        } catch (final IOException e) {
            throw new StrataException("Failed to serialize CompoundTag: " + e.getMessage());
        }
        return out.toByteArray();
    }

    public void flush() {
        final long handle = this.handle;
        if (handle == 0L) {
            return;
        }
        try {
            StrataNative.flush(handle);
        } catch (final StrataException e) {
            LOGGER.error("[strata] flush failed for {}: {}", this.vstore, e.getMessage());
        }
    }

    /** Best-effort GC + flush for the /strata command. */
    public String gcAndFlush() {
        final StringBuilder sb = new StringBuilder();
        if (this.config.gcEnabled) {
            try {
                final long freed = StrataNative.gc(this.handle, this.config.gcInvalidThreshold,
                    this.config.gcBudgetBytes, this.config.gcMinHoleBytes);
                sb.append("gc freed ").append(freed).append(" bytes");
            } catch (final StrataException e) {
                sb.append("gc error: ").append(e.getMessage());
            }
        } else {
            sb.append("gc disabled");
        }
        try {
            StrataNative.flush(this.handle);
            sb.append(", flushed");
        } catch (final StrataException e) {
            sb.append(", flush error: ").append(e.getMessage());
        }
        return sb.toString();
    }

    public void close() {
        final long handle;
        synchronized (this) {
            handle = this.handle;
            this.handle = 0L;
        }
        REGISTRY.values().remove(this);
        if (handle != 0L) {
            try {
                StrataNative.close(handle);
            } catch (final StrataException e) {
                LOGGER.error("[strata] close failed for {}: {}", this.vstore, e.getMessage());
            }
        }
    }

    /** Closes every open store (server shutdown). */
    public static void closeAll() {
        for (final StrataWorld store : REGISTRY.values().toArray(new StrataWorld[0])) {
            store.close();
        }
    }
}
