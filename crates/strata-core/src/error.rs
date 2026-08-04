use thiserror::Error;

#[derive(Debug, Error)]
pub enum StrataError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("envelope: {0}")]
    Envelope(String),
    #[error("codec: {0}")]
    Codec(String),
    #[error("corrupt at {path}: {detail}")]
    Corrupt { path: String, detail: String },
    #[error("manifest: {0}")]
    Manifest(String),
    #[error("config {file}:{line}: {detail}")]
    Config { file: String, line: u32, detail: String },
    /// `write_batch` 前缀提交语义：失败前已有 `committed` 条记录持久化。
    /// 已提交记录不会回滚，重试时调用方应跳过前 `committed` 条。
    #[error("write_batch: {committed} record(s) committed before failure: {source}")]
    BatchPartial {
        committed: u64,
        source: Box<StrataError>,
    },
    /// vstore 会话锁被其他进程/会话持有。
    #[error("lock: {0}")]
    Lock(String),
}
