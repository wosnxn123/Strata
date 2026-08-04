package dev.strata.bridge;

/**
 * Failure reported by the strata-ffi native layer (or by the bridge while
 * loading it). The message carries the {@code strata_last_error} text for
 * codes 1 (failure) and 2 (Rust panic caught at the boundary), plus the
 * bridge-side reason when the native library itself could not be loaded.
 *
 * <p>Vendored copy of {@code java-bridge/src/main/java/dev/strata/bridge}
 * (W2 bridge contract, API frozen); kept in-tree so the Canvas source patch
 * compiles without an external dependency. The native library itself still
 * comes from the {@code java-bridge} jar placed on the runtime classpath.
 */
public class StrataException extends RuntimeException {

    private static final long serialVersionUID = 1L;

    public StrataException(String message) {
        super(message);
    }

    public StrataException(String message, Throwable cause) {
        super(message, cause);
    }
}
