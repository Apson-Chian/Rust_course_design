//! 追加写日志（append-only log）与启动恢复。
//!
//! 每一次成功的写操作都会先以一行记录追加到数据文件并落盘，然后才更新内存，
//! 因此客户端收到 `OK` 时数据一定已经可靠保存。
//!
//! 记录格式（字段以 `\t` 分隔，一行一条）：
//! - `SET\t<key>\t<过期时间戳ms|->\t<value>`
//! - `DEL\t<key>`
//!
//! 键和值中的 `\`、`\t`、`\n`、`\r` 会被转义，保证一条记录不会跨行。

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::protocol::{validate_key, validate_value};
use crate::store::Store;

/// 一条持久化记录
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Record {
    Set {
        key: String,
        value: String,
        expire_at_ms: Option<u64>,
    },
    Del {
        key: String,
    },
}

impl Record {
    fn encode(&self) -> String {
        match self {
            Record::Set {
                key,
                value,
                expire_at_ms,
            } => {
                let exp = expire_at_ms.map(|t| t.to_string()).unwrap_or("-".into());
                format!("SET\t{}\t{}\t{}", escape(key), exp, escape(value))
            }
            Record::Del { key } => format!("DEL\t{}", escape(key)),
        }
    }

    fn decode(line: &str) -> Result<Record> {
        let parts: Vec<&str> = line.split('\t').collect();
        match parts.as_slice() {
            ["SET", key, exp, value] => {
                let expire_at_ms = match *exp {
                    "-" => None,
                    t => Some(
                        t.parse()
                            .map_err(|_| Error::Corrupt(format!("非法的过期时间: {t}")))?,
                    ),
                };
                let key = unescape(key)?;
                let value = unescape(value)?;
                validate_recovered(&key, &value)?;
                Ok(Record::Set {
                    key,
                    value,
                    expire_at_ms,
                })
            }
            ["DEL", key] => {
                let key = unescape(key)?;
                validate_key(&key)
                    .map_err(|e| Error::Corrupt(format!("DEL 记录的键不符合协议约束: {e}")))?;
                Ok(Record::Del { key })
            }
            _ => Err(Error::Corrupt(format!("字段个数或类型非法: {line}"))),
        }
    }
}

fn validate_recovered(key: &str, value: &str) -> Result<()> {
    validate_key(key).map_err(|e| Error::Corrupt(format!("SET 记录的键不符合协议约束: {e}")))?;
    validate_value(value)
        .map_err(|e| Error::Corrupt(format!("SET 记录的值不符合协议约束: {e}")))?;
    Ok(())
}

/// 追加写日志文件
#[derive(Debug)]
pub struct AppendLog {
    path: PathBuf,
    file: File,
}

impl AppendLog {
    /// 打开数据文件并恢复出内存状态。
    ///
    /// 文件不存在时自动创建所需目录与空文件，以空数据库状态启动；
    /// 文件内容损坏或末尾记录被截断时返回错误，绝不静默清空数据。
    pub fn open<P: AsRef<Path>>(path: P) -> Result<(AppendLog, Store)> {
        let path = path.as_ref().to_path_buf();
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                fs::create_dir_all(dir)?;
            }
        }
        let store = if path.exists() {
            Self::replay(&path)?
        } else {
            Store::new()
        };
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok((AppendLog { path, file }, store))
    }

    /// 按写入顺序重放全部记录，得到上次运行结束时的最终状态
    fn replay(path: &Path) -> Result<Store> {
        let meta = fs::metadata(path)?;
        let mut store = Store::new();
        let reader = BufReader::new(File::open(path)?);
        let mut consumed: u64 = 0;
        for (i, line) in reader.lines().enumerate() {
            let line = line.map_err(|e| Error::Corrupt(format!("第 {} 行读取失败: {e}", i + 1)))?;
            consumed += line.len() as u64 + 1; // +1 为换行符
            if line.is_empty() {
                continue;
            }
            match Record::decode(&line)
                .map_err(|e| Error::Corrupt(format!("第 {} 行 {e}", i + 1)))?
            {
                Record::Set {
                    key,
                    value,
                    expire_at_ms,
                } => store.set(key, value, expire_at_ms),
                Record::Del { key } => {
                    store.remove(&key);
                }
            }
        }
        // 最后一行缺少换行符，说明上次写入中途被打断
        if consumed != meta.len() {
            return Err(Error::Corrupt(format!(
                "文件 {} 末尾记录不完整（可能被截断）",
                path.display()
            )));
        }
        Ok(store)
    }

    /// 追加一条记录并立即落盘
    pub fn append(&mut self, record: &Record) -> Result<()> {
        writeln!(self.file, "{}", record.encode())?;
        self.file.flush()?;
        self.file.sync_data()?;
        Ok(())
    }

    /// 日志压缩：用当前有效数据重写文件，丢弃历史覆盖与删除记录。
    /// 先写临时文件再原子重命名，中途失败不会破坏原文件。返回压缩后的记录数。
    pub fn compact(&mut self, store: &mut Store) -> Result<usize> {
        let tmp_path = self.path.with_extension("compact.tmp");
        let mut tmp = File::create(&tmp_path)?;
        let mut count = 0;
        for (key, entry) in store.iter_valid() {
            let record = Record::Set {
                key: key.clone(),
                value: entry.value.clone(),
                expire_at_ms: entry.expire_at_ms,
            };
            writeln!(tmp, "{}", record.encode())?;
            count += 1;
        }
        tmp.flush()?;
        tmp.sync_data()?;
        fs::rename(&tmp_path, &self.path)?;
        self.file = OpenOptions::new().append(true).open(&self.path)?;
        Ok(count)
    }
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out
}

