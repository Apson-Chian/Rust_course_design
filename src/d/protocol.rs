//! 命令模型与文本协议。
//!
//! 协议采用「一行一条消息」的文本格式，以 `\n` 作为消息边界，UTF-8 编码：
//! - 请求：`SET 课程 Rust程序设计`、`GET 课程`、`LIST` ……
//! - 响应：`OK`、`VALUE Rust程序设计`、`NOT_FOUND`、`ERR 未知命令: FOO` ……
//!
//! 客户端与服务器共用本模块，保证双方对命令含义、参数个数和错误反馈的理解一致。

use crate::error::{Error, Result};

/// 单条消息（含换行符）的最大字节数，防止超长请求耗尽内存
pub const MAX_LINE: usize = 8 * 1024;
/// 键的最大字节数
pub const MAX_KEY: usize = 256;
/// 值的最大字节数
pub const MAX_VALUE: usize = 4 * 1024;

/// 客户端可以发起的操作
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// 写入或覆盖：`SET <key> <value>`
    Set { key: String, value: String },
    /// 带过期时间的写入：`SETEX <key> <秒> <value>`（扩展功能）
    SetEx {
        key: String,
        ttl_secs: u64,
        value: String,
    },
    /// 查询：`GET <key>`
    Get { key: String },
    /// 删除：`DEL <key>`
    Del { key: String },
    /// 列出全部键：`LIST`
    List,
    /// 查看运行状态：`STATS`
    Stats,
    /// 连通性检查：`PING`
    Ping,
    /// 日志压缩：`COMPACT`（扩展功能）
    Compact,
    /// 断开连接：`QUIT`
    Quit,
}

/// 服务器返回的结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    /// 操作成功
    Ok,
    /// 查询命中，携带值
    Value(String),
    /// 键不存在
    NotFound,
    /// 键列表（已排序）
    Keys(Vec<String>),
    /// 运行状态
    Stats(ServerStats),
    /// PING 的应答
    Pong,
    /// 退出应答
    Bye,
    /// 错误提示，连接不因此中断
    Err(String),
}

/// 服务器运行状态快照
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerStats {
    /// 当前有效键数量
    pub keys: usize,
    /// 当前在线连接数
    pub clients: usize,
    /// 已运行秒数
    pub uptime_secs: u64,
}

impl Command {
    /// 从一行文本解析命令，同时完成合法性检查。
    pub fn parse(line: &str) -> Result<Command> {
        // 只移除网络消息的行结束符和命令前导空白。SET 的值允许包含尾部空格，
        // 因此不能对整行调用 trim()。
        let line = line.trim_end_matches(['\r', '\n']);
        let line = line.trim_start();
        if line.is_empty() {
            return Err(Error::Protocol("空命令".into()));
        }
        if line.len() > MAX_LINE {
            return Err(Error::Protocol(format!("命令超长，上限 {MAX_LINE} 字节")));
        }
        // 先切出命令名，剩余部分按各命令的参数规则处理
        let (name, rest) = split_once_ws(line);
        let rest = rest.trim_start();
        match name.to_ascii_uppercase().as_str() {
            "SET" => {
                let (key, value) = split_once_ws(rest);
                let value = value.trim_start();
                validate_key(key)?;
                validate_value(value)?;
                Ok(Command::Set {
                    key: key.to_string(),
                    value: value.to_string(),
                })
            }
            "SETEX" => {
                let (key, rest) = split_once_ws(rest);
                let (ttl, value) = split_once_ws(rest.trim_start());
                let value = value.trim_start();
                validate_key(key)?;
                validate_value(value)?;
                let ttl_secs: u64 = ttl
                    .parse()
                    .map_err(|_| Error::Protocol(format!("过期时间必须是正整数秒: {ttl}")))?;
                if ttl_secs == 0 {
                    return Err(Error::Protocol("过期时间必须大于 0".into()));
                }
                Ok(Command::SetEx {
                    key: key.to_string(),
                    ttl_secs,
                    value: value.to_string(),
                })
            }
            "GET" => Ok(Command::Get {
                key: single_key("GET", rest)?,
            }),
            "DEL" => Ok(Command::Del {
                key: single_key("DEL", rest)?,
            }),
            "LIST" => no_arg("LIST", rest, Command::List),
            "STATS" => no_arg("STATS", rest, Command::Stats),
            "PING" => no_arg("PING", rest, Command::Ping),
            "COMPACT" => no_arg("COMPACT", rest, Command::Compact),
            "QUIT" | "EXIT" => no_arg("QUIT", rest, Command::Quit),
            other => Err(Error::Protocol(format!("未知命令: {other}"))),
        }
    }

