//! Codec registry for segment payloads.
//!
//! A `comp_id` byte packs two 4-bit fields:
//! - low nibble: codec id ([`CODEC_NONE`] / [`CODEC_ZSTD`])
//! - high nibble: dictionary slot (slot resolution is the caller's concern)

use std::io::{Read, Write};

use crate::StrataError;

/// No compression: payload stored as-is.
pub const CODEC_NONE: u8 = 0;
/// Zstandard compression.
pub const CODEC_ZSTD: u8 = 1;

const NIBBLE_MASK: u8 = 0x0F;

/// Extract the codec id (low 4 bits) from a `comp_id` byte.
pub fn codec_id(comp_id: u8) -> u8 {
    comp_id & NIBBLE_MASK
}

/// Extract the dictionary slot (high 4 bits) from a `comp_id` byte.
pub fn dict_slot(comp_id: u8) -> u8 {
    comp_id >> 4
}

/// Pack a codec id and a dictionary slot into a `comp_id` byte.
///
/// Both arguments are truncated to their low 4 bits.
pub fn make_comp_id(codec: u8, slot: u8) -> u8 {
    ((slot & NIBBLE_MASK) << 4) | (codec & NIBBLE_MASK)
}

/// Compression/decompression backend for segment payloads.
pub trait Codec: Send + Sync {
    /// Codec id this backend implements ([`CODEC_NONE`] or [`CODEC_ZSTD`]).
    fn id(&self) -> u8;

    /// Compress `input`, writing the result into `out` (cleared first).
    fn compress(&self, input: &[u8], out: &mut Vec<u8>) -> Result<(), StrataError>;

    /// Decompress `input`, writing the result into `out` (cleared first).
    fn decompress(&self, input: &[u8], out: &mut Vec<u8>) -> Result<(), StrataError>;
}

/// Pass-through codec: data is stored uncompressed.
pub struct NoneCodec;

impl Codec for NoneCodec {
    fn id(&self) -> u8 {
        CODEC_NONE
    }

    fn compress(&self, input: &[u8], out: &mut Vec<u8>) -> Result<(), StrataError> {
        out.clear();
        out.extend_from_slice(input);
        Ok(())
    }

    fn decompress(&self, input: &[u8], out: &mut Vec<u8>) -> Result<(), StrataError> {
        out.clear();
        out.extend_from_slice(input);
        Ok(())
    }
}

/// Zstandard codec, optionally backed by a shared dictionary.
pub struct ZstdCodec {
    level: i32,
    dict: Option<Vec<u8>>,
}

impl ZstdCodec {
    /// Create a codec at the given compression level.
    ///
    /// `dict` is an optional trained zstd dictionary; `None` disables it.
    pub fn new(level: i32, dict: Option<&[u8]>) -> Self {
        Self {
            level,
            dict: dict.map(<[u8]>::to_vec),
        }
    }

    fn compress_inner(&self, input: &[u8], out: &mut Vec<u8>) -> std::io::Result<()> {
        match &self.dict {
            None => zstd::stream::copy_encode(input, out, self.level),
            Some(dict) => {
                let prepared = zstd::dict::EncoderDictionary::copy(dict, self.level);
                let mut enc =
                    zstd::stream::write::Encoder::with_prepared_dictionary(out, &prepared)?;
                enc.write_all(input)?;
                enc.finish().map(|_| ())
            }
        }
    }

    fn decompress_inner(&self, input: &[u8], out: &mut Vec<u8>) -> std::io::Result<()> {
        match &self.dict {
            None => zstd::stream::copy_decode(input, out),
            Some(dict) => {
                let prepared = zstd::dict::DecoderDictionary::copy(dict);
                let mut dec =
                    zstd::stream::read::Decoder::with_prepared_dictionary(input, &prepared)?;
                dec.read_to_end(out).map(|_| ())
            }
        }
    }
}

impl Codec for ZstdCodec {
    fn id(&self) -> u8 {
        CODEC_ZSTD
    }

