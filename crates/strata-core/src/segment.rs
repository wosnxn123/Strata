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

use xxhash_rust::xxh64::xxh64;

/// 扫描恢复出的单条记录。
#[derive(Debug, Clone)]
pub struct ScannedRecord {
    pub env: Envelope,
    /// 信封头在文件中的偏移。
    pub offset: u64,
    pub payload: Vec<u8>,
}

/// 段文件扫描结果。
#[derive(Debug, Default)]
pub struct ScanResult {
    pub records: Vec<ScannedRecord>,
    /// 尾部坏/不足（崩溃容忍）。
    pub truncated_tail: bool,
    /// 中部坏区经 magic 重同步找回的次数。
    pub resync_count: u32,
}

/// 中部坏区重同步时向前搜索 MAGIC 的最大距离。
const RESYNC_WINDOW: usize = 64 * 1024;

/// 扫描段文件重建全部记录（恢复/verify 专用路径）。
///
/// 契约：
/// 1. 文件头 16B 校验 [`SEG_MAGIC`]（坏 → `Corrupt`）。
/// 2. 顺序读 40B 头 → [`Envelope::decode`] → 读 `payload_len` 字节负载。
/// 3. `xxh64(payload)`（seed 0）与 `env.payload_hash` 不符 → 该记录
///    `payload_hash` 置 0 保留，扫描继续。
/// 4. 剩余字节不足 40 或不足 `40 + payload_len`（文件尾部）→
///    `truncated_tail = true`，停止。
/// 5. 头部 decode 失败但不在尾部（中部坏）→ 向前最多 64KB 搜索下一个
///    信封 MAGIC（`b"VSEG"`）重同步：找到 → `resync_count += 1` 继续扫描；
///    找不到 → `Err(Corrupt)`。
pub fn scan_segment(path: &Path) -> Result<ScanResult, StrataError> {
    let corrupt = |detail: String| StrataError::Corrupt {
        path: path.display().to_string(),
        detail,
    };

    // 段文件默认 ≤64MiB，整读入内存解析。
    let data = std::fs::read(path)?;

    if data.len() < SEG_HEADER_SIZE as usize || data[0..4] != SEG_MAGIC {
        return Err(corrupt("bad segment header".into()));
    }

    let mut result = ScanResult::default();
    let mut pos = SEG_HEADER_SIZE as usize;

    while pos < data.len() {
        // 尾部剩余不足一个信封 → 崩溃截断，容忍。
        if data.len() - pos < ENVELOPE_SIZE {
            result.truncated_tail = true;
            break;
        }

        let mut env =
            match Envelope::decode(data[pos..pos + ENVELOPE_SIZE].try_into().unwrap()) {
                Ok(env) => env,
                Err(_) => {
                    // 中部坏：在 64KB 窗口内向前搜索下一个信封 MAGIC 重同步。
                    let window_end = (pos + RESYNC_WINDOW).min(data.len() - 4);
                    let found = (pos + 1..=window_end)
                        .find(|&s| data[s..s + 4] == crate::envelope::MAGIC);
                    match found {
                        Some(s) => {
                            result.resync_count += 1;
                            pos = s;
                            continue;
                        }
                        None => return Err(corrupt(format!("lost sync at offset {pos}"))),
                    }
                }
            };

        let total = ENVELOPE_SIZE as u64 + env.payload_len as u64;
        if ((data.len() - pos) as u64) < total {
            // 尾部负载不足 → 崩溃截断，容忍。
            result.truncated_tail = true;
            break;
        }

        let payload =
            data[pos + ENVELOPE_SIZE..pos + ENVELOPE_SIZE + env.payload_len as usize].to_vec();
        if xxh64(&payload, 0) != env.payload_hash {
            env.payload_hash = 0;
        }
        result.records.push(ScannedRecord {
            offset: pos as u64,
            env,
            payload,
        });
        pos += total as usize;
    }

    Ok(result)
}

#[cfg(test)]
mod scan_tests {
    use super::*;

    fn mk_env(payload: &[u8], hash: u64) -> Envelope {
        Envelope {
            record_ver: 1,
            type_id: 0,
            comp_id: 0,
            chunk_x: 3,
            chunk_z: 4,
            gen: 9,
            epoch_ts: 17,
            payload_len: payload.len() as u32,
            payload_hash: hash,
        }
    }

