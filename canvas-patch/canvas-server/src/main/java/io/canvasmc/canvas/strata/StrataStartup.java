package io.canvasmc.canvas.strata;

import java.io.IOException;
import java.nio.file.Path;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;

/**
 * Startup-time one-shot conversion for the
 * {@code --strataConvertToStrata} / {@code --strataConvertToAnvil} launch
 * flags (Cesium-style): the conversion runs synchronously before the world
 * loads, prints the keep-the-source reminder, and startup then continues
 * normally.
 *
 * <p>A failed conversion aborts startup — continuing on a half-converted
 * world would mix formats and confuse operators.
 */
public final class StrataStartup {

    private static final Logger LOGGER = LoggerFactory.getLogger("Strata");

    private StrataStartup() {
    }

    public static void runConversion(final Path worldDir, final boolean toStrata) {
        LOGGER.info("Strata startup conversion requested ({} -> {}) for {}",
            toStrata ? "Anvil" : "vstore", toStrata ? "vstore" : "Anvil", worldDir);
        final StrataConfig config = StrataConfig.load(worldDir, true);
        for (final String warning : config.warnings) {
            LOGGER.warn("Strata config: {}", warning);
        }
        try {
            final StrataConverter.Report report = toStrata
                ? StrataConverter.convertToStrata(worldDir, config)
                : StrataConverter.convertToAnvil(worldDir, config);
            if (toStrata) {
                LOGGER.info("Converted {} regions ({} records) to {}", report.regions(), report.records(), worldDir.resolve("vstore"));
                LOGGER.info("Anvil sources retained in region/, entities/, poi/ — verify the world, then delete them manually.");
                if (!config.enabled) {
                    LOGGER.info("Reminder: set strata.enabled=true in {} and restart to actually use the vstore.",
                        worldDir.resolve(StrataConfig.CONFIG_FILE));
                }
                LOGGER.info("Remove the --strataConvertToStrata flag before the next start, or the vstore will be rebuilt again.");
            } else {
                LOGGER.info("Converted {} regions ({} records) back to Anvil ({} records kept from existing Anvil)",
                    report.regions(), report.records(), report.skipped());
                LOGGER.info("vstore retained at {} — verify the world, then delete it manually.", worldDir.resolve("vstore"));
                LOGGER.info("Remove the --strataConvertToAnvil flag before the next start, or the region files will be rewritten again.");
            }
        } catch (final IOException ex) {
            LOGGER.error("Strata startup conversion failed for {}", worldDir, ex);
            throw new IllegalStateException("Strata startup conversion failed: " + ex.getMessage(), ex);
        }
    }
}
