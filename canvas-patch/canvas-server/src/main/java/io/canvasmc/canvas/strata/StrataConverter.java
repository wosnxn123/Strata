package io.canvasmc.canvas.strata;

import dev.strata.bridge.StrataNative;
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.DataInputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.io.UncheckedIOException;
import java.nio.ByteBuffer;
import java.nio.file.DirectoryStream;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardCopyOption;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.function.LongConsumer;
import java.util.zip.DeflaterOutputStream;
import java.util.zip.InflaterInputStream;
import java.util.zip.GZIPInputStream;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;

/**
 * Anvil &lt;-&gt; Strata vstore conversion, mirroring the strata-cli
 * {@code convert} semantics:
 *
 * <ul>
 *   <li>{@code convertToStrata}: reads every {@code .mca} region file in
 *       {@code region/}, {@code entities/} and {@code poi/}, writes each
 *       chunk's NBT into a fresh vstore (overwriting any existing one), and
 *       leaves the Anvil sources in place;</li>
 *   <li>{@code convertToAnvil}: aggregates the newest record per key that is
 *       reachable through the Anvil key manifest and rewrites the region
 *       files, leaving the vstore in place.</li>
 * </ul>
 *
 * <p>Both entry points detect every dimension root under the given world
 * root ({@link StrataWorld#dimensionRoots(Path)}) and convert each one
 * into its own {@code <dimRoot>/vstore}, reporting every dimension
 * individually.
 *
 * <p>Both directions run entirely on the calling thread (the startup hook and
 * the {@code /canvas strata convert} command both invoke them outside the
 * tick) and never touch live world files while the server runs.
 */
public final class StrataConverter {

    private static final Logger LOGGER = LoggerFactory.getLogger("Strata");

    private static final int SECTOR = 4096;
    private static final int HEADER_BYTES = 2 * SECTOR;
    private static final int ENTRIES = 1024;
    private static final int VER_GZIP = 1;
    private static final int VER_DEFLATE = 2;
    private static final int VER_NONE = 3;
    private static final int VER_EXTERNAL_MASK = 0x80;

    private static final String[] SOURCE_DIRS = {"region", "entities", "poi"};
    private static final int[] TYPE_IDS = {StrataNative.TYPE_CHUNK, StrataNative.TYPE_ENTITY, StrataNative.TYPE_POI};

    private StrataConverter() {
    }

    /** Anvil record: chunk-local coordinates plus the raw (decompressed) NBT. */
    private record ChunkRecord(int localX, int localZ, byte[] nbt) {
    }

    /** Aggregate result counters. */
    public record Report(long regions, long records, long skipped) {
    }

    /**
     * Converts every dimension detected under {@code worldDir} from Anvil to
     * Strata. Dimensions are detected CLI-parity
     * ({@link StrataWorld#dimensionRoots(Path)}); each dimension's
     * {@code region/}, {@code entities/} and {@code poi/} files are read
     * into its own fresh {@code <dimRoot>/vstore} (overwriting any existing
     * one), and the Anvil sources stay in place (operators verify, then
     * delete them manually — same contract as the CLI). Every dimension is
     * reported individually; the returned report aggregates all of them.
     */
    public static Report convertToStrata(final Path worldDir, final StrataConfig config) throws IOException {
        long regions = 0L;
        long records = 0L;
        for (final Path dimRoot : conversionRoots(worldDir)) {
            final Report report = convertDimToStrata(dimRoot, config);
            regions += report.regions();
            records += report.records();
        }
        return new Report(regions, records, 0L);
    }

