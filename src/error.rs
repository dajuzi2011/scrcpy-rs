//! 统一错误类型。故意保持很小，不引入 anyhow/thiserror。

use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    /// 普通错误信息
    Msg(String),
    Io(std::io::Error),
    Win(windows::core::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Msg(m) => f.write_str(m),
            Error::Io(e) => write!(f, "{e}"),
            Error::Win(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<windows::core::Error> for Error {
    fn from(e: windows::core::Error) -> Self {
        Error::Win(e)
    }
}

/// 快速构造一个 Msg 错误。支持 format 语法。
#[macro_export]
macro_rules! bail {
    ($($arg:tt)*) => {
        return Err($crate::error::Error::Msg(format!($($arg)*)))
    };
}

/// 构造错误但不返回。
#[macro_export]
macro_rules! errmsg {
    ($($arg:tt)*) => {
        $crate::error::Error::Msg(format!($($arg)*))
    };
}
