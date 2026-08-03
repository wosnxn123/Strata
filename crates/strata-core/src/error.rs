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
}
