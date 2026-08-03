//! Property tests for the 40-byte record envelope.

use proptest::prelude::*;
use strata_core::envelope::{Envelope, ENVELOPE_SIZE};

fn arb_envelope() -> impl Strategy<Value = Envelope> {
    (
        any::<u8>(),
        any::<u16>(),
        any::<u8>(),
        any::<i32>(),
        any::<i32>(),
        any::<u64>(),
        any::<u32>(),
        any::<u32>(),
        any::<u64>(),
    )
        .prop_map(
            |(record_ver, type_id, comp_id, chunk_x, chunk_z, gen, epoch_ts, payload_len, payload_hash)| {
                Envelope {
                    record_ver,
                    type_id,
                    comp_id,
                    chunk_x,
                    chunk_z,
                    gen,
                    epoch_ts,
                    payload_len,
                    payload_hash,
                }
            },
        )
}

proptest! {
    /// Any field values survive an encode → decode roundtrip unchanged.
    #[test]
    fn roundtrip(env in arb_envelope()) {
        let mut buf = [0u8; ENVELOPE_SIZE];
        env.encode(&mut buf);
        let dec = Envelope::decode(&buf).expect("valid envelope must decode");
        prop_assert_eq!(dec, env);
    }

    /// A single-bit flip anywhere in the encoded record is always detected:
    /// decoding either fails or yields a different envelope.
    #[test]
    fn single_bit_flip_detected(
        env in arb_envelope(),
        byte_idx in 0usize..ENVELOPE_SIZE,
        bit_idx in 0u8..8,
    ) {
        let mut buf = [0u8; ENVELOPE_SIZE];
        env.encode(&mut buf);
        buf[byte_idx] ^= 1 << bit_idx;
        match Envelope::decode(&buf) {
            Err(_) => {} // tamper caught by magic check
            Ok(dec) => prop_assert_ne!(dec, env, "tampered envelope decoded to the original value"),
        }
    }
}