    /// 序列化为一行文本（不含换行符），供客户端发送。
    pub fn encode(&self) -> String {
        match self {
            Command::Set { key, value } => format!("SET {key} {value}"),
            Command::SetEx {
                key,
                ttl_secs,
                value,
            } => format!("SETEX {key} {ttl_secs} {value}"),
            Command::Get { key } => format!("GET {key}"),
            Command::Del { key } => format!("DEL {key}"),
            Command::List => "LIST".into(),
            Command::Stats => "STATS".into(),
            Command::Ping => "PING".into(),
            Command::Compact => "COMPACT".into(),
            Command::Quit => "QUIT".into(),
        }
    }
}

impl Response {
    /// 序列化为一行文本（不含换行符）。
    pub fn encode(&self) -> String {
        match self {
            Response::Ok => "OK".into(),
            Response::Value(v) => format!("VALUE {v}"),
            Response::NotFound => "NOT_FOUND".into(),
            Response::Keys(keys) => {
                if keys.is_empty() {
                    "KEYS".into()
                } else {
                    format!("KEYS {}", keys.join(" "))
                }
            }
            Response::Stats(s) => format!(
                "STATS keys={} clients={} uptime_secs={}",
                s.keys, s.clients, s.uptime_secs
            ),
            Response::Pong => "PONG".into(),
            Response::Bye => "BYE".into(),
            Response::Err(m) => format!("ERR {m}"),
        }
    }

    /// 从一行文本解析响应，供客户端使用。
    pub fn parse(line: &str) -> Result<Response> {
        let line = line.trim_end_matches(['\r', '\n']);
        let (tag, rest) = split_once_ws(line);
        let rest = rest.trim_start();
        match tag {
            "OK" => response_no_arg("OK", rest, Response::Ok),
            "VALUE" if !rest.is_empty() => Ok(Response::Value(rest.to_string())),
            "VALUE" => Err(Error::Protocol("VALUE 响应缺少值".into())),
            "NOT_FOUND" => response_no_arg("NOT_FOUND", rest, Response::NotFound),
            "KEYS" => Ok(Response::Keys(
                rest.split_whitespace().map(String::from).collect(),
            )),
            "STATS" => parse_stats(rest).map(Response::Stats),
            "PONG" => response_no_arg("PONG", rest, Response::Pong),
            "BYE" => response_no_arg("BYE", rest, Response::Bye),
            "ERR" if !rest.is_empty() => Ok(Response::Err(rest.to_string())),
            "ERR" => Err(Error::Protocol("ERR 响应缺少错误信息".into())),
            other => Err(Error::Protocol(format!("无法识别的响应: {other}"))),
        }
    }

    /// 面向用户的中文展示文本。
    pub fn display(&self) -> String {
        match self {
            Response::Ok => "OK".into(),
            Response::Value(v) => v.clone(),
            Response::NotFound => "(空) 键不存在".into(),
            Response::Keys(keys) if keys.is_empty() => "(空) 暂无数据".into(),
            Response::Keys(keys) => keys
                .iter()
                .enumerate()
                .map(|(i, k)| format!("{}) {k}", i + 1))
                .collect::<Vec<_>>()
                .join("\n"),
            Response::Stats(s) => format!(
                "键数量: {}  在线连接: {}  运行时长: {}s",
                s.keys, s.clients, s.uptime_secs
            ),
            Response::Pong => "PONG".into(),
            Response::Bye => "已断开连接".into(),
            Response::Err(m) => format!("错误: {m}"),
        }
    }
}