    /** Conversion core: one dimension root from Anvil to Strata. */
    private static Report convertDimToStrata(final Path dimDir, final StrataConfig config) throws IOException {
        final Path vstore = dimDir.resolve("vstore");
        if (Files.isDirectory(vstore)) {
            deleteRecursively(vstore);
        }
        if (!StrataWorld.ensureNative()) {
            throw new IOException("Strata native library is unavailable; cannot convert");
        }
        long handle = 0L;
        try {
            handle = StrataNative.open(
                vstore.toString(),
                config.hotLevel, config.hotEnabled,
                config.coldLevel, config.coldEnabled,
                config.dictionary, config.cacheMb, config.segmentMaxBytes,
                config.compressionThreads
            );
            long regions = 0L;
            long records = 0L;
            for (int kind = 0; kind < SOURCE_DIRS.length; kind++) {
                final Path dir = dimDir.resolve(SOURCE_DIRS[kind]);
                if (!Files.isDirectory(dir)) {
                    continue;
                }
                for (final Path regionFile : listRegionFiles(dir)) {
                    final int[] base = parseRegionName(regionFile);
                    final List<ChunkRecord> chunks = readRegion(regionFile);
                    for (final ChunkRecord chunk : chunks) {
                        final int chunkX = base[0] * 32 + chunk.localX();
                        final int chunkZ = base[1] * 32 + chunk.localZ();
                        StrataNative.write(handle, chunkX, chunkZ, TYPE_IDS[kind], chunk.nbt());
                        records++;
                    }
                    StrataNative.flush(handle);
                    regions++;
                }
            }
            StrataNative.flush(handle);
            LOGGER.info("Converted {} regions ({} records) from Anvil to vstore {}", regions, records, vstore);
            return new Report(regions, records, 0L);
        } finally {
            if (handle != 0L) {
                StrataNative.close(handle);
            }
        }
    }

    /**
     * Converts every dimension detected under {@code worldDir} from Strata
     * back to Anvil (detection as in {@link #convertToStrata}). Uses the
     * existing Anvil region files as the key manifest (the CLI does the
     * same: the vstore is enumerated by scanning segment files, which the
     * FFI does not expose), rewriting each region file atomically via a temp
     * file. The vstore stays in place. Dimensions without a vstore are
     * skipped; when no dimension has one the call fails. Every dimension is
     * reported individually; the returned report aggregates all of them.
     */
    public static Report convertToAnvil(final Path worldDir, final StrataConfig config) throws IOException {
        long regions = 0L;
        long records = 0L;
        long skipped = 0L;
        boolean convertedAny = false;
        for (final Path dimRoot : conversionRoots(worldDir)) {
            if (!Files.isDirectory(dimRoot.resolve("vstore"))) {
                LOGGER.info("No vstore under {}, skipping this dimension", dimRoot);
                continue;
            }
            final Report report = convertDimToAnvil(dimRoot, config);
            convertedAny = true;
            regions += report.regions();
            records += report.records();
            skipped += report.skipped();
        }
        if (!convertedAny) {
            throw new IOException(worldDir + ": no vstore directory, nothing to convert");
        }
        return new Report(regions, records, skipped);
    }

    /** Conversion core: one dimension root from Strata back to Anvil. */
    private static Report convertDimToAnvil(final Path dimDir, final StrataConfig config) throws IOException {
        final Path vstore = dimDir.resolve("vstore");
        if (!Files.isDirectory(vstore)) {
            throw new IOException(dimDir + ": no vstore directory, nothing to convert");
        }
        if (!StrataWorld.ensureNative()) {
            throw new IOException("Strata native library is unavailable; cannot convert");
        }
        long handle = 0L;
        try {
            handle = StrataNative.open(
                vstore.toString(),
                config.hotLevel, config.hotEnabled,
                config.coldLevel, config.coldEnabled,
                config.dictionary, config.cacheMb, config.segmentMaxBytes,
                config.compressionThreads
            );
            long regions = 0L;
            long records = 0L;
            long skipped = 0L;
            for (int kind = 0; kind < SOURCE_DIRS.length; kind++) {
                final Path dir = dimDir.resolve(SOURCE_DIRS[kind]);
                if (!Files.isDirectory(dir)) {
                    continue;
                }
                for (final Path regionFile : listRegionFiles(dir)) {
                    final int[] base = parseRegionName(regionFile);
                    final List<ChunkRecord> rewritten = new ArrayList<>();
                    for (final ChunkRecord key : readRegion(regionFile)) {
                        final int chunkX = base[0] * 32 + key.localX();
                        final int chunkZ = base[1] * 32 + key.localZ();
                        final byte[] nbt = StrataNative.read(handle, chunkX, chunkZ, TYPE_IDS[kind]);
                        if (nbt != null && nbt.length > 0) {
                            rewritten.add(new ChunkRecord(key.localX(), key.localZ(), nbt));
                        } else if (nbt == null) {
                            // key not in the vstore: keep the existing Anvil data
                            rewritten.add(key);
                            skipped++;
                        } // else: deletion marker -> drop from Anvil
                    }
                    final Path tmp = regionFile.resolveSibling(regionFile.getFileName() + ".tmp");
                    writeRegion(tmp, rewritten);
                    Files.move(tmp, regionFile, StandardCopyOption.REPLACE_EXISTING);
                    records += rewritten.size();
                    regions++;
                }
            }
            StrataNative.flush(handle);
            LOGGER.info("Converted {} regions ({} records) from vstore {} back to Anvil ({} kept from Anvil)",
                regions, records, vstore, skipped);
            return new Report(regions, records, skipped);
        } finally {
            if (handle != 0L) {
                StrataNative.close(handle);
            }
        }
    }

