package io.canvasmc.canvas.strata;

import dev.strata.bridge.StrataException;
import dev.strata.bridge.StrataNative;
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.nio.file.DirectoryStream;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;
import net.minecraft.nbt.CompoundTag;
import net.minecraft.nbt.NbtAccounter;
import net.minecraft.nbt.NbtIo;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;

/**
 * Owns one Strata virtual store (vstore) rooted at {@code <dimDir>/vstore}
 * and the per-dimension registry of open stores.
 *
 * <p>One vstore serves all three record types (chunk/entity/poi) of a
 * dimension directory — exactly the layout the strata-cli converter
 * produces — so overworld, nether, end and plugin-created world stores all
 * live next to their Anvil directories and remain CLI-compatible. The
 * {@code strata.properties} configuration is shared per world root.</p>
 *
 * <p>Every failure path degrades to plain Anvil: a missing or broken native
 * library can never take the server down.</p>
 */
public final class StrataWorld {

    public static final Logger LOGGER = LoggerFactory.getLogger("Strata");

    private static final Object NATIVE_LOCK = new Object();
    private static boolean nativeLoadFailed;
    private static volatile boolean nativeLoaded;

    /** Open stores keyed by the absolute, normalized dimension directory. */
    private static final Map<Path, StrataWorld> REGISTRY = new ConcurrentHashMap<>();

    private final Path configRoot;
    private final Path dimDir;
    private final Path vstore;
    private final StrataConfig config;
    private volatile long handle;

