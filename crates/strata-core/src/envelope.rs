//! 40-byte fixed-size record envelope.
//!
//! Wire layout (all little-endian):
//!
//! | offset | size | field        |
//! |--------|------|--------------|
//! | 0      | 4    | magic "VSEG" |
//! | 4      | 1    | record_ver   |
//! | 5      | 2    | type_id      |
//! | 7      | 1    | comp_id      |
//! | 8      | 4    | chunk_x      |
//! | 12     | 4    | chunk_z      |
//! | 16     | 8    | gen          |
//! | 24     | 4    | epoch_ts     |
//! | 28     | 4    | payload_len  |
//! | 32     | 8    | payload_hash |

use crate::StrataError;

/// Size of an encoded envelope, in bytes.
pub const ENVELOPE_SIZE: usize = 40;

/// Magic prefix at the head of every envelope.
pub const MAGIC: [u8; 4] = *b"VSEG";

/// Record shell. The payload (NBT) is never parsed.
///
/// `comp_id`: low 4 bits = codec, high 4 bits = dictionary slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    pub record_ver: u8,
    pub type_id: u16,
    pub comp_id: u8,
    pub chunk_x: i32,
    pub chunk_z: i32,
    pub gen: u64,
    pub epoch_ts: u32,
    pub payload_len: u32,
    pub payload_hash: u64,
}

impl Envelope {
    /// Serialize into a fixed-size buffer.
    pub fn encode(&self, out: &mut [u8; ENVELOPE_SIZE]) {
        out[0..4].copy_from_slice(&MAGIC);
        out[4] = self.record_ver;
        out[5..7].copy_from_slice(&self.type_id.to_le_bytes());
        out[7] = self.comp_id;
        out[8..12].copy_from_slice(&self.chunk_x.to_le_bytes());
        out[12..16].copy_from_slice(&self.chunk_z.to_le_bytes());
        out[16..24].copy_from_slice(&self.gen.to_le_bytes());
        out[24..28].copy_from_slice(&self.epoch_ts.to_le_bytes());
        out[28..32].copy_from_slice(&self.payload_len.to_le_bytes());
        out[32..40].copy_from_slice(&self.payload_hash.to_le_bytes());
    }

    /// Parse a fixed-size buffer. Fails if the magic does not match.
    pub fn decode(b: &[u8; ENVELOPE_SIZE]) -> Result<Self, StrataError> {
        if b[0..4] != MAGIC {
            return Err(StrataError::Envelope("bad magic".into()));
        }
        Ok(Self {
            record_ver: b[4],
            type_id: u16::from_le_bytes([b[5], b[6]]),
            comp_id: b[7],
            chunk_x: i32::from_le_bytes(b[8..12].try_into().unwrap()),
            chunk_z: i32::from_le_bytes(b[12..16].try_into().unwrap()),
            gen: u64::from_le_bytes(b[16..24].try_into().unwrap()),
            epoch_ts: u32::from_le_bytes(b[24..28].try_into().unwrap()),
            payload_len: u32::from_le_bytes(b[28..32].try_into().unwrap()),
            payload_hash: u64::from_le_bytes(b[32..40].try_into().unwrap()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Envelope {
        Envelope {
            record_ver: 1,
            type_id: 7,
            comp_id: 0x10,
            chunk_x: -33,
            chunk_z: 4095,
            gen: 42,
            epoch_ts: 1_234_567,
            payload_len: 1234,
            payload_hash: 0xDEAD_BEEF,
        }
    }

    #[test]
    fn roundtrip() {
        let env = sample();
        let mut buf = [0u8; ENVELOPE_SIZE];
        env.encode(&mut buf);
        assert_eq!(&buf[0..4], b"VSEG");
        let dec = Envelope::decode(&buf).expect("decode");
        assert_eq!(dec, env);
    }

    #[test]
    fn bad_magic_rejected() {
        let env = sample();
        let mut buf = [0u8; ENVELOPE_SIZE];
        env.encode(&mut buf);
        buf[0] = b'X';
        assert!(Envelope::decode(&buf).is_err());
    }

    #[test]
    fn field_offsets_little_endian() {
        let env = sample();
        let mut buf = [0u8; ENVELOPE_SIZE];
        env.encode(&mut buf);

        assert_eq!(buf[4], 1);
        assert_eq!(u16::from_le_bytes(buf[5..7].try_into().unwrap()), 7);
        assert_eq!(buf[7], 0x10);
        assert_eq!(i32::from_le_bytes(buf[8..12].try_into().unwrap()), -33);
        assert_eq!(u64::from_le_bytes(buf[16..24].try_into().unwrap()), 42);
        assert_eq!(
            u32::from_le_bytes(buf[24..28].try_into().unwrap()),
            1_234_567
        );
        assert_eq!(u32::from_le_bytes(buf[28..32].try_into().unwrap()), 1234);
        assert_eq!(
            u64::from_le_bytes(buf[32..40].try_into().unwrap()),
            0xDEAD_BEEF
        );
    }
}
