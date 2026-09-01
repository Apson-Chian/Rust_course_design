//! 命令行参数解析（手写解析，避免引入第三方依赖）。

use std::path::PathBuf;

/// 服务器启动参数
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// 监听地址，形如 127.0.0.1:7878
    pub addr: String,
    /// 数据文件路径（追加写日志）
    pub data_file: PathBuf,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            addr: "127.0.0.1:7878".to_string(),
            data_file: PathBuf::from("data/rkv.log"),
        }
    }
}

impl ServerConfig {
    /// 支持 `--addr <ip:port>` 和 `--data <file>`，`-h/--help` 打印用法。
    pub fn from_args<I: IntoIterator<Item = String>>(args: I) -> Result<Self, String> {
        let mut cfg = Self::default();
        let mut it = args.into_iter();
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--addr" => cfg.addr = it.next().ok_or("--addr 缺少参数")?,
                "--data" => cfg.data_file = PathBuf::from(it.next().ok_or("--data 缺少参数")?),
                "-h" | "--help" => return Err(Self::usage()),
                other => return Err(format!("未知参数: {other}\n{}", Self::usage())),
            }
        }
        Ok(cfg)
    }

    pub fn usage() -> String {
        "用法: rkv-server [--addr 127.0.0.1:7878] [--data data/rkv.log]".to_string()
    }
}

/// 客户端启动参数
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// 服务器地址
    pub addr: String,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            addr: "127.0.0.1:7878".to_string(),
        }
    }
}

impl ClientConfig {
    pub fn from_args<I: IntoIterator<Item = String>>(args: I) -> Result<Self, String> {
        let mut cfg = Self::default();
        let mut it = args.into_iter();
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--addr" => cfg.addr = it.next().ok_or("--addr 缺少参数")?,
                "-h" | "--help" => return Err(Self::usage()),
                other => return Err(format!("未知参数: {other}\n{}", Self::usage())),
            }
        }
        Ok(cfg)
    }

    pub fn usage() -> String {
        "用法: rkv-client [--addr 127.0.0.1:7878]".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_server_args() {
        let cfg = ServerConfig::from_args(
            ["--addr", "0.0.0.0:9000", "--data", "/tmp/a.log"].map(String::from),
        )
        .unwrap();
        assert_eq!(cfg.addr, "0.0.0.0:9000");
        assert_eq!(cfg.data_file, PathBuf::from("/tmp/a.log"));
    }

    #[test]
    fn reject_unknown_arg() {
        assert!(ServerConfig::from_args(["--xxx".to_string()]).is_err());
        assert!(ServerConfig::from_args(["--addr".to_string()]).is_err());
    }
}
