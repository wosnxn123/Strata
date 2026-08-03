//! 段文件追加写入器。
//!
//! 文件格式：16B 文件头（magic `VS01` + seg_id u32 LE + 8B 全零 reserved）
//! 后接紧排的信封序列（每条 = 40B 信封 + payload_len 字节负载，无对齐填充）。

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;

use crate::envelope::{Envelope, ENVELOPE_SIZE};
use crate::StrataError;

/// 段文件头大小：4B magic + 4B seg_id + 8B reserved。
pub const SEG_HEADER_SIZE: u64 = 16;
/// 段文件 magic。
pub const SEG_MAGIC: [u8; 4] = *b"VS01";

/// 追加式段文件写入器。`offset` 指向下一条记录的写入位置。
pub struct SegmentWriter {
    w: BufWriter<File>,
    offset: u64,
}

impl SegmentWriter {
    /// 创建新段文件并写入 16B 文件头。`create_new`：文件已存在则返回 Err。
    pub fn create(path: &Path, seg_id: u32) -> Result<Self, StrataError> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        let mut w = BufWriter::new(file);

        let mut header = [0u8; SEG_HEADER_SIZE as usize];
        header[0..4].copy_from_slice(&SEG_MAGIC);
        header[4..8].copy_from_slice(&seg_id.to_le_bytes());
        // 8..16 保持全零（reserved）
        w.write_all(&header)?;

        Ok(Self {
            w,
            offset: SEG_HEADER_SIZE,
        })
    }

    /// 追加一条记录（信封 + 负载），返回该信封头的文件偏移。
    /// `env.payload_len` 必须等于 `payload.len()`。
    pub fn append(&mut self, env: &Envelope, payload: &[u8]) -> Result<u64, StrataError> {
        debug_assert_eq!(
            env.payload_len as usize,
            payload.len(),
            "envelope payload_len must match payload buffer length"
        );

        let start = self.offset;

        let mut buf = [0u8; ENVELOPE_SIZE];
        env.encode(&mut buf);
        self.w.write_all(&buf)?;
        self.w.write_all(payload)?;

        self.offset += ENVELOPE_SIZE as u64 + payload.len() as u64;
        Ok(start)
    }

    /// 下一条记录的写入偏移（= 当前文件逻辑长度）。
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// flush 并 fsync 底层文件。
    pub fn fsync(&mut self) -> Result<(), StrataError> {
        self.w.flush()?;
        self.w.get_ref().sync_all()?;
        Ok(())
    }

    /// flush 并关闭写入器。
    pub fn close(mut self) -> Result<(), StrataError> {
        self.w.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(payload_len: u32) -> Envelope {
        Envelope {
            record_ver: 1,
            type_id: 0,
            comp_id: 0,
            chunk_x: 1,
            chunk_z: 2,
            gen: 1,
            epoch_ts: 0,
            payload_len,
            payload_hash: 0,
        }
    }

    #[test]
    fn append_returns_correct_offsets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("seg-0001.vseg");
        let mut w = SegmentWriter::create(&path, 1).unwrap();
        assert_eq!(w.offset(), 16);
        let o1 = w.append(&env(5), b"AAAAA").unwrap();
        assert_eq!(o1, 16);
        let o2 = w.append(&env(3), b"BBB").unwrap();
        assert_eq!(o2, 16 + ENVELOPE_SIZE as u64 + 5);
        w.close().unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            16 + 2 * ENVELOPE_SIZE as u64 + 8
        );
    }

    #[test]
    fn create_fails_if_exists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("seg-0002.vseg");
        let w = SegmentWriter::create(&path, 2).unwrap();
        w.close().unwrap();
        assert!(SegmentWriter::create(&path, 2).is_err());
    }
}
