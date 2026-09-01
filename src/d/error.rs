//! 统一错误类型：把 IO 错误、协议错误、数据文件损坏等归一处理。

use std::fmt;

#[derive(Debug)]
pub enum Error {
    /// 文件、网络等底层 IO 错误
    Io(std::io::Error),
    /// 客户端命令不合法（未知命令、参数个数错误等）
    Protocol(String),
    /// 持久化文件内容损坏或格式非法
    Corrupt(String),
    /// 服务器内部状态不可安全使用
    Internal(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "IO 错误: {e}"),
            Error::Protocol(m) => write!(f, "命令错误: {m}"),
            Error::Corrupt(m) => write!(f, "数据文件损坏: {m}"),
            Error::Internal(m) => write!(f, "内部错误: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}
