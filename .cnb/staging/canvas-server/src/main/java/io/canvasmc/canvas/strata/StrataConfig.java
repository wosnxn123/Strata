package io.canvasmc.canvas.strata;

import java.io.IOException;
import java.io.Reader;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import java.util.Locale;
import java.util.Properties;

/**
 * Java mirror of the strata-cli {@code strata.properties} world-level
 * configuration (single source of truth: the same file the CLI converter
 * reads and templates).
 *
 * <p>Recognized keys (identical to the CLI):
 * <pre>
 * strata.enabled=false                     # master switch, default false
 * strata.compression.hot=zstd-3|none
 * strata.compression.cold=zstd-9
 * strata.compression.hot-enabled=true
 * strata.compression.cold-enabled=true
 * strata.compression.dictionary=true
 * strata.compression.threads=1             # batch compression workers (default serial)
 * strata.index.cache-mb=512
 * strata.tiering.enabled=true
 * strata.tiering.stable-flushes=30
 * strata.tiering.invalid-demote-ratio=0.25
 * strata.gc.enabled=true
 * strata.gc.invalid-threshold=0.6
 * strata.gc.budget-bytes=33554432
 * strata.gc.min-hole-bytes=65536
 * </pre>
 * An explicit {@code strata.enabled=false} short-circuits parsing exactly
 * like the CLI. Unknown keys are reported as warnings, never fatal.
 */
public final class StrataConfig {

    public static final String CONFIG_FILE = "strata.properties";

    public boolean enabled;
    public int hotLevel = 3;
    public boolean hotEnabled = true;
    public int coldLevel = 9;
    public boolean coldEnabled = true;
    public boolean dictionary = true;
    public int compressionThreads = 1;
    public long cacheMb = 512;
    public long segmentMaxBytes = 64L * 1024L * 1024L;
    public boolean gcEnabled = true;
    public double gcInvalidThreshold = 0.6;
    public long gcBudgetBytes = 32L * 1024L * 1024L;
    public long gcMinHoleBytes = 64L * 1024L;
    public boolean tieringEnabled = true;
    public int tieringStableFlushes = 30;
    public double tieringDemoteRatio = 0.25;

    public final List<String> warnings = new ArrayList<>();

    private StrataConfig() {
    }

    /**
     * Loads {@code <worldRoot>/strata.properties}. A missing file yields the
     * defaults with {@code enabled=false} (and, for the converter, the CLI
     * template is written so operators can discover the switch).
     */
    public static StrataConfig load(final Path worldRoot, final boolean writeTemplateIfMissing) {
        final StrataConfig config = new StrataConfig();
        final Path file = worldRoot.resolve(CONFIG_FILE);
        if (!Files.isRegularFile(file)) {
            config.enabled = false;
            if (writeTemplateIfMissing) {
                try {
                    Files.writeString(file, TEMPLATE);
                    config.warnings.add("created " + CONFIG_FILE + " template (strata.enabled=false)");
                } catch (final IOException ex) {
                    config.warnings.add("could not create " + CONFIG_FILE + " template: " + ex.getMessage());
                }
            }
            return config;
        }
        final Properties properties = new Properties();
        try (final Reader reader = Files.newBufferedReader(file)) {
            properties.load(reader);
        } catch (final IOException ex) {
            config.warnings.add("could not read " + file + ": " + ex.getMessage() + "; storage stays disabled");
            config.enabled = false;
            return config;
        }
        // CLI parity: only an *explicit* strata.enabled=false short-circuits;
        // an absent key keeps everything parsed (warnings visible) but disabled.
        final String enabledRaw = trimToNull(properties.getProperty("strata.enabled"));
        if ("false".equalsIgnoreCase(enabledRaw)) {
            config.enabled = false;
            return config;
        }
        config.enabled = "true".equalsIgnoreCase(enabledRaw);
        for (final String key : properties.stringPropertyNames()) {
            final String value = trimToNull(properties.getProperty(key));
            switch (key) {
                case "strata.enabled" -> {
                }
                case "strata.compression.hot" -> applyCodec(config, file, key, value, true);
                case "strata.compression.cold" -> applyCodec(config, file, key, value, false);
                case "strata.compression.hot-enabled" -> config.hotEnabled = parseBool(config, file, key, value, true);
                case "strata.compression.cold-enabled" -> config.coldEnabled = parseBool(config, file, key, value, true);
                case "strata.compression.dictionary" -> config.dictionary = parseBool(config, file, key, value, true);
                case "strata.compression.threads" -> config.compressionThreads = (int) parseLong(config, file, key, value, config.compressionThreads);
                case "strata.index.cache-mb" -> config.cacheMb = parseLong(config, file, key, value, config.cacheMb);
                case "strata.gc.enabled" -> config.gcEnabled = parseBool(config, file, key, value, true);
                case "strata.gc.invalid-threshold" -> config.gcInvalidThreshold = parseDouble(config, file, key, value, config.gcInvalidThreshold);
                case "strata.gc.budget-bytes" -> config.gcBudgetBytes = parseLong(config, file, key, value, config.gcBudgetBytes);
                case "strata.gc.min-hole-bytes" -> config.gcMinHoleBytes = parseLong(config, file, key, value, config.gcMinHoleBytes);
                case "strata.tiering.enabled" -> config.tieringEnabled = parseBool(config, file, key, value, true);
                case "strata.tiering.stable-flushes" -> config.tieringStableFlushes = (int) parseLong(config, file, key, value, config.tieringStableFlushes);
                case "strata.tiering.invalid-demote-ratio" -> config.tieringDemoteRatio = parseDouble(config, file, key, value, config.tieringDemoteRatio);
                default -> config.warnings.add(file.getFileName() + ": ignoring unknown key '" + key + "'");
            }
        }
        return config;
    }