    #[test]
    fn scan_roundtrip_two_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("seg-rt.vseg");
        let p1: &[u8] = b"payload-one";
        let p2: &[u8] = b"pp";

        let mut w = SegmentWriter::create(&path, 1).unwrap();
        let o1 = w.append(&mk_env(p1, xxh64(p1, 0)), p1).unwrap();
        let o2 = w.append(&mk_env(p2, xxh64(p2, 0)), p2).unwrap();
        w.close().unwrap();

        let r = scan_segment(&path).unwrap();
        assert_eq!(r.records.len(), 2);
        assert!(!r.truncated_tail);
        assert_eq!(r.resync_count, 0);
        assert_eq!(r.records[0].offset, o1);
        assert_eq!(r.records[1].offset, o2);
        assert_eq!(r.records[0].payload, p1);
        assert_eq!(r.records[1].payload, p2);
        assert_eq!(r.records[0].env.payload_hash, xxh64(p1, 0));
        assert_eq!(r.records[1].env, mk_env(p2, xxh64(p2, 0)));
    }

    #[test]
    fn truncated_tail_tolerated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("seg-trunc.vseg");
        let p1: &[u8] = b"payload-one";
        let p2: &[u8] = b"second";

        let mut w = SegmentWriter::create(&path, 2).unwrap();
        w.append(&mk_env(p1, xxh64(p1, 0)), p1).unwrap();
        w.append(&mk_env(p2, xxh64(p2, 0)), p2).unwrap();
        w.close().unwrap();

        // 砍掉文件尾 3 字节：记录 1 完整，记录 2 尾部截断。
        let len = std::fs::metadata(&path).unwrap().len();
        let f = OpenOptions::new().write(true).open(&path).unwrap();
        f.set_len(len - 3).unwrap();
        drop(f);

        let r = scan_segment(&path).unwrap();
        assert_eq!(r.records.len(), 1);
        assert_eq!(r.records[0].payload, p1);
        assert!(r.truncated_tail);
        assert_eq!(r.resync_count, 0);
    }

    #[test]
    fn hash_mismatch_flagged_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("seg-badhash.vseg");
        let p: &[u8] = b"payload-one";

        let mut w = SegmentWriter::create(&path, 3).unwrap();
        // 故意填错误的 payload_hash。
        w.append(&mk_env(p, 0), p).unwrap();
        w.close().unwrap();

        let r = scan_segment(&path).unwrap();
        assert_eq!(r.records.len(), 1);
        assert_eq!(r.records[0].payload, p);
        assert_eq!(r.records[0].env.payload_hash, 0);
        assert!(!r.truncated_tail);
    }

    #[test]
    fn resync_after_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("seg-garbage.vseg");
        let p1: &[u8] = b"payload-one";
        let p2: &[u8] = b"tail-record";

        // 手工拼字节：16B 段头 + 记录1 + 100B 垃圾 + 记录2。
        let mut data = Vec::new();
        data.extend_from_slice(&SEG_MAGIC);
        data.extend_from_slice(&4u32.to_le_bytes());
        data.extend_from_slice(&[0u8; 8]);

        let mut env1 = [0u8; ENVELOPE_SIZE];
        mk_env(p1, xxh64(p1, 0)).encode(&mut env1);
        data.extend_from_slice(&env1);
        data.extend_from_slice(p1);

        data.extend_from_slice(&[0xFFu8; 100]);

        let mut env2 = [0u8; ENVELOPE_SIZE];
        mk_env(p2, xxh64(p2, 0)).encode(&mut env2);
        data.extend_from_slice(&env2);
        data.extend_from_slice(p2);

        std::fs::write(&path, &data).unwrap();

        let r = scan_segment(&path).unwrap();
        assert_eq!(r.records.len(), 2);
        assert_eq!(r.records[0].payload, p1);
        assert_eq!(r.records[1].payload, p2);
        assert_eq!(r.resync_count, 1);
        assert!(!r.truncated_tail);
        assert_eq!(r.records[1].offset as usize, 16 + ENVELOPE_SIZE + p1.len() + 100);
    }
}
