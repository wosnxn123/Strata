//! 影子双副本 manifest（`manifest.vsm` + `manifest.vsm.bak`）。
//!
//! 磁盘文件 = 8B `xxh64(body, seed=0)`（小端）+ body。body 布局（全小端）：
//!
//! ```text
//! format_version u32 | epoch u64 | next_gen u64 | next_seg_id u32
//! seg_count u32 → 每段：id u32 | live u64 | total u64 | bucket u8 | created_epoch u64 | last_rewrite u64
//! cold_count u32 → 每冷：region_x i32 | region_z i32 | invalid u32 | total u32
//! bitmap_count u32 → 每条：x i32 | z i32 | 384B 原始位图
//! dict_count u32 → 每槽：type_id u16 | len u32 | 字典字节
//! ```
//!
//! 保存流程：写 `manifest.vsm.tmp` + fsync → 旧主副本（若存在）rename 为
//! `manifest.vsm.bak` → tmp rename 为主副本。加载先校验主副本哈希，失败回落 `.bak`。

use std::fs::File;
use std::io::Write;
use std::path::Path;

use xxhash_rust::xxh64::xxh64;

use crate::StrataError;

/// 主 manifest 文件名。
pub const MANIFEST_FILE: &str = "manifest.vsm";
/// 备份 manifest 文件名（上一份主副本）。
pub const MANIFEST_BAK_FILE: &str = "manifest.vsm.bak";
/// 写入用临时文件名。
const MANIFEST_TMP_FILE: &str = "manifest.vsm.tmp";
/// 唯一支持的格式版本。
pub const FORMAT_VERSION: u32 = 2;
/// 每个 region 位图快照的字节数。
pub const REGION_BITMAP_BYTES: usize = 384;
/// 字典槽数量上限。
pub const MAX_DICT_SLOTS: usize = 16;

/// 分层桶。序列化为 u8：0=Young，1=Active，2=Stable。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bucket {
    Young,
    Active,
    Stable,
}

impl Bucket {
    fn encode(self) -> u8 {
        match self {
            Bucket::Young => 0,
            Bucket::Active => 1,
            Bucket::Stable => 2,
        }
    }

    fn decode(v: u8) -> Result<Self, StrataError> {
        match v {
            0 => Ok(Bucket::Young),
            1 => Ok(Bucket::Active),
            2 => Ok(Bucket::Stable),
            _ => Err(StrataError::Manifest(format!("bad bucket value {v}"))),
        }
    }
}

/// 单个段文件的元数据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentMeta {
    pub id: u32,
    pub live_bytes: u64,
    pub total_bytes: u64,
    pub bucket: Bucket,
    pub created_epoch: u64,
    pub last_rewrite_epoch: u64,
}

/// 冷区元数据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColdMeta {
    pub region_x: i32,
    pub region_z: i32,
    pub invalid_count: u32,
    pub total_slots: u32,
}

/// region 坐标键。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegionKey {
    pub x: i32,
    pub z: i32,
}

/// 全量 manifest 快照。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Manifest {
    /// 格式版本，恒为 [`FORMAT_VERSION`]。
    pub format_version: u32,
    pub epoch: u64,
    pub next_gen: u64,
    pub next_seg_id: u32,
    pub segments: Vec<SegmentMeta>,
    pub cold: Vec<ColdMeta>,
    /// 每个 region 一份 [`REGION_BITMAP_BYTES`] 字节的位图快照。
    pub region_bitmaps: Vec<(RegionKey, Vec<u8>)>,
    /// (type_id, 字典内容)，至多 [`MAX_DICT_SLOTS`] 槽。
    pub dict_slots: Vec<(u16, Vec<u8>)>,
}

impl Manifest {
    /// 保存：序列化 body → 前置 xxh64 → 写 tmp + fsync → 旧主副本降级为
    /// `.bak` → tmp 升级为主副本。
    pub fn save(&self, dir: &Path) -> Result<(), StrataError> {
        if self.format_version != FORMAT_VERSION {
            return Err(StrataError::Manifest(format!(
                "unsupported format_version {} (expected {FORMAT_VERSION})",
                self.format_version
            )));
        }
        for (_, bm) in &self.region_bitmaps {
            if bm.len() != REGION_BITMAP_BYTES {
                return Err(StrataError::Manifest(format!(
                    "region bitmap must be {REGION_BITMAP_BYTES} bytes, got {}",
                    bm.len()
                )));
            }
        }
        if self.dict_slots.len() > MAX_DICT_SLOTS {
            return Err(StrataError::Manifest(format!(
                "too many dict slots: {} > {MAX_DICT_SLOTS}",
                self.dict_slots.len()
            )));
        }

        let body = self.encode_body();
        let mut buf = Vec::with_capacity(8 + body.len());
        buf.extend_from_slice(&xxh64(&body, 0).to_le_bytes());
        buf.extend_from_slice(&body);

        let tmp = dir.join(MANIFEST_TMP_FILE);
        let main = dir.join(MANIFEST_FILE);
        let bak = dir.join(MANIFEST_BAK_FILE);

        // create+truncate：上一次崩溃残留的 tmp 直接覆盖。
        let mut f = File::create(&tmp)?;
        f.write_all(&buf)?;
        f.sync_all()?;

        if main.exists() {
            // Windows 上 rename 到已存在的目标会失败，先清掉旧 bak。
            if bak.exists() {
                std::fs::remove_file(&bak)?;
            }
            std::fs::rename(&main, &bak)?;
        }
        std::fs::rename(&tmp, &main)?;
        Ok(())
    }