    private static void applyCodec(final StrataConfig config, final Path file, final String key, final String value, final boolean hot) {
        if (hot && "none".equals(value)) {
            config.hotEnabled = false;
            return;
        }
        if (value == null || !value.startsWith("zstd-")) {
            config.warnings.add(file + ": " + key + ": expected 'zstd-<level>' or (hot only) 'none', got '" + value + "'");
            return;
        }
        final int level;
        try {
            level = Integer.parseInt(value.substring(5));
        } catch (final NumberFormatException ex) {
            config.warnings.add(file + ": " + key + ": bad zstd level in '" + value + "'");
            return;
        }
        if (level == 0 || level < -10 || level > 22) {
            config.warnings.add(file + ": " + key + ": zstd level must be in [-10, 22] and not 0, got " + level);
            return;
        }
        if (hot) {
            config.hotLevel = level;
            config.hotEnabled = true;
        } else {
            config.coldLevel = level;
            config.coldEnabled = true;
        }
    }

    private static boolean parseBool(final StrataConfig config, final Path file, final String key, final String value, final boolean fallback) {
        if (value == null) {
            return fallback;
        }
        final String lower = value.toLowerCase(Locale.ROOT);
        if (lower.equals("true") || lower.equals("false")) {
            return Boolean.parseBoolean(lower);
        }
        config.warnings.add(file + ": " + key + ": expected true/false, got '" + value + "'");
        return fallback;
    }

    private static long parseLong(final StrataConfig config, final Path file, final String key, final String value, final long fallback) {
        if (value == null) {
            return fallback;
        }
        try {
            return Long.parseLong(value);
        } catch (final NumberFormatException ex) {
            config.warnings.add(file + ": " + key + ": expected an integer, got '" + value + "'");
            return fallback;
        }
    }

    private static double parseDouble(final StrataConfig config, final Path file, final String key, final String value, final double fallback) {
        if (value == null) {
            return fallback;
        }
        try {
            return Double.parseDouble(value);
        } catch (final NumberFormatException ex) {
            config.warnings.add(file + ": " + key + ": expected a number, got '" + value + "'");
            return fallback;
        }
    }

    private static String trimToNull(final String value) {
        if (value == null) {
            return null;
        }
        final String trimmed = value.trim();
        return trimmed.isEmpty() ? null : trimmed;
    }

    /** Same template the strata-cli writes when the file is missing. */
    private static final String TEMPLATE = """
        # Strata storage configuration / Strata 存储配置
        # Master switch (default off — opt in) / 总开关（默认关闭，需显式启用）
        strata.enabled=false
        # Cold tier (hot -> cold migration) / 冷层（热→冷迁移）
        strata.tiering.enabled=true
        strata.tiering.stable-flushes=30
        strata.tiering.invalid-demote-ratio=0.25
        # Compression / 压缩
        strata.compression.hot-enabled=true
        strata.compression.cold-enabled=true
        strata.compression.hot=zstd-3
        strata.compression.cold=zstd-9
        strata.compression.dictionary=true
        # Batch compression workers: 0=auto(all cores) 1=serial(default, TPS-first) N>=2=capped
        # 批量压缩线程：0=自动(全核) 1=串行(默认,TPS优先) N≥2=限N线程
        strata.compression.threads=1
        # Index memory budget (MiB) / 索引内存预算（MiB）
        strata.index.cache-mb=512
        # GC / 垃圾回收
        strata.gc.enabled=true
        strata.gc.invalid-threshold=0.6
        strata.gc.budget-bytes=33554432
        """;
}
