//! zstd dictionary training for small, similarly-shaped payloads.

use crate::StrataError;

/// Minimum number of samples required before training is attempted.
const MIN_SAMPLES: usize = 100;
/// Minimum total sample bytes required before training is attempted.
const MIN_TOTAL_BYTES: usize = 100 * 1024;
/// Size of the dictionary to train.
const DICT_SIZE: usize = 32 * 1024;

/// Train a zstd dictionary from representative payload samples.
///
/// Returns an empty `Vec` when there is not enough material to train on
/// (fewer than 100 samples or less than 100 KiB in total); callers treat
/// an empty result as "no dictionary". A successful result is capped at
/// 32 KiB.
pub fn train_dictionary(samples: &[&[u8]]) -> Result<Vec<u8>, StrataError> {
    let total_bytes: usize = samples.iter().map(|s| s.len()).sum();
    if samples.len() < MIN_SAMPLES || total_bytes < MIN_TOTAL_BYTES {
        return Ok(Vec::new());
    }
    Ok(zstd::dict::from_samples(samples, DICT_SIZE)?)
}
