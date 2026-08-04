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
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.locks.ReadWriteLock;
import java.util.concurrent.locks.ReentrantReadWriteLock;
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
 * <p>Fail-closed: when a vstore with data already exists but Strata cannot
 * take it over (disabled, native library missing, open failure),
 * {@link #openFor} refuses to start the level instead of silently serving
 * Anvil — the vstore's data would otherwise be invisible. The
 * {@code strata.force-anvil=true} escape hatch overrides that refusal with
 * a loud warning on every start.</p>
 *
 * <p>Handle operations take a shared lock; {@link #close()} takes the
 * exclusive lock, so it only runs once every in-flight operation has
 * drained, and no new operation can reach the native handle afterwards.</p>
 */
public final class StrataWorld {

    public static final Logger LOGGER = LoggerFactory.getLogger("Strata");

    private static final Object NATIVE_LOCK = new Object();
    private static boolean nativeLoadFailed;
    private static volatile boolean nativeLoaded;

    /** Open stores keyed by the real (symlink/case/junction-resolved) dimension directory. */
    private static final Map<Path, StrataWorld> REGISTRY = new ConcurrentHashMap<>();

    private final Path configRoot;
    private final Path dimDir;
    private final Path vstore;
    private final StrataConfig config;
    /** Guards every native-handle operation; close() takes the write lock. */
    private final ReadWriteLock opLock = new ReentrantReadWriteLock();
    /** Successful flushes since the last maintenance pass (gc + tier). */
    private final AtomicInteger successfulFlushes = new AtomicInteger();
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
     * Registry key for a dimension directory: the real filesystem path, so
     * case-folded, 8.3 and junction/symlink spellings of the same directory
     * map onto one store; falls back to absolute+normalized when the path
     * cannot be resolved (e.g. not yet created).
     */
    private static Path registryKey(final Path dir) {
        try {
            return dir.toRealPath();
        } catch (final IOException e) {
            return dir.toAbsolutePath().normalize();
        }
    }

    /**
     * Opens (or reuses) the vstore for one dimension directory
     * {@code dimDir} ({@code <dimDir>/vstore}), reading the shared
     * {@code strata.properties} from {@code configRoot} (the world root).
     * Returns {@code null} when Strata is disabled there and no vstore data
     * would be left invisible.
     *
     * <p>Fail-closed: when a vstore already holds data
     * ({@code vstore/manifest.vsm} exists) but Strata cannot serve it this
     * run, this method throws {@link IllegalStateException} instead of
     * returning {@code null} — silently booting on Anvil would hide the
     * vstore's records. {@code strata.force-anvil=true} explicitly accepts
     * that risk (with a loud warning on every start).</p>
     *
     * @param writeTemplateIfMissing creates the CLI template
     *                               {@code strata.properties} on first start
     *                               (only the overworld level passes
     *                               {@code true})
     */
    public static StrataWorld openFor(final Path configRoot, final Path dimDir, final boolean writeTemplateIfMissing) {
        final Path root = configRoot.toAbsolutePath().normalize();
        final Path dim = registryKey(dimDir);
        final StrataWorld existing = REGISTRY.get(dim);
        if (existing != null) {
            return existing;
        }
        final StrataConfig config = StrataConfig.load(root, writeTemplateIfMissing);
        for (final String warning : config.warnings) {
            LOGGER.warn("Strata config: {}", warning);
        }
        final Path vstore = dim.resolve("vstore");
        final boolean vstorePresent = Files.isRegularFile(vstore.resolve("manifest.vsm"));
        if (!config.enabled) {
            refuseInvisibleData(dim, vstore, vstorePresent, config, "strata.enabled is not true");
            return null;
        }
        if (!ensureNative()) {
            refuseInvisibleData(dim, vstore, vstorePresent, config, "the Strata native library failed to load");
            return null;
        }
        try {
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
            final StrataWorld store = new StrataWorld(root, dim, config, handle);
            final StrataWorld race = REGISTRY.putIfAbsent(dim, store);
            if (race != null) {
                StrataNative.close(handle);
                return race;
            }
            LOGGER.info("[strata] virtual store online for {} (config={}, vstore={})", dim, root, vstore);
            return store;
        } catch (final IOException | RuntimeException e) { // StrataException is a RuntimeException
            refuseInvisibleData(dim, vstore, vstorePresent, config, "the vstore failed to open: " + e.getMessage());
            LOGGER.warn("[strata] store unavailable for {}, falling back to Anvil: {}", dim, e.getMessage());
            return null;
        }
    }

    /**
     * Fail-closed guard: a vstore with data exists but Strata will not serve
     * it this run. Refuses to boot the level unless {@code strata.force-anvil}
     * explicitly accepts that its data stays invisible (warned on every
     * start either way).
     */
    private static void refuseInvisibleData(final Path dim, final Path vstore, final boolean vstorePresent, final StrataConfig config, final String reason) {
        if (!vstorePresent) {
            return;
        }
        if (config.forceAnvil) {
            LOGGER.warn("[strata] *** vstore present at {} but Strata is not active ({}) — booting on Anvil because strata.force-anvil=true; VSTORE DATA IS INVISIBLE until Strata is re-enabled or the vstore is converted back", vstore, reason);
            return;
        }
        throw new IllegalStateException("[strata] refusing to start level " + dim + ": a Strata vstore exists at " + vstore
            + " but Strata is not active (" + reason + "), so its chunk data would be invisible. "
            + "Either set strata.enabled=true in strata.properties (and repair the native library if it failed to load), "
            + "or set strata.force-anvil=true to boot on Anvil anyway, accepting that vstore data stays invisible.");
    }

    /** Returns the open store for the dimension directory {@code dimDir}, or {@code null}. */
    public static StrataWorld get(final Path dimDir) {
        return REGISTRY.get(registryKey(dimDir));
    }

    /**
     * Detects every dimension root under {@code worldRoot} — same order and
     * validity rules as the strata-cli:
     *
     * <ol>
     * <li>{@code worldRoot} itself (the overworld);</li>
     * <li>{@code worldRoot/DIM-1} and {@code worldRoot/DIM1} (vanilla
     *     layout);</li>
     * <li>each {@code worldRoot/dimensions/minecraft/<name>} subdirectory
     *     (Canvas/Paper layout).</li>
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
     * Reads one record. An empty payload is the deletion marker.
     *
     * <p>Store errors and corrupt records are thrown as {@link IOException}
     * instead of being reported as {@link ReadState#MISS}: the Anvil copy of
     * a vstore-managed chunk has been deleted, so silently "falling back"
     * would serve stale or no data. The chunk load must fail loudly and be
     * retried/reported by the caller.
     */
    public ReadOutcome read(final int x, final int z, final int typeId) throws IOException {
        this.opLock.readLock().lock();
        try {
            final long handle = this.handle;
            if (handle == 0L) {
                return ReadOutcome.miss();
            }
            final byte[] payload;
            try {
                payload = StrataNative.read(handle, x, z, typeId);
            } catch (final StrataException e) {
                throw new IOException("[strata] vstore read failed at (" + x + ", " + z + ") type " + typeId + ": " + e.getMessage(), e);
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
                throw new IOException("[strata] corrupt vstore record at (" + x + ", " + z + ") type " + typeId + ": " + e.getMessage(), e);
            }
        } finally {
            this.opLock.readLock().unlock();
        }
    }

    /**
     * Writes one record. Returns {@code true} when the record is now owned
     * by the vstore (and the Anvil copy may be dropped), {@code false} when
     * the write failed and the caller must keep the Anvil path.
     */
    public boolean write(final int x, final int z, final int typeId, final CompoundTag compound) {
        this.opLock.readLock().lock();
        try {
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
        } finally {
            this.opLock.readLock().unlock();
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
        this.opLock.readLock().lock();
        try {
            final long handle = this.handle;
            if (handle == 0L) {
                return;
            }
            try {
                StrataNative.flush(handle);
                this.successfulFlushes.incrementAndGet();
            } catch (final StrataException e) {
                LOGGER.error("[strata] flush failed for {}: {}", this.vstore, e.getMessage());
            }
        } finally {
            this.opLock.readLock().unlock();
        }
    }

    /**
     * Schedules online maintenance: every {@code strata.tiering.stable-flushes}
     * successful flushes since the last pass, runs one GC + tiering cycle
     * ({@link #maintenanceTick()}). Called by the save-all hook after
     * {@link #flush()}; startup never scans.
     */
    public void maybeRunMaintenance() {
        final int threshold = this.config.tieringStableFlushes;
        if (threshold <= 0 || this.successfulFlushes.get() < threshold) {
            return;
        }
        this.successfulFlushes.addAndGet(-threshold);
        this.maintenanceTick();
    }

    /**
     * One online maintenance pass: GC (config threshold/budget/min-hole) +
     * flush + tiering (config stable-flushes/demote-ratio). Failures are
     * logged, never thrown — maintenance must not break the save path.
     */
    public void maintenanceTick() {
        this.opLock.readLock().lock();
        try {
            final long handle = this.handle;
            if (handle == 0L) {
                return;
            }
            if (this.config.gcEnabled) {
                try {
                    StrataNative.gc(handle, this.config.gcInvalidThreshold, this.config.gcBudgetBytes, this.config.gcMinHoleBytes);
                } catch (final StrataException e) {
                    LOGGER.warn("[strata] scheduled gc failed for {}: {}", this.vstore, e.getMessage());
                }
            }
            try {
                StrataNative.flush(handle);
            } catch (final StrataException e) {
                LOGGER.warn("[strata] post-gc flush failed for {}: {}", this.vstore, e.getMessage());
            }
            if (this.config.tieringEnabled) {
                try {
                    StrataNative.tier(handle, true, this.config.tieringStableFlushes, this.config.tieringDemoteRatio);
                } catch (final StrataException e) {
                    LOGGER.warn("[strata] scheduled tiering failed for {}: {}", this.vstore, e.getMessage());
                }
            }
        } finally {
            this.opLock.readLock().unlock();
        }
    }

    /** Best-effort GC + flush for the /strata command. */
    public String gcAndFlush() {
        this.opLock.readLock().lock();
        try {
            final long handle = this.handle;
            if (handle == 0L) {
                return "store closed";
            }
            final StringBuilder sb = new StringBuilder();
            if (this.config.gcEnabled) {
                try {
                    StrataNative.gc(handle, this.config.gcInvalidThreshold, this.config.gcBudgetBytes, this.config.gcMinHoleBytes);
                    sb.append("gc ok");
                } catch (final StrataException e) {
                    sb.append("gc error: ").append(e.getMessage());
                }
            } else {
                sb.append("gc disabled");
            }
            try {
                StrataNative.flush(handle);
                sb.append(", flushed");
            } catch (final StrataException e) {
                sb.append(", flush error: ").append(e.getMessage());
            }
            return sb.toString();
        } finally {
            this.opLock.readLock().unlock();
        }
    }

    public void close() {
        final long handle;
        this.opLock.writeLock().lock();
        try {
            // the write lock only grants after every in-flight reader has
            // drained; zeroing the handle keeps new operations off the native side
            handle = this.handle;
            this.handle = 0L;
        } finally {
            this.opLock.writeLock().unlock();
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
