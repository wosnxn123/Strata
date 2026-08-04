//! Epoch log: crash-recoverable journal of segment writes.
//!
//! Every time a segment is written, an entry is appended to
//! `current.velog` so that after a crash the index can be rebuilt by
//! replaying the log. Entries are fixed-size ([`ENTRY_SIZE`] = 64 bytes):
//!
//! ```text
//! offset 0   [ seg_id  u32 LE ]
//! offset 4   [ 40B encoded Envelope ]
//! offset 44  [ offset  u64 LE ]
//! offset 52  [ 12B zero padding ]
//! ```
//!
//! All multi-byte integers are little-endian, matching the on-disk
//! convention of the rest of the engine.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::envelope::{Envelope, ENVELOPE_SIZE};
use crate::StrataError;

/// Size of one epoch-log entry on disk, in bytes.
pub const ENTRY_SIZE: usize = 64;

/// Byte offset of `seg_id` within an entry.
const OFF_SEG_ID: usize = 0;
/// Byte offset of the encoded envelope within an entry.
const OFF_ENV: usize = 4;
/// Byte offset of the payload `offset` within an entry.
const OFF_OFFSET: usize = OFF_ENV + ENVELOPE_SIZE; // 44
/// Byte offset of the zero padding within an entry.
#[allow(dead_code)]
const OFF_PAD: usize = OFF_OFFSET + 8; // 52

/// File name of the current epoch log inside the data directory.
pub const EPOCH_LOG_NAME: &str = "current.velog";

/// One journaled segment write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpochEntry {
    /// Segment the write landed in.
    pub seg_id: u32,
    /// Envelope describing the written payload.
    pub env: Envelope,
    /// Byte offset of the entry within the segment file.
    pub offset: u64,
}

/// Append-only epoch log backed by `current.velog`.
pub struct EpochLog {
    w: BufWriter<File>,
    path: PathBuf,
}

impl EpochLog {
    /// Open `dir/current.velog`, creating it if missing, otherwise
    /// reopening it positioned at the end.
    ///
    /// Opened in `write` (not `append`) mode: on Windows an append-mode
    /// handle (`FILE_APPEND_DATA`) cannot `SetEndOfFile`, so `rotate`'s
    /// truncate would fail with Access denied.
    pub fn open(dir: &Path) -> Result<Self, StrataError> {
        let path = dir.join(EPOCH_LOG_NAME);
        let mut file = OpenOptions::new().create(true).write(true).open(&path)?;
        file.seek(SeekFrom::End(0))?;
        Ok(Self {
            w: BufWriter::new(file),
            path,
        })
    }

    /// Append one fixed-size ([`ENTRY_SIZE`]) entry and flush it to the OS.
    pub fn record(&mut self, e: &EpochEntry) -> Result<(), StrataError> {
        let mut buf = [0u8; ENTRY_SIZE]; // zero padding pre-filled
        buf[OFF_SEG_ID..OFF_SEG_ID + 4].copy_from_slice(&e.seg_id.to_le_bytes());
        let mut env_buf = [0u8; ENVELOPE_SIZE];
        env_buf.copy_from_slice(&buf[OFF_ENV..OFF_ENV + ENVELOPE_SIZE]);
        e.env.encode(&mut env_buf);
        buf[OFF_ENV..OFF_ENV + ENVELOPE_SIZE].copy_from_slice(&env_buf);
        buf[OFF_OFFSET..OFF_OFFSET + 8].copy_from_slice(&e.offset.to_le_bytes());
        self.w.write_all(&buf)?;
        self.w.flush()?;
        Ok(())
    }

    /// Cut the epoch: flush, `sync_all`, truncate the log to zero
    /// length, then sync again so the empty state is durable.
    pub fn rotate(&mut self) -> Result<(), StrataError> {
        self.w.flush()?;
        let file = self.w.get_mut();
        file.sync_all()?;
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        file.sync_all()?;
        Ok(())
    }

    /// Replay the log: parse 64-byte entries from the start.
    ///
    /// A torn tail (fewer than [`ENTRY_SIZE`] bytes left) or a bad
    /// envelope ends the replay early; the damaged suffix is discarded
    /// and the entries parsed so far are returned. This makes replay
    /// crash-tolerant: only the incomplete tail of the last epoch is
    /// ever lost.
    pub fn replay(&self) -> Result<Vec<EpochEntry>, StrataError> {
        let mut file = File::open(&self.path)?;
        let mut data = Vec::new();
        file.read_to_end(&mut data)?;

        let mut entries = Vec::with_capacity(data.len() / ENTRY_SIZE);
        for chunk in data.chunks_exact(ENTRY_SIZE) {
            let seg_id =
                u32::from_le_bytes(chunk[OFF_SEG_ID..OFF_SEG_ID + 4].try_into().unwrap());
            let env_bytes: &[u8; ENVELOPE_SIZE] = chunk[OFF_ENV..OFF_ENV + ENVELOPE_SIZE]
                .try_into()
                .unwrap();
            let env = match Envelope::decode(env_bytes) {
                Ok(env) => env,
                Err(_) => break, // torn/corrupt entry: drop the tail
            };
            let offset =
                u64::from_le_bytes(chunk[OFF_OFFSET..OFF_OFFSET + 8].try_into().unwrap());
            entries.push(EpochEntry { seg_id, env, offset });
        }
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_env() -> Envelope {
        Envelope {
            record_ver: 1,
            type_id: 0,
            comp_id: 0,
            chunk_x: 5,
            chunk_z: 6,
            gen: 9,
            epoch_ts: 0,
            payload_len: 3,
            payload_hash: 7,
        }
    }

    #[test]
    fn record_rotate_replay_cycle() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = EpochLog::open(dir.path()).unwrap();
        log.record(&EpochEntry {
            seg_id: 2,
            env: sample_env(),
            offset: 100,
        })
        .unwrap();
        log.record(&EpochEntry {
            seg_id: 2,
            env: sample_env(),
            offset: 200,
        })
        .unwrap();
        assert_eq!(log.replay().unwrap().len(), 2);
        log.rotate().unwrap();
        assert!(log.replay().unwrap().is_empty());
    }

    #[test]
    fn torn_tail_entry_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = EpochLog::open(dir.path()).unwrap();
        log.record(&EpochEntry {
            seg_id: 1,
            env: sample_env(),
            offset: 16,
        })
        .unwrap();
        drop(log);
        // 模拟崩溃：追加 10 字节垃圾（不足一条 64B）
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(dir.path().join("current.velog"))
            .unwrap()
            .write_all(&[0xAA; 10])
            .unwrap();
        let log = EpochLog::open(dir.path()).unwrap();
        assert_eq!(log.replay().unwrap().len(), 1); // 坏尾丢弃，好条目保留
    }
}
