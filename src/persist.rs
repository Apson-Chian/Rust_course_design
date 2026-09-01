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

/// 触发压缩的最小记录数，低于该值时压缩收益有限
pub const COMPACT_MIN_RECORDS: u64 = 64;

/// 日志文件统计信息
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogStats {
    /// 文件中的记录条数（含被覆盖和删除的历史记录）
    pub records: u64,
    /// 文件字节数
    pub bytes: u64,
}

/// 追加写日志文件
#[derive(Debug)]
pub struct AppendLog {
    path: PathBuf,
    file: File,
    /// 已确认写入的字节数，用于追加失败时回滚半条记录
    size: u64,
    /// 已确认写入的记录条数
    records: u64,
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
        let (store, records) = if path.exists() {
            Self::replay(&path)?
        } else {
            (Store::new(), 0)
        };
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let size = file.metadata()?.len();
        Ok((
            AppendLog {
                path,
                file,
                size,
                records,
            },
            store,
        ))
    }

    /// 修复被截断的数据文件：丢弃末尾那条不完整的记录，返回丢弃的字节数。
    ///
    /// 进程在写日志中途崩溃会留下半行记录，此时 [`AppendLog::open`] 会拒绝启动。
    /// 该方法提供显式的修复入口——只丢弃最后一个换行符之后的残留字节，
    /// 已完整落盘的记录不受影响，避免自动修复掩盖真正的数据损坏。
    pub fn repair_truncated<P: AsRef<Path>>(path: P) -> Result<u64> {
        let path = path.as_ref();
        // 课设规模下日志文件较小，一次性读入即可
        let data = fs::read(path)?;
        let keep = match data.iter().rposition(|b| *b == b'\n') {
            Some(i) => i as u64 + 1,
            None => 0, // 整个文件都是残留内容
        };
        let dropped = data.len() as u64 - keep;
        if dropped == 0 {
            return Ok(0);
        }
        let file = OpenOptions::new().write(true).open(path)?;
        file.set_len(keep)?;
        file.sync_data()?;
        Ok(dropped)
    }

    /// 按写入顺序重放全部记录，得到上次运行结束时的最终状态与记录条数
    fn replay(path: &Path) -> Result<(Store, u64)> {
        let meta = fs::metadata(path)?;
        let mut store = Store::new();
        let reader = BufReader::new(File::open(path)?);
        let mut consumed: u64 = 0;
        let mut records: u64 = 0;
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
            records += 1;
        }
        // 最后一行缺少换行符，说明上次写入中途被打断
        if consumed != meta.len() {
            return Err(Error::Corrupt(format!(
                "文件 {} 末尾记录不完整（可能被截断）",
                path.display()
            )));
        }
        Ok((store, records))
    }

    /// 追加一条记录并立即落盘。
    ///
    /// 写入或落盘失败时，把文件回滚到本次写入前的长度，
    /// 避免残留半条记录导致下次启动被判定为「文件损坏」。
    pub fn append(&mut self, record: &Record) -> Result<()> {
        let line = format!("{}\n", record.encode());
        match self.write_line(&line) {
            Ok(()) => {
                self.size += line.len() as u64;
                self.records += 1;
                Ok(())
            }
            Err(e) => {
                self.rollback(self.size);
                Err(e)
            }
        }
    }

    fn write_line(&mut self, line: &str) -> Result<()> {
        self.file.write_all(line.as_bytes())?;
        self.file.flush()?;
        self.file.sync_data()?;
        Ok(())
    }

    /// 把文件截断回指定长度；回滚本身失败时只告警，
    /// 因为此时原始错误更重要，且下次启动仍会报出文件损坏
    fn rollback(&mut self, len: u64) {
        if let Err(e) = self.file.set_len(len).and_then(|_| self.file.sync_data()) {
            eprintln!("[rkv-persist] 回滚未完成的写入失败: {e}");
        }
    }

    /// 当前日志文件的统计信息
    pub fn stats(&self) -> LogStats {
        LogStats {
            records: self.records,
            bytes: self.size,
        }
    }

    /// 是否值得压缩：记录数达到下限，且历史冗余记录占到一半以上。
    /// 只做判断不自动执行，压缩时机由调用方决定。
    pub fn should_compact(&self, live_keys: usize) -> bool {
        self.records >= COMPACT_MIN_RECORDS && self.records >= 2 * live_keys.max(1) as u64
    }

    /// 日志压缩：用当前有效数据重写文件，丢弃历史覆盖与删除记录。
    /// 先写临时文件再原子重命名，中途失败会清理临时文件且不影响原文件。
    /// 返回压缩后的记录数。
    pub fn compact(&mut self, store: &mut Store) -> Result<usize> {
        let tmp_path = self.path.with_extension("compact.tmp");
        let count = match Self::write_snapshot(&tmp_path, store) {
            Ok(count) => count,
            Err(e) => {
                // 快照没写成功，原文件保持不变
                let _ = fs::remove_file(&tmp_path);
                return Err(e);
            }
        };
        fs::rename(&tmp_path, &self.path)?;
        // 重命名本身也要落盘，否则崩溃后目录项可能仍指向旧文件
        sync_parent_dir(&self.path);
        self.file = OpenOptions::new().append(true).open(&self.path)?;
        self.size = self.file.metadata()?.len();
        self.records = count as u64;
        Ok(count)
    }

    /// 把当前有效数据写入指定文件并落盘，返回记录数
    fn write_snapshot(tmp_path: &Path, store: &mut Store) -> Result<usize> {
        let mut tmp = File::create(tmp_path)?;
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
        Ok(count)
    }
}