    /**
     * The dimension roots to convert for a world root: the CLI-parity
     * detection of {@link StrataWorld#dimensionRoots(Path)}, falling back to
     * the world root itself when nothing is detected (e.g. a world without
     * region files yet) so the legacy single-root behavior is preserved.
     */
    private static List<Path> conversionRoots(final Path worldDir) {
        final List<Path> roots = StrataWorld.dimensionRoots(worldDir);
        return roots.isEmpty() ? List.of(worldDir.toAbsolutePath().normalize()) : roots;
    }

    /** Reads one Anvil region file into its present chunks (NBT decompressed). */
    private static List<ChunkRecord> readRegion(final Path file) throws IOException {
        final byte[] data = Files.readAllBytes(file);
        if (data.length < HEADER_BYTES) {
            throw new IOException(file + ": region file too small (" + data.length + " bytes)");
        }
        final ByteBuffer buffer = ByteBuffer.wrap(data);
        final List<ChunkRecord> chunks = new ArrayList<>();
        for (int index = 0; index < ENTRIES; index++) {
            final int loc = buffer.getInt(index * 4);
            if (loc == 0) {
                continue;
            }
            final int offset = loc >>> 8;
            final int count = loc & 0xFF;
            if (count == 0) {
                continue;
            }
            final int start = offset * SECTOR;
            final int end = start + count * SECTOR;
            if (start + 5 > data.length || end > data.length) {
                throw new IOException(file + ": chunk slot " + index + " references bytes outside the file");
            }
            final int recordLen = buffer.getInt(start);
            if (recordLen <= 0 || start + 4 + recordLen > data.length) {
                throw new IOException(file + ": chunk slot " + index + " has bad record length " + recordLen);
            }
            final int version = data[start + 4] & 0xFF;
            if ((version & VER_EXTERNAL_MASK) != 0) {
                throw new IOException(file + ": unsupported external chunk at slot " + index);
            }
            final byte[] payload = new byte[recordLen - 1];
            System.arraycopy(data, start + 5, payload, 0, payload.length);
            final byte[] nbt = switch (version) {
                case VER_GZIP -> decompressGzip(payload);
                case VER_DEFLATE -> decompressDeflate(payload);
                case VER_NONE -> payload;
                default -> throw new IOException(file + ": unknown chunk compression version " + version);
            };
            chunks.add(new ChunkRecord(index & 31, index >>> 5, nbt));
        }
        return chunks;
    }