    /// 加载：主副本哈希校验通过→用主；否则尝试 `.bak`；副本存在但都坏→
    /// `Err(Manifest("corrupt"))`；两个都不存在→`Ok(None)`。
    pub fn load(dir: &Path) -> Result<Option<Manifest>, StrataError> {
        let main = dir.join(MANIFEST_FILE);
        let bak = dir.join(MANIFEST_BAK_FILE);

        let mut any_exists = false;
        for path in [&main, &bak] {
            if !path.exists() {
                continue;
            }
            any_exists = true;
            let bytes = std::fs::read(path)?;
            if let Ok(m) = parse_file(&bytes) {
                return Ok(Some(m));
            }
        }
        if any_exists {
            Err(StrataError::Manifest("corrupt".into()))
        } else {
            Ok(None)
        }
    }

    /// 序列化 body（不含前置 8B 哈希）。
    fn encode_body(&self) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&self.format_version.to_le_bytes());
        b.extend_from_slice(&self.epoch.to_le_bytes());
        b.extend_from_slice(&self.next_gen.to_le_bytes());
        b.extend_from_slice(&self.next_seg_id.to_le_bytes());

        b.extend_from_slice(&(self.segments.len() as u32).to_le_bytes());
        for s in &self.segments {
            b.extend_from_slice(&s.id.to_le_bytes());
            b.extend_from_slice(&s.live_bytes.to_le_bytes());
            b.extend_from_slice(&s.total_bytes.to_le_bytes());
            b.push(s.bucket.encode());
            b.extend_from_slice(&s.created_epoch.to_le_bytes());
            b.extend_from_slice(&s.last_rewrite_epoch.to_le_bytes());
        }

        b.extend_from_slice(&(self.cold.len() as u32).to_le_bytes());
        for c in &self.cold {
            b.extend_from_slice(&c.region_x.to_le_bytes());
            b.extend_from_slice(&c.region_z.to_le_bytes());
            b.extend_from_slice(&c.invalid_count.to_le_bytes());
            b.extend_from_slice(&c.total_slots.to_le_bytes());
        }

        b.extend_from_slice(&(self.region_bitmaps.len() as u32).to_le_bytes());
        for (k, bm) in &self.region_bitmaps {
            b.extend_from_slice(&k.x.to_le_bytes());
            b.extend_from_slice(&k.z.to_le_bytes());
            b.extend_from_slice(bm);
        }

        b.extend_from_slice(&(self.dict_slots.len() as u32).to_le_bytes());
        for (type_id, dict) in &self.dict_slots {
            b.extend_from_slice(&type_id.to_le_bytes());
            b.extend_from_slice(&(dict.len() as u32).to_le_bytes());
            b.extend_from_slice(dict);
        }
        b
    }
}

/// 校验 8B 前缀哈希后反序列化 body。
fn parse_file(bytes: &[u8]) -> Result<Manifest, StrataError> {
    if bytes.len() < 8 {
        return Err(StrataError::Manifest("manifest truncated".into()));
    }
    let (hash, body) = bytes.split_at(8);
    let stored = u64::from_le_bytes(hash.try_into().unwrap());
    if stored != xxh64(body, 0) {
        return Err(StrataError::Manifest("manifest hash mismatch".into()));
    }
    decode_body(body)
}

