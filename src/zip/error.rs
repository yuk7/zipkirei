use std::fmt;
use std::io;

#[derive(Debug)]
pub enum Error {
    /// ファイルI/Oに関するエラー
    Io {
        context: Option<String>,
        source: io::Error,
    },
    /// ZIPアーカイブのフォーマットやシグネチャが不正な場合
    InvalidArchive(ArchiveError),
    /// 分割ZIPなど、サポート対象外の機能が検出された場合
    Unsupported(UnsupportedFeature),
    /// ファイル名が不正なUTF-8シーケンスの場合（CLIで特別なヒントを出すために利用）
    NonUtf8Filename { entry_no: u64 },
    /// 各種サイズ制限を超過した場合
    LimitExceeded(LimitError),
    /// 汎用のエラーメッセージ（移行用のフォールバック）
    Message(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArchiveError {
    TooSmall,
    EocdNotFound,
    UnexpectedEof {
        part: String,
        entry_no: Option<u64>,
    },
    InvalidSignature {
        record: &'static str,
        entry_no: Option<u64>,
        offset: Option<u64>,
    },
    CentralDirectoryRange {
        offset: u64,
        size: u64,
        file_len: u64,
    },
    CentralDirectoryOverlap {
        offset: u64,
        size: u64,
        limit: u64,
    },
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnsupportedFeature {
    MultiDisk,
    MultiDiskZip64,
    EntryCountMismatch,
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LimitError {
    Value { context: String, value: u64 },
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Self::Io {
            context: None,
            source: err,
        }
    }
}

impl From<String> for Error {
    fn from(msg: String) -> Self {
        Self::Message(msg)
    }
}

impl From<&str> for Error {
    fn from(msg: &str) -> Self {
        Self::Message(msg.to_string())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                context: Some(context),
                source,
            } => write!(f, "I/O error: {}: {}", context, source),
            Self::Io {
                context: None,
                source,
            } => write!(f, "I/O error: {}", source),
            Self::InvalidArchive(msg) => write!(f, "Invalid ZIP archive: {}", msg),
            Self::Unsupported(msg) => write!(f, "Unsupported ZIP feature: {}", msg),
            Self::NonUtf8Filename { entry_no } => {
                write!(f, "entry {} has a non-UTF-8 filename; rerun with --not-utf-8 to leave filename bytes and bit 11 unchanged", entry_no)
            }
            Self::LimitExceeded(msg) => write!(f, "ZIP limit exceeded: {}", msg),
            Self::Message(msg) => write!(f, "{}", msg),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl Clone for Error {
    fn clone(&self) -> Self {
        match self {
            Self::Io { context, source } => Self::Io {
                context: context.clone(),
                source: io::Error::new(source.kind(), source.to_string()),
            },
            Self::InvalidArchive(msg) => Self::InvalidArchive(msg.clone()),
            Self::Unsupported(msg) => Self::Unsupported(msg.clone()),
            Self::NonUtf8Filename { entry_no } => Self::NonUtf8Filename {
                entry_no: *entry_no,
            },
            Self::LimitExceeded(msg) => Self::LimitExceeded(msg.clone()),
            Self::Message(msg) => Self::Message(msg.clone()),
        }
    }
}

impl PartialEq for Error {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Io {
                    context: a_context,
                    source: a_source,
                },
                Self::Io {
                    context: b_context,
                    source: b_source,
                },
            ) => {
                a_context == b_context
                    && a_source.kind() == b_source.kind()
                    && a_source.to_string() == b_source.to_string()
            }
            (Self::InvalidArchive(a), Self::InvalidArchive(b)) => a == b,
            (Self::Unsupported(a), Self::Unsupported(b)) => a == b,
            (Self::NonUtf8Filename { entry_no: a }, Self::NonUtf8Filename { entry_no: b }) => {
                a == b
            }
            (Self::LimitExceeded(a), Self::LimitExceeded(b)) => a == b,
            (Self::Message(a), Self::Message(b)) => a == b,
            _ => false,
        }
    }
}

impl Eq for Error {}

impl Error {
    pub fn io_context(context: impl Into<String>, source: io::Error) -> Self {
        Self::Io {
            context: Some(context.into()),
            source,
        }
    }