/// 按第一段空白切分为「首词 + 剩余」
fn split_once_ws(s: &str) -> (&str, &str) {
    match s.find(char::is_whitespace) {
        Some(i) => (&s[..i], &s[i..]),
        None => (s, ""),
    }
}

/// 校验键：非空、无空白、不超长
pub(crate) fn validate_key(key: &str) -> Result<()> {
    if key.is_empty() {
        return Err(Error::Protocol("缺少键名".into()));
    }
    if key.len() > MAX_KEY {
        return Err(Error::Protocol(format!("键超长，上限 {MAX_KEY} 字节")));
    }
    if key.chars().any(char::is_whitespace) {
        return Err(Error::Protocol("键中不能包含空白字符".into()));
    }
    Ok(())
}

/// 校验值：非空、不含换行、不超长
pub(crate) fn validate_value(value: &str) -> Result<()> {
    if value.is_empty() {
        return Err(Error::Protocol("缺少值".into()));
    }
    if value.len() > MAX_VALUE {
        return Err(Error::Protocol(format!("值超长，上限 {MAX_VALUE} 字节")));
    }
    if value.contains('\n') || value.contains('\r') {
        return Err(Error::Protocol("值中不能包含换行符".into()));
    }
    Ok(())
}

/// 解析「只有一个键参数」的命令
fn single_key(name: &str, rest: &str) -> Result<String> {
    let (key, extra) = split_once_ws(rest);
    if !extra.trim().is_empty() {
        return Err(Error::Protocol(format!("{name} 只接受一个参数")));
    }
    validate_key(key)?;
    Ok(key.to_string())
}

/// 解析无参命令
fn no_arg(name: &str, rest: &str, cmd: Command) -> Result<Command> {
    if rest.trim().is_empty() {
        Ok(cmd)
    } else {
        Err(Error::Protocol(format!("{name} 不接受参数")))
    }
}

fn response_no_arg(tag: &str, rest: &str, response: Response) -> Result<Response> {
    if rest.is_empty() {
        Ok(response)
    } else {
        Err(Error::Protocol(format!("{tag} 响应不应包含额外内容")))
    }
}

/// 严格解析 STATS：三个字段必须各出现一次，值合法，且不接受未知字段。
fn parse_stats(s: &str) -> Result<ServerStats> {
    let mut keys = None;
    let mut clients = None;
    let mut uptime_secs = None;

    for field in s.split_whitespace() {
        let (name, value) = field
            .split_once('=')
            .ok_or_else(|| Error::Protocol(format!("STATS 字段格式非法: {field}")))?;
        match name {
            "keys" => parse_unique_field(&mut keys, name, value)?,
            "clients" => parse_unique_field(&mut clients, name, value)?,
            "uptime_secs" => parse_unique_field(&mut uptime_secs, name, value)?,
            _ => return Err(Error::Protocol(format!("STATS 包含未知字段: {name}"))),
        }
    }

    Ok(ServerStats {
        keys: keys.ok_or_else(|| Error::Protocol("STATS 缺少 keys 字段".into()))?,
        clients: clients.ok_or_else(|| Error::Protocol("STATS 缺少 clients 字段".into()))?,
        uptime_secs: uptime_secs
            .ok_or_else(|| Error::Protocol("STATS 缺少 uptime_secs 字段".into()))?,
    })
}

