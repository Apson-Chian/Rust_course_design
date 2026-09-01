//! 存储引擎：把内存存储与持久化日志组合起来，对外提供「执行一条命令」的接口。
//!
//! 写操作严格遵循「先落盘、后改内存、再返回成功」的顺序。

use std::path::Path;
use std::time::Instant;

use crate::error::Result;
use crate::persist::{AppendLog, Record};
use crate::protocol::{Command, Response, ServerStats};
use crate::store::{now_ms, Store};

pub struct Engine {
    store: Store,
    log: AppendLog,
    started_at: Instant,
}

impl Engine {
    /// 打开数据文件并恢复数据；文件损坏时返回错误由调用方决定是否启动
    pub fn open<P: AsRef<Path>>(data_file: P) -> Result<Engine> {
        let (log, store) = AppendLog::open(data_file)?;
        Ok(Engine {
            store,
            log,
            started_at: Instant::now(),
        })
    }

    /// 当前有效键数量
    pub fn key_count(&mut self) -> usize {
        self.store.len()
    }

    /// 执行一条命令并生成响应。`clients` 由服务器层传入，用于 STATS 展示。
    ///
    /// 任何执行期错误都被转换为 `Response::Err` 返回给客户端，连接不会中断。
    pub fn execute(&mut self, cmd: &Command, clients: usize) -> Response {
        match cmd {
            Command::Set { key, value } => self.write(key.clone(), value.clone(), None),
            Command::SetEx {
                key,
                ttl_secs,
                value,
            } => {
                let expire_at = ttl_secs
                    .checked_mul(1000)
                    .and_then(|ttl_ms| now_ms().checked_add(ttl_ms));
                match expire_at {
                    Some(expire_at) => self.write(key.clone(), value.clone(), Some(expire_at)),
                    None => Response::Err("过期时间过大".into()),
                }
            }
            Command::Get { key } => match self.store.get(key) {
                Some(v) => Response::Value(v.to_string()),
                None => Response::NotFound,
            },
            Command::Del { key } => self.delete(key),
            Command::List => Response::Keys(self.store.keys()),
            Command::Stats => Response::Stats(ServerStats {
                keys: self.store.len(),
                clients,
                uptime_secs: self.started_at.elapsed().as_secs(),
            }),
            Command::Ping => Response::Pong,
            Command::Compact => match self.log.compact(&mut self.store) {
                Ok(_) => Response::Ok,
                Err(e) => Response::Err(e.to_string()),
            },
            Command::Quit => Response::Bye,
        }
    }

    fn write(&mut self, key: String, value: String, expire_at_ms: Option<u64>) -> Response {
        let record = Record::Set {
            key: key.clone(),
            value: value.clone(),
            expire_at_ms,
        };
        match self.log.append(&record) {
            Ok(()) => {
                self.store.set(key, value, expire_at_ms);
                Response::Ok
            }
            Err(e) => Response::Err(format!("写入失败，数据未保存: {e}")),
        }
    }

    fn delete(&mut self, key: &str) -> Response {
        // 键不存在时无需写日志，避免产生无意义的记录
        if self.store.get(key).is_none() {
            return Response::NotFound;
        }
        match self.log.append(&Record::Del { key: key.into() }) {
            Ok(()) => {
                self.store.remove(key);
                Response::Ok
            }
            Err(e) => Response::Err(format!("删除失败，数据未变更: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_file(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rkv-engine-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("rkv.log")
    }

    fn run(engine: &mut Engine, line: &str) -> Response {
        engine.execute(&Command::parse(line).unwrap(), 1)
    }

    #[test]
    fn crud_through_commands() {
        let mut e = Engine::open(tmp_file("crud")).unwrap();
        assert_eq!(run(&mut e, "GET 课程"), Response::NotFound);
        assert_eq!(run(&mut e, "SET 课程 Rust程序设计"), Response::Ok);
        assert_eq!(
            run(&mut e, "GET 课程"),
            Response::Value("Rust程序设计".into())
        );
        assert_eq!(run(&mut e, "SET 课程 高级系统编程"), Response::Ok);
        assert_eq!(
            run(&mut e, "GET 课程"),
            Response::Value("高级系统编程".into())
        );
        assert_eq!(run(&mut e, "LIST"), Response::Keys(vec!["课程".into()]));
        assert_eq!(run(&mut e, "DEL 课程"), Response::Ok);
        assert_eq!(run(&mut e, "DEL 课程"), Response::NotFound);
        assert_eq!(run(&mut e, "PING"), Response::Pong);
    }

    #[test]
    fn data_survives_restart() {
        let path = tmp_file("restart");
        {
            let mut e = Engine::open(&path).unwrap();
            run(&mut e, "SET k1 v1");
            run(&mut e, "SET k2 v2");
            run(&mut e, "DEL k1");
        }
        let mut e = Engine::open(&path).unwrap();
        assert_eq!(run(&mut e, "GET k1"), Response::NotFound);
        assert_eq!(run(&mut e, "GET k2"), Response::Value("v2".into()));
        assert_eq!(e.key_count(), 1);
    }

    #[test]
    fn stats_reports_key_count() {
        let mut e = Engine::open(tmp_file("stats")).unwrap();
        run(&mut e, "SET a 1");
        match run(&mut e, "STATS") {
            Response::Stats(s) => {
                assert_eq!(s.keys, 1);
                assert_eq!(s.clients, 1);
            }
            other => panic!("期望 STATS，实际 {other:?}"),
        }
    }

    #[test]
    fn oversized_ttl_returns_error_without_breaking_engine() {
        let mut e = Engine::open(tmp_file("ttl-overflow")).unwrap();
        let cmd = Command::SetEx {
            key: "k".into(),
            ttl_secs: u64::MAX,
            value: "v".into(),
        };
        assert!(matches!(e.execute(&cmd, 1), Response::Err(_)));
        assert_eq!(run(&mut e, "SET healthy yes"), Response::Ok);
        assert_eq!(run(&mut e, "GET healthy"), Response::Value("yes".into()));
    }
}