    /** Writes chunks into an Anvil region file using DEFLATE (version 2). */
    private static void writeRegion(final Path file, final List<ChunkRecord> chunks) throws IOException {
        final byte[][] records = new byte[chunks.size()][];
        final int[] lengths = new int[chunks.size()];
        for (int i = 0; i < chunks.size(); i++) {
            final byte[] compressed = compressDeflate(chunks.get(i).nbt());
            // record = 4-byte length + 1 version byte + compressed payload
            lengths[i] = compressed.length + 1;
            records[i] = compressed;
        }
        // allocate sectors
        final int[] locations = new int[ENTRIES];
        final int[] timestamps = new int[ENTRIES];
        final List<byte[]> body = new ArrayList<>();
        int sector = HEADER_BYTES / SECTOR;
        for (int i = 0; i < chunks.size(); i++) {
            final ChunkRecord chunk = chunks.get(i);
            final int index = (chunk.localX() & 31) + (chunk.localZ() & 31) * 32;
            final int recordSectors = (lengths[i] + 4 + SECTOR - 1) / SECTOR;
            locations[index] = (sector << 8) | recordSectors;
            timestamps[index] = (int) (System.currentTimeMillis() / 1000L);
            final ByteBuffer header = ByteBuffer.allocate(5);
            header.putInt(lengths[i]);
            header.put((byte) VER_DEFLATE);
            body.add(header.array());
            body.add(records[i]);
            body.add(new byte[recordSectors * SECTOR - lengths[i] - 4]);
            sector += recordSectors;
        }
        final ByteBuffer out = ByteBuffer.allocate(sector * SECTOR);
        for (final int location : locations) {
            out.putInt(location);
        }
        for (final int timestamp : timestamps) {
            out.putInt(timestamp);
        }
        for (final byte[] part : body) {
            out.put(part);
        }
        Files.write(file, out.array());
    }

    private static byte[] decompressGzip(final byte[] payload) throws IOException {
        try (final ByteArrayInputStream in = new ByteArrayInputStream(payload);
             final GZIPInputStream gzip = new GZIPInputStream(in);
             final ByteArrayOutputStream out = new ByteArrayOutputStream(payload.length * 2)) {
            gzip.transferTo(out);
            return out.toByteArray();
        }
    }

    private static byte[] decompressDeflate(final byte[] payload) throws IOException {
        try (final ByteArrayInputStream in = new ByteArrayInputStream(payload);
             final InflaterInputStream inflater = new InflaterInputStream(in);
             final ByteArrayOutputStream out = new ByteArrayOutputStream(payload.length * 2)) {
            inflater.transferTo(out);
            return out.toByteArray();
        }
    }

    private static byte[] compressDeflate(final byte[] nbt) throws IOException {
        final ByteArrayOutputStream out = new ByteArrayOutputStream(Math.max(64, nbt.length / 2));
        try (final DeflaterOutputStream deflater = new DeflaterOutputStream(out)) {
            deflater.write(nbt);
        }
        return out.toByteArray();
    }

    private static List<Path> listRegionFiles(final Path dir) throws IOException {
        final List<Path> files = new ArrayList<>();
        try (final DirectoryStream<Path> stream = Files.newDirectoryStream(dir, "*.mca")) {
            for (final Path path : stream) {
                if (path.getFileName().toString().matches("r\\.-?\\d+\\.-?\\d+\\.mca")) {
                    files.add(path);
                }
            }
        }
        files.sort(Path::compareTo);
        return files;
    }

    private static int[] parseRegionName(final Path file) throws IOException {
        final String name = file.getFileName().toString();
        final String[] parts = name.substring(2, name.length() - 4).split("\\.");
        if (parts.length != 2) {
            throw new IOException(file + ": malformed region file name");
        }
        try {
            return new int[]{Integer.parseInt(parts[0]), Integer.parseInt(parts[1])};
        } catch (final NumberFormatException ex) {
            throw new IOException(file + ": malformed region coordinates", ex);
        }
    }

    private static void deleteRecursively(final Path root) throws IOException {
        try (final java.util.stream.Stream<Path> walk = Files.walk(root)) {
            final List<Path> all = walk.sorted(java.util.Comparator.reverseOrder()).toList();
            for (final Path path : all) {
                Files.deleteIfExists(path);
            }
        }
    }
}