fn parse_unique_field<T: std::str::FromStr>(
    slot: &mut Option<T>,
    name: &str,
    value: &str,
) -> Result<()> {
    if slot.is_some() {
        return Err(Error::Protocol(format!("STATS 字段重复: {name}")));
    }
    *slot = Some(
        value
            .parse()
            .map_err(|_| Error::Protocol(format!("STATS 字段值非法: {name}={value}")))?,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic_commands() {
        assert_eq!(
            Command::parse("SET 课程 Rust程序设计").unwrap(),
            Command::Set {
                key: "课程".into(),
                value: "Rust程序设计".into()
            }
        );
        assert_eq!(
            Command::parse("get k1").unwrap(),
            Command::Get { key: "k1".into() }
        );
        assert_eq!(Command::parse("  LIST  ").unwrap(), Command::List);
        assert_eq!(Command::parse("PING").unwrap(), Command::Ping);
        assert_eq!(Command::parse("EXIT").unwrap(), Command::Quit);
    }

    #[test]
    fn value_keeps_inner_spaces() {
        let cmd = Command::parse("SET note hello  world").unwrap();
        assert_eq!(
            cmd,
            Command::Set {
                key: "note".into(),
                value: "hello  world".into()
            }
        );
    }

    #[test]
    fn value_keeps_trailing_spaces() {
        let cmd = Command::parse("  SET note hello world  \n").unwrap();
        assert_eq!(
            cmd,
            Command::Set {
                key: "note".into(),
                value: "hello world  ".into()
            }
        );
        assert_eq!(Command::parse(&cmd.encode()).unwrap(), cmd);
    }

    #[test]
    fn parse_setex() {
        assert_eq!(
            Command::parse("SETEX k 60 v").unwrap(),
            Command::SetEx {
                key: "k".into(),
                ttl_secs: 60,
                value: "v".into()
            }
        );
        assert!(Command::parse("SETEX k abc v").is_err());
        assert!(Command::parse("SETEX k 0 v").is_err());
    }

    #[test]
    fn reject_invalid_input() {
        assert!(Command::parse("").is_err()); // 空命令
        assert!(Command::parse("FOO a").is_err()); // 未知命令
        assert!(Command::parse("SET k").is_err()); // 缺少值
        assert!(Command::parse("SET").is_err()); // 缺少键和值
        assert!(Command::parse("GET a b").is_err()); // 多余参数
        assert!(Command::parse("LIST extra").is_err()); // 无参命令带参数
        assert!(Command::parse(&format!("SET k {}", "v".repeat(MAX_VALUE + 1))).is_err());
    }

    #[test]
    fn command_roundtrip() {
        for cmd in [
            Command::Set {
                key: "k".into(),
                value: "v v".into(),
            },
            Command::SetEx {
                key: "k".into(),
                ttl_secs: 5,
                value: "v".into(),
            },
            Command::Del { key: "k".into() },
            Command::Stats,
            Command::Compact,
        ] {
            assert_eq!(Command::parse(&cmd.encode()).unwrap(), cmd);
        }
    }

    #[test]
    fn response_roundtrip() {
        for resp in [
            Response::Ok,
            Response::Value("hello world".into()),
            Response::NotFound,
            Response::Keys(vec!["a".into(), "b".into()]),
            Response::Keys(vec![]),
            Response::Stats(ServerStats {
                keys: 3,
                clients: 2,
                uptime_secs: 10,
            }),
            Response::Pong,
            Response::Bye,
            Response::Err("未知命令: FOO".into()),
        ] {
            assert_eq!(Response::parse(&resp.encode()).unwrap(), resp);
        }
    }

    #[test]
    fn reject_malformed_responses() {
        for line in [
            "OK extra",
            "VALUE",
            "PONG extra",
            "ERR",
            "STATS keys=1 clients=2",
            "STATS keys=1 clients=x uptime_secs=3",
            "STATS keys=1 keys=2 clients=3 uptime_secs=4",
            "STATS keys=1 clients=2 uptime_secs=3 extra=4",
        ] {
            assert!(Response::parse(line).is_err(), "应拒绝响应: {line}");
        }
    }
}