    private StrataWorld(final Path configRoot, final Path dimDir, final StrataConfig config, final long handle) {
        this.configRoot = configRoot;
        this.dimDir = dimDir;
        this.vstore = dimDir.resolve("vstore");
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
     * Opens (or reuses) the vstore for one dimension directory
     * {@code dimDir} ({@code <dimDir>/vstore}), reading the shared
     * {@code strata.properties} from {@code configRoot} (the world root).
     * Returns {@code null} when Strata is disabled there or anything fails
     * to open — in both cases the caller keeps its original Anvil behavior
     * untouched.
     *
     * @param writeTemplateIfMissing creates the CLI template
     *                               {@code strata.properties} on first start
     *                               (only the overworld level passes
     *                               {@code true})
     */
    public static StrataWorld openFor(final Path configRoot, final Path dimDir, final boolean writeTemplateIfMissing) {
        final Path root = configRoot.toAbsolutePath().normalize();
        final Path dim = dimDir.toAbsolutePath().normalize();
        final StrataWorld existing = REGISTRY.get(dim);
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
            final Path vstore = dim.resolve("vstore");
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
                    dim, StrataNative.lastError());
                return null;
            }
            final StrataWorld store = new StrataWorld(root, dim, config, handle);
            final StrataWorld race = REGISTRY.putIfAbsent(dim, store);
            if (race != null) {
                StrataNative.close(handle);
                return race;
            }
            LOGGER.info("[strata] virtual store online for {} (config={}, vstore={})", dim, root, vstore);
            if (config.tieringEnabled) {
                try {
                    StrataNative.tier(handle, true, config.tieringStableFlushes, config.tieringDemoteRatio);
                } catch (final StrataException e) {
                    LOGGER.warn("[strata] tiering enable failed for {}: {}", dim, e.getMessage());
                }
            }
            return store;
        } catch (final IOException | RuntimeException e) { // StrataException is a RuntimeException
            LOGGER.warn("[strata] store unavailable for {}, falling back to Anvil: {}", dim, e.getMessage());
            return null;
        }
    }

    /** Returns the open store for the dimension directory {@code dimDir}, or {@code null}. */
    public static StrataWorld get(final Path dimDir) {
        return REGISTRY.get(dimDir.toAbsolutePath().normalize());
    }

    /**
     * Detects every dimension root under {@code worldRoot} — same order and
     * validity rules as the strata-cli:
     *
     * <ol>
     *   <li>{@code worldRoot} itself (the overworld);</li>
     *   <li>{@code worldRoot/DIM-1} and {@code worldRoot/DIM1} (vanilla
     *       layout);</li>
     *   <li>each {@code worldRoot/dimensions/minecraft/<name>} subdirectory
     *       (Canvas/Paper layout).</li>
     * </ol>
     *
     * A directory counts as a dimension root when it holds at least one of
     * {@code region/}, {@code entities/} or {@code poi/}. The result is
     * ordered, deduplicated and normalized; pure java.nio.file, no native
     * library required.
     */
    public static List<Path> dimensionRoots(final Path worldRoot) {
        final Path root = worldRoot.toAbsolutePath().normalize();
        final List<Path> candidates = new ArrayList<>();
        candidates.add(root);
        candidates.add(root.resolve("DIM-1"));
        candidates.add(root.resolve("DIM1"));
        final Path dimensions = root.resolve("dimensions").resolve("minecraft");
        if (Files.isDirectory(dimensions)) {
            final List<Path> subdirs = new ArrayList<>();
            try (final DirectoryStream<Path> stream = Files.newDirectoryStream(dimensions)) {
                for (final Path entry : stream) {
                    if (Files.isDirectory(entry)) {
                        subdirs.add(entry);
                    }
                }
            } catch (final IOException e) {
                LOGGER.warn("[strata] could not enumerate {}: {}", dimensions, e.getMessage());
            }
            subdirs.sort(Path::compareTo);
            candidates.addAll(subdirs);
        }
        final List<Path> result = new ArrayList<>();
        for (final Path candidate : candidates) {
            if (!Files.isDirectory(candidate)) {
                continue;
            }
            if (Files.isDirectory(candidate.resolve("region"))
                || Files.isDirectory(candidate.resolve("entities"))
                || Files.isDirectory(candidate.resolve("poi"))) {
                final Path normalized = candidate.toAbsolutePath().normalize();
                if (!result.contains(normalized)) {
                    result.add(normalized);
                }
            }
        }
        return result;
    }

    public boolean enabled() {
        return this.handle != 0L;
    }

    /** The world root this store reads its {@code strata.properties} from. */
    public Path worldRoot() {
        return this.configRoot;
    }

    /** The dimension directory this store belongs to ({@code <dimDir>/vstore}). */
    public Path dimDir() {
        return this.dimDir;
    }

    public Path vstoreRoot() {
        return this.vstore;
    }

    /** Snapshot of every open store (server-wide flush / stats). */
    public static java.util.Collection<StrataWorld> openStores() {
        return java.util.List.copyOf(REGISTRY.values());
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

    /** Outcome of a vstore read. */
    public enum ReadState {
        /** Record absent in the vstore — caller falls back to Anvil. */
        MISS,
        /** Record present and valid. {@link ReadOutcome#tag()} is the data. */
        HIT,
        /** Record is an explicit deletion marker — caller returns NO_DATA. */
        DELETED
    }

    /** A vstore read result: {@code state} + the parsed tag when HIT. */
    public record ReadOutcome(ReadState state, CompoundTag tag) {
        public static ReadOutcome miss() {
            return new ReadOutcome(ReadState.MISS, null);
        }

        public static ReadOutcome hit(final CompoundTag tag) {
            return new ReadOutcome(ReadState.HIT, tag);
        }

        public static ReadOutcome deleted() {
            return new ReadOutcome(ReadState.DELETED, null);
        }
    }

    /**
     * Reads one record. Store/transport errors are logged and reported as
     * {@link ReadState#MISS} so callers fall back to Anvil instead of serving
     * corrupted data. An empty payload is the deletion marker.
     */
    public ReadOutcome read(final int x, final int z, final int typeId) {
        final long handle = this.handle;
        if (handle == 0L) {
            return ReadOutcome.miss();
        }
        final byte[] payload;
        try {
            payload = StrataNative.read(handle, x, z, typeId);
        } catch (final StrataException e) {
            LOGGER.error("[strata] read failed at ({}, {}) type {}: {}", x, z, typeId, e.getMessage());
            return ReadOutcome.miss();
        }
        if (payload == null) {
            return ReadOutcome.miss();
        }
        if (payload.length == 0) {
            return ReadOutcome.deleted();
        }
        try {
            return ReadOutcome.hit(NbtIo.readCompressed(new ByteArrayInputStream(payload), NbtAccounter.unlimitedHeap()));
        } catch (final IOException e) {
            LOGGER.error("[strata] corrupt record at ({}, {}) type {}: {}", x, z, typeId, e.getMessage());
            return ReadOutcome.miss();
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
            // null compound == deletion marker -> empty payload in the vstore
            final byte[] payload = compound == null ? null : serialize(compound);
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