/// 游标式读取器：所有取值都做边界检查，越界→`Err(Manifest(..))`。
struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], StrataError> {
        if n > self.b.len() - self.pos {
            return Err(StrataError::Manifest("manifest truncated".into()));
        }
        let s = &self.b[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8, StrataError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, StrataError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn u32(&mut self) -> Result<u32, StrataError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64, StrataError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn i32(&mut self) -> Result<i32, StrataError> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    /// 要求 body 恰好读完，拒绝尾部多余字节。
    fn finish(&self) -> Result<(), StrataError> {
        if self.pos != self.b.len() {
            return Err(StrataError::Manifest("manifest trailing bytes".into()));
        }
        Ok(())
    }
}

/// 按文件格式反序列化 body。
fn decode_body(body: &[u8]) -> Result<Manifest, StrataError> {
    let mut r = Reader::new(body);

    let format_version = r.u32()?;
    if format_version != FORMAT_VERSION {
        return Err(StrataError::Manifest(format!(
            "unsupported format_version {format_version} (expected {FORMAT_VERSION})"
        )));
    }
    let epoch = r.u64()?;
    let next_gen = r.u64()?;
    let next_seg_id = r.u32()?;

    let seg_count = r.u32()?;
    let mut segments = Vec::new();
    for _ in 0..seg_count {
        segments.push(SegmentMeta {
            id: r.u32()?,
            live_bytes: r.u64()?,
            total_bytes: r.u64()?,
            bucket: Bucket::decode(r.u8()?)?,
            created_epoch: r.u64()?,
            last_rewrite_epoch: r.u64()?,
        });
    }

    let cold_count = r.u32()?;
    let mut cold = Vec::new();
    for _ in 0..cold_count {
        cold.push(ColdMeta {
            region_x: r.i32()?,
            region_z: r.i32()?,
            invalid_count: r.u32()?,
            total_slots: r.u32()?,
        });
    }

    let bitmap_count = r.u32()?;
    let mut region_bitmaps = Vec::new();
    for _ in 0..bitmap_count {
        let x = r.i32()?;
        let z = r.i32()?;
        let bm = r.take(REGION_BITMAP_BYTES)?.to_vec();
        region_bitmaps.push((RegionKey { x, z }, bm));
    }

    let dict_count = r.u32()?;
    if dict_count as usize > MAX_DICT_SLOTS {
        return Err(StrataError::Manifest(format!(
            "too many dict slots: {dict_count} > {MAX_DICT_SLOTS}"
        )));
    }
    let mut dict_slots = Vec::new();
    for _ in 0..dict_count {
        let type_id = r.u16()?;
        let len = r.u32()? as usize;
        let dict = r.take(len)?.to_vec();
        dict_slots.push((type_id, dict));
    }

    r.finish()?;
    Ok(Manifest {
        format_version,
        epoch,
        next_gen,
        next_seg_id,
        segments,
        cold,
        region_bitmaps,
        dict_slots,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Manifest {
        Manifest {
            format_version: 2,
            epoch: 3,
            next_gen: 42,
            next_seg_id: 5,
            segments: vec![SegmentMeta {
                id: 1,
                live_bytes: 100,
                total_bytes: 200,
                bucket: Bucket::Young,
                created_epoch: 1,
                last_rewrite_epoch: 2,
            }],
            cold: vec![ColdMeta {
                region_x: -1,
                region_z: 2,
                invalid_count: 0,
                total_slots: 1024,
            }],
            region_bitmaps: vec![(RegionKey { x: 0, z: 0 }, vec![0xAB; 384])],
            dict_slots: vec![(0, vec![1, 2, 3])],
        }
    }

    #[test]
    fn save_load_roundtrip_and_failover() {
        let dir = tempfile::tempdir().unwrap();
        let m = sample();
        m.save(dir.path()).unwrap();
        assert_eq!(Manifest::load(dir.path()).unwrap().unwrap(), m);
        // 损坏主副本 → 自动切 .bak（save 两次后主坏切 bak 应还原第一次内容）
        let m2 = {
            let mut m = sample();
            m.epoch = 99;
            m
        };
        m2.save(dir.path()).unwrap();
        let p = dir.path().join("manifest.vsm");
        let mut bytes = std::fs::read(&p).unwrap();
        bytes[10] ^= 0xFF;
        std::fs::write(&p, bytes).unwrap();
        assert_eq!(Manifest::load(dir.path()).unwrap().unwrap(), m); // 回落到 bak（第一次内容）
    }

    #[test]
    fn empty_dir_loads_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Manifest::load(dir.path()).unwrap().is_none());
    }

    #[test]
    fn both_copies_corrupt_errors() {
        let dir = tempfile::tempdir().unwrap();
        let m = sample();
        m.save(dir.path()).unwrap();
        // 第二次 save：旧主副本降级为 .bak，两份副本同时在场。
        let m2 = {
            let mut m = sample();
            m.epoch = 99;
            m
        };
        m2.save(dir.path()).unwrap();

        let p = dir.path().join(MANIFEST_FILE);
        let bak = dir.path().join(MANIFEST_BAK_FILE);
        for path in [&p, &bak] {
            let mut bytes = std::fs::read(path).unwrap();
            bytes[10] ^= 0xFF;
            std::fs::write(path, bytes).unwrap();
        }
        assert!(Manifest::load(dir.path()).is_err());
    }

    /// 把带正确哈希的 body 直接落为主副本，驱动 decode 错误路径。
    fn write_raw(dir: &Path, body: &[u8]) {
        let mut bytes = Vec::with_capacity(8 + body.len());
        bytes.extend_from_slice(&xxh64(body, 0).to_le_bytes());
        bytes.extend_from_slice(body);
        std::fs::write(dir.join(MANIFEST_FILE), &bytes).unwrap();
    }

    #[test]
    fn decode_rejects_bad_bucket_and_truncation() {
        let dir = tempfile::tempdir().unwrap();

        // 坏 bucket 值：哈希合法但 body 非法。bucket 位于 body 偏移 48：
        // 头部 24B + seg_count 4B + id 4B + live 8B + total 8B。
        let mut body = sample().encode_body();
        body[48] = 7;
        write_raw(dir.path(), &body);
        assert!(Manifest::load(dir.path()).is_err());

        // 截断 body 尾部一字节（字典数据）→ 越界错误。
        let body = sample().encode_body();
        write_raw(dir.path(), &body[..body.len() - 1]);
        assert!(Manifest::load(dir.path()).is_err());
    }
}
