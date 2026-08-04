package io.canvasmc.canvas.subcommands;

import com.mojang.brigadier.Command;
import com.mojang.brigadier.builder.LiteralArgumentBuilder;
import dev.strata.bridge.StrataException;
import io.canvasmc.canvas.commands.SubCommand;
import io.canvasmc.canvas.strata.StrataConfig;
import io.canvasmc.canvas.strata.StrataConverter;
import io.canvasmc.canvas.strata.StrataWorld;
import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import net.minecraft.commands.CommandBuildContext;
import net.minecraft.commands.CommandSourceStack;
import net.minecraft.commands.Commands;
import net.minecraft.network.chat.Component;
import net.minecraft.server.MinecraftServer;
import net.minecraft.server.level.ServerLevel;

/**
 * {@code /strata} (also {@code /canvas strata} and {@code /canvas:strata}):
 * Strata vstore inspection, maintenance and one-shot conversion.
 *
 * <p>Conversions run synchronously on the command thread; they refuse to run
 * when Strata is active for any level (the vstore handle would be live) or
 * while players are online, and they force a full Anvil save first so no
 * in-flight data is lost.
 */
public final class StrataSubCommand implements SubCommand {

    @Override
    public String getName() {
        return "strata";
    }

    @Override
    public String getDescription() {
        return "Strata storage integration: version, stats, flush and Anvil conversion";
    }

    @Override
    public LiteralArgumentBuilder<CommandSourceStack> construct(final LiteralArgumentBuilder<CommandSourceStack> base, final CommandBuildContext context) {
        return base
            .executes(source -> version(source.getSource()))
            .then(Commands.literal("version")
                .executes(source -> version(source.getSource())))
            .then(Commands.literal("stats")
                .executes(source -> stats(source.getSource())))
            .then(Commands.literal("flush")
                .executes(source -> flush(source.getSource())))
            .then(Commands.literal("convert-to-strata")
                .executes(source -> convert(source.getSource(), true, false))
                .then(Commands.literal("-f").executes(source -> convert(source.getSource(), true, true)))
                .then(Commands.literal("--force").executes(source -> convert(source.getSource(), true, true))))
            .then(Commands.literal("convert-to-anvil")
                .executes(source -> convert(source.getSource(), false, false))
                .then(Commands.literal("-f").executes(source -> convert(source.getSource(), false, true)))
                .then(Commands.literal("--force").executes(source -> convert(source.getSource(), false, true))));
    }

    private static int version(final CommandSourceStack source) {
        final boolean loaded = StrataWorld.ensureNative();
        source.sendSuccess(() -> Component.literal("Strata bridge: " + (loaded ? "loaded" : "unavailable (server runs on Anvil)")), false);
        source.sendSuccess(() -> Component.literal("native: " + StrataWorld.nativeVersion()), false);
        return Command.SINGLE_SUCCESS;
    }

    private static int stats(final CommandSourceStack source) {
        final MinecraftServer server = source.getServer();
        source.sendSuccess(() -> Component.literal("Strata native: " + StrataWorld.nativeVersion()), false);
        final Path worldDir = server.storageSource.getLevelDirectory().path();
        for (final ServerLevel level : server.getAllLevels()) {
            final StrataWorld strata = level.canvas$strataWorld();
            if (strata == null) {
                final boolean configured = Files.isRegularFile(worldDir.resolve(StrataConfig.CONFIG_FILE));
                source.sendSuccess(() -> Component.literal(
                    level.dimension().identifier() + ": Anvil (strata.properties " + (configured ? "present, enabled=false" : "absent") + ")"), false);
                continue;
            }
            final long bytes = strata.vstoreBytes();
            source.sendSuccess(() -> Component.literal(
                level.dimension().identifier() + ": Strata active, vstore " + strata.vstoreRoot()
                    + (bytes >= 0 ? " (" + humanBytes(bytes) + ")" : "")), false);
        }
        return Command.SINGLE_SUCCESS;
    }

    private static int flush(final CommandSourceStack source) {
        int flushed = 0;
        for (final ServerLevel level : source.getServer().getAllLevels()) {
            final StrataWorld strata = level.canvas$strataWorld();
            if (strata != null) {
                strata.flush();
                flushed++;
            }
        }
        final int count = flushed;
        source.sendSuccess(() -> Component.literal("Flushed " + count + " Strata vstore(s)"), false);
        return Command.SINGLE_SUCCESS;
    }

    private static int convert(final CommandSourceStack source, final boolean toStrata, final boolean force) {
        final MinecraftServer server = source.getServer();
        for (final ServerLevel level : server.getAllLevels()) {
            if (level.canvas$strataWorld() != null) {
                source.sendFailure(Component.literal(
                    "Refusing: Strata is active for " + level.dimension().identifier()
                        + ". Stop the server and use --strataConvertTo" + (toStrata ? "Strata" : "Anvil") + " instead."));
                return Command.SINGLE_SUCCESS;
            }
        }
        if (!server.getPlayerList().getPlayers().isEmpty()) {
            source.sendFailure(Component.literal("Refusing: players are online. Run conversions on an empty or stopped server."));
            return Command.SINGLE_SUCCESS;
        }
        // flush every Anvil byte to disk first so the conversion sees a complete world
        server.saveAllChunks(false, true, true);
        final Path worldDir = server.storageSource.getLevelDirectory().path();
        final StrataConfig config = StrataConfig.load(worldDir, true);
        for (final String warning : config.warnings) {
            StrataWorld.LOGGER.warn("Strata config: {}", warning);
        }
        try {
            final StrataConverter.Report report = toStrata
                ? StrataConverter.convertToStrata(worldDir, config, force)
                : StrataConverter.convertToAnvil(worldDir, config, force);
            if (toStrata) {
                source.sendSuccess(() -> Component.literal(
                    "Converted " + report.regions() + " regions (" + report.records() + " records) to " + worldDir.resolve("vstore")), false);
                source.sendSuccess(() -> Component.literal(
                    "Anvil sources retained in region/, entities/, poi/ - verify, then delete them manually."), false);
                if (!config.enabled) {
                    source.sendSuccess(() -> Component.literal(
                        "Reminder: set strata.enabled=true in " + worldDir.resolve(StrataConfig.CONFIG_FILE) + " and restart to use the vstore."), false);
                }
            } else {
                source.sendSuccess(() -> Component.literal(
                    "Converted " + report.regions() + " regions (" + report.records() + " records) back to Anvil"
                        + (report.legacyRaw() > 0 ? " (" + report.legacyRaw() + " legacy raw records)" : "")), false);
                source.sendSuccess(() -> Component.literal(
                    "vstore retained at " + worldDir.resolve("vstore") + " - verify, then delete it manually."), false);
            }
        } catch (final IOException | StrataException ex) {
            StrataWorld.LOGGER.error("Strata conversion failed", ex);
            source.sendFailure(Component.literal("Conversion failed: " + ex.getMessage()));
        }
        return Command.SINGLE_SUCCESS;
    }

    private static String humanBytes(final long bytes) {
        if (bytes < 1024) {
            return bytes + " B";
        }
        final String[] units = {"KiB", "MiB", "GiB", "TiB"};
        double value = bytes;
        int unit = -1;
        do {
            value /= 1024.0;
            unit++;
        } while (value >= 1024.0 && unit < units.length - 1);
        return String.format(java.util.Locale.ROOT, "%.1f %s", value, units[unit]);
    }
}
