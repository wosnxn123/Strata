/* strata-ffi — C ABI for the Strata vstore.
 *
 * Handles: `void*` returned by strata_open points to a store whose
 * operations (read/write/flush/gc/tier) are thread-safe on the same handle
 * (RwLock-serialized). strata_close destroys the handle itself, so the
 * CALLER must ensure close is mutually exclusive with all in-flight
 * operations on that handle (quiesce first, then close). After close the
 * handle is permanently invalid and must never be passed to any function.
 * Pass the handle back unchanged to every call and finish with
 * strata_close.
 *
 * Error codes (all int32_t-returning functions):
 *   0  success
 *   1  failure — call strata_last_error for details
 *   2  Rust-side panic, caught at the boundary — details via strata_last_error
 *   3  strata_read only: the key has no record
 *
 * The JNI layer (feature "jni", Java_dev_strata_bridge_StrataNative_*) uses
 * different error semantics: readNative throws
 * dev.strata.bridge.StrataException on failure (a NULL return with no
 * pending exception means "no record"), while the C ABI here reports
 * failures via codes 1/2/3 plus strata_last_error. JNI string arguments
 * are read as UTF-16 (GetStringChars), so supplementary-plane code points
 * in paths (emoji, CJK extension B) are fully supported.
 *
 * Null pointer arguments yield code 1 ("null pointer"); strata_write accepts
 * (nbt == NULL, len == 0) as an empty payload. All `char*` inputs must be
 * NUL-terminated valid UTF-8.
 *
 * Buffer ownership: strata_read allocates via Rust's global allocator; free
 * the returned buffer with strata_read_free(buf, len) — never free()/delete.
 * strata_last_error / strata_version copy at most len-1 bytes and always
 * NUL-terminate when len > 0; they return the number of bytes copied
 * (excluding the terminator) or -1 if buf is NULL.
 */
#ifndef STRATA_FFI_H
#define STRATA_FFI_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Open (or create) a vstore at `root`.
 * Boolean flags are int32_t: nonzero = enabled.
 * compression_threads: batch-write compression workers — 0 = auto (all
 * available cores), 1 = serial (default), N >= 2 = bounded to N threads.
 * Returns NULL on failure; see strata_last_error. */
void* strata_open(const char* root,
                  int32_t hot_level,
                  int32_t hot_enabled,
                  int32_t cold_level,
                  int32_t cold_enabled,
                  int32_t dictionary,
                  uint64_t cache_mb,
                  uint64_t segment_max_bytes,
                  int32_t compression_threads);

/* Write one record (compressed internally). */
int32_t strata_write(void* h,
                     int32_t x,
                     int32_t z,
                     uint16_t type_id,
                     const uint8_t* nbt,
                     size_t len);

/* Read the latest version of a record.
 * On success (0) the payload is returned in *out_ptr/*out_len and must be
 * released with strata_read_free. Returns 3 when the key has no record. */
int32_t strata_read(void* h,
                    int32_t x,
                    int32_t z,
                    uint16_t type_id,
                    uint8_t** out_ptr,
                    size_t* out_len);

/* Release a buffer allocated by strata_read. */
void strata_read_free(uint8_t* buf, size_t len);

/* Flush buffered data, merge incremental indexes, advance the epoch. */
int32_t strata_flush(void* h);

/* Run one GC pass (whole-segment drop, hole punching, compaction). */
int32_t strata_gc(void* h,
                  double invalid_threshold,
                  uint64_t budget_bytes,
                  uint64_t min_hole_bytes);

/* Run one tiering pass (hot -> cold promotion / demotion). */
int32_t strata_tier(void* h,
                    int32_t enabled,
                    uint32_t stable_flushes,
                    double invalid_demote_ratio);

/* Drop the store. The caller must guarantee no operation is in flight on
 * `h` (operations are RwLock-serialized, but close destroys the lock
 * itself — concurrent use after/with close is use-after-free). `h` is
 * permanently invalid afterwards; NULL is a no-op. */
void strata_close(void* h);

/* Copy the last error message for this thread into buf (NUL-terminated,
 * truncated when buf is too small). Returns bytes copied or -1 on NULL buf. */
int32_t strata_last_error(uint8_t* buf, size_t len);

/* Write "strata-ffi <version>" into buf (same truncation rules). */
int32_t strata_version(uint8_t* buf, size_t len);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* STRATA_FFI_H */