/// 同步父目录，使重命名等目录项变更真正落盘。
/// Windows 不允许以文件方式打开目录，因此仅在 unix 上执行。
#[cfg(unix)]
fn sync_parent_dir(path: &Path) {
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    if let Ok(f) = File::open(dir) {
        let _ = f.sync_all();
    }
}

#[cfg(not(unix))]
fn sync_parent_dir(_path: &Path) {}

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
    fn partial_write_is_rolled_back() {
        let path = tmp_path("rollback");
        let (mut log, _store) = AppendLog::open(&path).unwrap();
        log.append(&Record::Set {
            key: "k".into(),
            value: "v".into(),
            expire_at_ms: None,
        })
        .unwrap();
        let good_size = log.size;

        // 模拟写入过程中崩溃：只落了半条记录
        log.file.write_all(b"SET\tk2\t-\tpar").unwrap();
        log.file.flush().unwrap();
        assert!(fs::metadata(&path).unwrap().len() > good_size);

        log.rollback(good_size);

        // 回滚后文件仍是完整的，可以正常恢复
        assert_eq!(fs::metadata(&path).unwrap().len(), good_size);
        let (_log, mut restored) = AppendLog::open(&path).unwrap();
        assert_eq!(restored.get("k"), Some("v"));
    }

    #[test]
    fn repair_truncated_drops_incomplete_tail() {
        let path = tmp_path("repair");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "SET\tk\t-\tv\nSET\tk2\t-\tpar").unwrap();
        // 未修复前拒绝启动
        assert!(matches!(AppendLog::open(&path), Err(Error::Corrupt(_))));

        assert_eq!(AppendLog::repair_truncated(&path).unwrap(), 12);
        assert_eq!(AppendLog::repair_truncated(&path).unwrap(), 0); // 幂等

        // 修复后只丢弃未完成的那条，已确认的数据仍在
        let (_log, mut store) = AppendLog::open(&path).unwrap();
        assert_eq!(store.get("k"), Some("v"));
        assert_eq!(store.get("k2"), None);
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

    #[test]
    fn stats_track_records_and_size() {
        let path = tmp_path("stats");
        let (mut log, mut store) = AppendLog::open(&path).unwrap();
        assert_eq!(
            log.stats(),
            LogStats {
                records: 0,
                bytes: 0
            }
        );

        for i in 0..COMPACT_MIN_RECORDS {
            log.append(&Record::Set {
                key: "k".into(),
                value: i.to_string(),
                expire_at_ms: None,
            })
            .unwrap();
            store.set("k".into(), i.to_string(), None);
        }
        let stats = log.stats();
        assert_eq!(stats.records, COMPACT_MIN_RECORDS);
        assert_eq!(stats.bytes, fs::metadata(&path).unwrap().len());
        assert!(log.should_compact(1)); // 64 条记录只对应 1 个有效键

        log.compact(&mut store).unwrap();
        assert_eq!(log.stats().records, 1);
        assert!(!log.should_compact(1)); // 压缩后没有冗余

        // 统计信息在重启后依然准确
        let (reopened, _) = AppendLog::open(&path).unwrap();
        assert_eq!(reopened.stats(), log.stats());
    }

    /// 压缩过程中无法创建临时文件时，原数据文件必须保持完好
    #[test]
    #[cfg(unix)]
    fn compact_failure_keeps_original_file() {
        use std::os::unix::fs::PermissionsExt;

        let path = tmp_path("compact-fail");
        let (mut log, mut store) = AppendLog::open(&path).unwrap();
        for i in 0..3 {
            let value = i.to_string();
            log.append(&Record::Set {
                key: "k".into(),
                value: value.clone(),
                expire_at_ms: None,
            })
            .unwrap();
            store.set("k".into(), value, None);
        }
        let before = fs::read_to_string(&path).unwrap();

        // 去掉目录写权限，压缩时创建临时文件会失败
        let dir = path.parent().unwrap();
        fs::set_permissions(dir, fs::Permissions::from_mode(0o555)).unwrap();
        let result = log.compact(&mut store);
        fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(result.is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), before); // 原文件未被破坏
        let (_log, mut restored) = AppendLog::open(&path).unwrap();
        assert_eq!(restored.get("k"), Some("2"));
    }
}