fn unescape(s: &str) -> Result<String> {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('\\') => out.push('\\'),
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            other => {
                return Err(Error::Corrupt(format!("非法转义字符: \\{:?}", other)));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 生成互不冲突的临时文件路径
    fn tmp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rkv-test-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir.join("rkv.log")
    }

    #[test]
    fn record_roundtrip() {
        for rec in [
            Record::Set {
                key: "k\\1".into(),
                value: "含\t制表符和\\反斜杠".into(),
                expire_at_ms: None,
            },
            Record::Set {
                key: "k".into(),
                value: "v".into(),
                expire_at_ms: Some(1234),
            },
            Record::Del { key: "k".into() },
        ] {
            assert_eq!(Record::decode(&rec.encode()).unwrap(), rec);
        }
    }

    #[test]
    fn recover_final_state_after_restart() {
        let path = tmp_path("recover");
        {
            let (mut log, mut store) = AppendLog::open(&path).unwrap();
            assert_eq!(store.len(), 0); // 首次启动为空库
            log.append(&Record::Set {
                key: "a".into(),
                value: "1".into(),
                expire_at_ms: None,
            })
            .unwrap();
            log.append(&Record::Set {
                key: "a".into(),
                value: "2".into(), // 覆盖
                expire_at_ms: None,
            })
            .unwrap();
            log.append(&Record::Set {
                key: "b".into(),
                value: "x".into(),
                expire_at_ms: None,
            })
            .unwrap();
            log.append(&Record::Del { key: "b".into() }).unwrap();
        }

        let (_log, mut store) = AppendLog::open(&path).unwrap();
        assert_eq!(store.get("a"), Some("2"));
        assert_eq!(store.get("b"), None);
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn corrupt_file_reports_error() {
        let path = tmp_path("corrupt");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "SET\tk\t-\tv\nBAD LINE\n").unwrap();
        assert!(matches!(AppendLog::open(&path), Err(Error::Corrupt(_))));
    }

    #[test]
    fn truncated_file_reports_error() {
        let path = tmp_path("truncated");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut f = File::create(&path).unwrap();
        write!(f, "SET\tk\t-\tv\nSET\tk2\t-\tpar").unwrap(); // 末尾无换行
        drop(f);
        assert!(matches!(AppendLog::open(&path), Err(Error::Corrupt(_))));
    }

    #[test]
    fn recovered_records_must_respect_protocol_limits() {
        let bad_key = tmp_path("bad-key");
        fs::create_dir_all(bad_key.parent().unwrap()).unwrap();
        fs::write(&bad_key, "SET\tbad key\t-\tv\n").unwrap();
        assert!(matches!(AppendLog::open(&bad_key), Err(Error::Corrupt(_))));

        let bad_value = tmp_path("bad-value");
        fs::create_dir_all(bad_value.parent().unwrap()).unwrap();
        fs::write(&bad_value, "SET\tk\t-\tline1\\nline2\n").unwrap();
        assert!(matches!(
            AppendLog::open(&bad_value),
            Err(Error::Corrupt(_))
        ));
    }

    #[test]
    fn compact_shrinks_log() {
        let path = tmp_path("compact");
        let (mut log, mut store) = AppendLog::open(&path).unwrap();
        for i in 0..5 {
            let rec = Record::Set {
                key: "k".into(),
                value: i.to_string(),
                expire_at_ms: None,
            };
            log.append(&rec).unwrap();
            store.set("k".into(), i.to_string(), None);
        }
        assert_eq!(log.compact(&mut store).unwrap(), 1);

        let (_log, mut restored) = AppendLog::open(&path).unwrap();
        assert_eq!(restored.get("k"), Some("4"));
        assert_eq!(fs::read_to_string(&path).unwrap().lines().count(), 1);
    }
}