    pub fn invalid_archive(message: impl Into<String>) -> Self {
        Self::InvalidArchive(ArchiveError::Other(message.into()))
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::Unsupported(UnsupportedFeature::Other(message.into()))
    }

    pub fn limit_exceeded(message: impl Into<String>) -> Self {
        Self::LimitExceeded(LimitError::Other(message.into()))
    }

    pub fn contains(&self, pat: &str) -> bool {
        self.to_string().contains(pat)
    }

    fn eq_str(&self, other: &str) -> bool {
        match self {
            Self::Io { source, .. } => source.to_string() == other || self.to_string() == other,
            Self::InvalidArchive(msg) => msg.to_string() == other || self.to_string() == other,
            Self::Unsupported(msg) => msg.to_string() == other || self.to_string() == other,
            Self::LimitExceeded(msg) => msg.to_string() == other || self.to_string() == other,
            Self::Message(msg) => msg == other || self.to_string() == other,
            Self::NonUtf8Filename { .. } => self.to_string() == other,
        }
    }
}

impl PartialEq<&str> for Error {
    fn eq(&self, other: &&str) -> bool {
        self.eq_str(*other)
    }
}

impl PartialEq<Error> for &str {
    fn eq(&self, other: &Error) -> bool {
        other.eq_str(self)
    }
}

impl PartialEq<String> for Error {
    fn eq(&self, other: &String) -> bool {
        self.eq_str(other)
    }
}

impl PartialEq<Error> for String {
    fn eq(&self, other: &Error) -> bool {
        other.eq_str(self)
    }
}

impl fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooSmall => write!(f, "file is too small to be a valid ZIP archive"),
            Self::EocdNotFound => {
                write!(
                    f,
                    "End of Central Directory record not found; not a valid ZIP archive"
                )
            }
            Self::UnexpectedEof { part, entry_no } => match entry_no {
                Some(entry_no) => {
                    write!(f, "unexpected EOF reading {} at entry {}", part, entry_no)
                }
                None => write!(f, "unexpected EOF reading {}", part),
            },
            Self::InvalidSignature {
                record,
                entry_no,
                offset,
            } => match (entry_no, offset) {
                (Some(entry_no), Some(offset)) => write!(
                    f,
                    "invalid {} signature at entry {} (offset {:#x})",
                    record, entry_no, offset
                ),
                (Some(entry_no), None) => {
                    write!(f, "invalid {} signature at entry {}", record, entry_no)
                }
                (None, Some(offset)) => {
                    write!(f, "invalid {} signature at offset {:#x}", record, offset)
                }
                (None, None) => write!(f, "invalid {} signature", record),
            },
            Self::CentralDirectoryRange {
                offset,
                size,
                file_len,
            } => write!(
                f,
                "Central Directory range exceeds file length: offset {}, size {}, file length {}",
                offset, size, file_len
            ),
            Self::CentralDirectoryOverlap {
                offset,
                size,
                limit,
            } => write!(
                f,
                "Central Directory overlaps end records: offset {}, size {}, limit {}",
                offset, size, limit
            ),
            Self::Other(msg) => write!(f, "{}", msg),
        }
    }
}

impl fmt::Display for UnsupportedFeature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MultiDisk => write!(f, "multi-disk ZIP archives are not supported"),
            Self::MultiDiskZip64 => write!(f, "multi-disk ZIP64 archives are not supported"),
            Self::EntryCountMismatch => {
                write!(f, "entry count mismatch; multi-disk ZIP may be unsupported")
            }
            Self::Other(msg) => write!(f, "{}", msg),
        }
    }
}

impl fmt::Display for LimitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Value { context, value } => write!(f, "{}: {}", context, value),
            Self::Other(msg) => write!(f, "{}", msg),
        }
    }
}