    fn compress(&self, input: &[u8], out: &mut Vec<u8>) -> Result<(), StrataError> {
        out.clear();
        self.compress_inner(input, out)
            .map_err(|e| StrataError::Codec(e.to_string()))
    }

    fn decompress(&self, input: &[u8], out: &mut Vec<u8>) -> Result<(), StrataError> {
        out.clear();
        self.decompress_inner(input, out)
            .map_err(|e| StrataError::Codec(e.to_string()))
    }
}

/// Build the codec selected by `comp_id`.
///
/// The low nibble picks the codec; unknown ids are rejected with
/// [`StrataError::Codec`]. `zstd_level` and `dict` only apply to
/// [`CODEC_ZSTD`]; a `None` dictionary means no dictionary is used.
/// The dictionary slot (high nibble) is resolved by the caller, so a
/// dictionary passed alongside slot 0 is still honored.
pub fn codec_for(
    comp_id: u8,
    zstd_level: i32,
    dict: Option<&[u8]>,
) -> Result<Box<dyn Codec>, StrataError> {
    match codec_id(comp_id) {
        CODEC_NONE => Ok(Box::new(NoneCodec)),
        CODEC_ZSTD => Ok(Box::new(ZstdCodec::new(zstd_level, dict))),
        other => Err(StrataError::Codec(format!("unknown codec id {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dict::train_dictionary;

    #[test]
    fn comp_id_bit_semantics() {
        let comp_id = make_comp_id(CODEC_ZSTD, 3);
        assert_eq!(codec_id(comp_id), CODEC_ZSTD);
        assert_eq!(dict_slot(comp_id), 3);
    }

    #[test]
    fn zstd_roundtrip() {
        let data = vec![7u8; 64 * 1024];
        let codec = codec_for(make_comp_id(CODEC_ZSTD, 0), 3, None).expect("zstd codec");

        let mut compressed = Vec::new();
        codec.compress(&data, &mut compressed).expect("compress");
        assert!(compressed.len() < data.len() / 10);

        let mut restored = Vec::new();
        codec
            .decompress(&compressed, &mut restored)
            .expect("decompress");
        assert_eq!(restored, data);
    }

    #[test]
    fn dictionary_improves_small_objects() {
        // Each sample is a small record: 40 x 0xAB + u32 counter + namespace id.
        // The record is repeated within each sample so that 120 samples clear
        // train_dictionary's minimum-data threshold (>= 100 KiB in total).
        const NUM_SAMPLES: u32 = 120;
        const REPEATS: usize = 20;

        let mut samples: Vec<Vec<u8>> = Vec::with_capacity(NUM_SAMPLES as usize);
        for i in 0..NUM_SAMPLES {
            let mut record = Vec::with_capacity(40 + 4 + 15);
            record.extend_from_slice(&[0xABu8; 40]);
            record.extend_from_slice(&i.to_le_bytes());
            record.extend_from_slice(b"minecraft:stone");
            samples.push(record.repeat(REPEATS));
        }
        let refs: Vec<&[u8]> = samples.iter().map(Vec::as_slice).collect();

        let dict = train_dictionary(&refs).expect("train");
        assert!(!dict.is_empty());

        let sample = &samples[0];
        let mut with_dict = Vec::new();
        let mut without_dict = Vec::new();
        codec_for(make_comp_id(CODEC_ZSTD, 1), 3, Some(&dict))
            .expect("dict codec")
            .compress(sample, &mut with_dict)
            .expect("compress with dict");
        codec_for(make_comp_id(CODEC_ZSTD, 0), 3, None)
            .expect("plain codec")
            .compress(sample, &mut without_dict)
            .expect("compress without dict");

        assert!(
            with_dict.len() < without_dict.len(),
            "dictionary should shrink samples: {} >= {}",
            with_dict.len(),
            without_dict.len()
        );
    }

    #[test]
    fn unknown_codec_rejected() {
        assert!(codec_for(make_comp_id(15, 0), 3, None).is_err());
    }
}
