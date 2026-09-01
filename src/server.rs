//! TCP 服务器：监听连接、读取请求、调用存储引擎并返回响应。
//!
//! 消息以 `\n` 分隔，服务器保证「一问一答」：读取到完整一行后才处理并回复。
//! 每个连接由独立线程处理，多个线程通过 `Arc<Mutex<Engine>>` 共享同一份数据；
//! 互斥锁只在执行一次数据操作时短暂持有，读写网络期间不持锁。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::config::ServerConfig;
use crate::engine::Engine;
use crate::error::{Error, Result};
use crate::protocol::{Command, Response, MAX_LINE};

/// 默认允许的同时在线客户端数量。
pub const DEFAULT_MAX_CLIENTS: usize = 64;
const CLIENT_IO_TIMEOUT: Duration = Duration::from_secs(60);

/// 服务器实例：持有监听套接字与共享的存储引擎
pub struct Server {
    listener: TcpListener,
    engine: Arc<Mutex<Engine>>,
    /// 当前在线连接数，仅用于 STATS 展示
    clients: Arc<AtomicUsize>,
    max_clients: usize,
}

impl Server {
    /// 恢复数据并绑定监听地址
    pub fn bind(cfg: &ServerConfig) -> Result<Server> {
        Self::bind_with_max_clients(cfg, DEFAULT_MAX_CLIENTS)
    }

    /// 使用指定连接上限绑定服务器，主要供测试和嵌入式调用方使用。
    pub fn bind_with_max_clients(cfg: &ServerConfig, max_clients: usize) -> Result<Server> {
        if max_clients == 0 {
            return Err(Error::Internal("最大连接数必须大于 0".into()));
        }
        let engine = Engine::open(&cfg.data_file)?;
        let listener = TcpListener::bind(&cfg.addr)?;
        Ok(Server {
            listener,
            engine: Arc::new(Mutex::new(engine)),
            clients: Arc::new(AtomicUsize::new(0)),
            max_clients,
        })
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.listener.local_addr()?)
    }

    pub fn key_count(&self) -> Result<usize> {
        let mut engine = self
            .engine
            .lock()
            .map_err(|_| Error::Internal("存储引擎锁已中毒".into()))?;
        Ok(engine.key_count())
    }

    /// 循环接受连接，为每个连接创建独立线程
    pub fn serve(&self) -> Result<()> {
        for stream in self.listener.incoming() {
            match stream {
                Ok(s) => {
                    if let Err(e) = self.accept_client(s) {
                        eprintln!("[rkv-server] 建立连接失败: {e}");
                    }
                }
                Err(e) => eprintln!("[rkv-server] 接受连接失败: {e}"),
            }
        }
        Ok(())
    }

    /// 服务到收到关闭信号为止，随后等待已接入的连接线程退出。
    ///
    /// 主要供测试和需要优雅停机的调用方使用。调用方应先关闭客户端连接，
    /// 再设置 `shutdown`，避免等待仍在读取请求的连接线程。
    pub fn serve_until(&self, shutdown: &AtomicBool) -> Result<()> {
        self.listener.set_nonblocking(true)?;
        let mut workers = Vec::new();

        while !shutdown.load(Ordering::Acquire) {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    // 部分平台会让 accept 出来的套接字继承监听器的非阻塞状态；
                    // 连接处理仍使用阻塞式 BufRead，因此这里显式恢复。
                    stream.set_nonblocking(false)?;
                    if let Some(worker) = self.accept_client(stream)? {
                        workers.push(worker);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(e) => return Err(e.into()),
            }
        }

        for worker in workers {
            worker
                .join()
                .map_err(|_| Error::Internal("客户端连接线程发生 panic".into()))?;
        }
        Ok(())
    }

    fn accept_client(&self, mut stream: TcpStream) -> Result<Option<thread::JoinHandle<()>>> {
        let Some(guard) = ClientGuard::try_acquire(Arc::clone(&self.clients), self.max_clients)
        else {
            stream.set_write_timeout(Some(CLIENT_IO_TIMEOUT))?;
            write_response(&mut stream, &Response::Err("服务器连接数已满".into()))?;
            return Ok(None);
        };

        stream.set_read_timeout(Some(CLIENT_IO_TIMEOUT))?;
        stream.set_write_timeout(Some(CLIENT_IO_TIMEOUT))?;
        let engine = Arc::clone(&self.engine);
        let clients = Arc::clone(&self.clients);
        // 单个连接的错误被隔离在自己的线程内，不影响服务器与其他客户端
        Ok(Some(thread::spawn(move || {
            if let Err(e) = handle_conn(stream, engine, clients, guard) {
                eprintln!("[rkv-server] 连接异常结束: {e}");
            }
        })))
    }
}

/// 启动服务器（阻塞运行）
pub fn run(cfg: &ServerConfig) -> Result<()> {
    let server = Server::bind(cfg)?;
    println!(
        "[rkv-server] 启动成功，监听 {}，数据文件 {}，已恢复 {} 个键",
        server.local_addr()?,
        cfg.data_file.display(),
        server.key_count()?
    );
    server.serve()
}

/// 在线连接计数的 RAII 守卫，线程结束（含 panic）时自动减一
struct ClientGuard(Arc<AtomicUsize>);

impl ClientGuard {
    /// 原子地检查连接上限并占用一个名额，避免多个接入线程同时越过上限。
    fn try_acquire(counter: Arc<AtomicUsize>, max_clients: usize) -> Option<Self> {
        counter
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                (current < max_clients).then_some(current + 1)
            })
            .ok()
            .map(|_| ClientGuard(counter))
    }
}

impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// 处理单个连接上的所有请求，直到客户端 QUIT 或断开
fn handle_conn(
    stream: TcpStream,
    engine: Arc<Mutex<Engine>>,
    clients: Arc<AtomicUsize>,
    _guard: ClientGuard,
) -> Result<()> {
    let peer = stream.peer_addr()?;
    println!("[rkv-server] 客户端接入: {peer}");
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;

    loop {
        let response = match read_request(&mut reader)? {
            Request::Eof => break,
            Request::TooLong => Response::Err(format!("请求超长，上限 {MAX_LINE} 字节")),
            Request::NotUtf8 => Response::Err("请求不是合法的 UTF-8 文本".into()),
            // 解析失败只回错误，连接保持可用，后续合法命令照常处理
            Request::Line(line) => match Command::parse(&line) {
                Ok(cmd) => {
                    let online = clients.load(Ordering::SeqCst);
                    // 仅在执行数据操作期间持锁
                    let resp = match engine.lock() {
                        Ok(mut engine) => engine.execute(&cmd, online),
                        Err(_) => {
                            Response::Err(Error::Internal("存储引擎锁已中毒".into()).to_string())
                        }
                    };
                    if cmd == Command::Quit {
                        write_response(&mut writer, &resp)?;
                        break;
                    }
                    resp
                }
                Err(e) => Response::Err(e.to_string()),
            },
        };
        write_response(&mut writer, &response)?;
    }

    println!("[rkv-server] 客户端断开: {peer}");
    Ok(())
}

fn write_response(w: &mut impl Write, resp: &Response) -> Result<()> {
    w.write_all(resp.encode().as_bytes())?;
    w.write_all(b"\n")?;
    w.flush()?;
    Ok(())
}

/// 一次读取的结果
enum Request {
    /// 读到完整一行请求
    Line(String),
    /// 对端关闭
    Eof,
    /// 单行超过上限，已丢弃该行剩余内容
    TooLong,
    /// 内容不是合法 UTF-8
    NotUtf8,
}

/// 带长度上限地读取一行请求，防止超长请求耗尽内存。
/// 超长时丢弃本行剩余字节，使连接可以继续处理下一条命令。
fn read_request<R: BufRead>(reader: &mut R) -> Result<Request> {
    let mut buf = Vec::new();
    let n = reader
        .by_ref()
        .take(MAX_LINE as u64)
        .read_until(b'\n', &mut buf)?;
    if n == 0 {
        return Ok(Request::Eof);
    }
    if buf.last() != Some(&b'\n') {
        // 达到上限时这一行仍未结束，跳过其剩余部分
        skip_rest_of_line(reader)?;
        return Ok(Request::TooLong);
    }
    match String::from_utf8(buf) {
        Ok(line) => Ok(Request::Line(line)),
        Err(_) => Ok(Request::NotUtf8),
    }
}

fn skip_rest_of_line<R: BufRead>(reader: &mut R) -> Result<()> {
    let mut junk = Vec::new();
    loop {
        junk.clear();
        let n = reader
            .by_ref()
            .take(MAX_LINE as u64)
            .read_until(b'\n', &mut junk)?;
        if n == 0 || junk.last() == Some(&b'\n') {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::path::PathBuf;

    fn tmp_file(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!("rkv-server-{}-{}", name, std::process::id()))
            .join("rkv.log")
    }

    fn wait_for_count(counter: &AtomicUsize, expected: usize) {
        for _ in 0..100 {
            if counter.load(Ordering::SeqCst) == expected {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!(
            "等待连接数变为 {expected} 超时，当前为 {}",
            counter.load(Ordering::SeqCst)
        );
    }

    fn request(stream: &mut TcpStream, line: &str) -> String {
        writeln!(stream, "{line}").unwrap();
        stream.flush().unwrap();
        let mut response = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut response)
            .unwrap();
        response.trim_end().to_string()
    }

    #[test]
    fn client_guard_enforces_limit_and_releases_slot() {
        let counter = Arc::new(AtomicUsize::new(0));
        let first = ClientGuard::try_acquire(Arc::clone(&counter), 2).unwrap();
        let second = ClientGuard::try_acquire(Arc::clone(&counter), 2).unwrap();
        assert!(ClientGuard::try_acquire(Arc::clone(&counter), 2).is_none());
        assert_eq!(counter.load(Ordering::SeqCst), 2);

        drop(first);
        let replacement = ClientGuard::try_acquire(Arc::clone(&counter), 2).unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 2);
        drop(second);
        drop(replacement);
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn server_rejects_excess_connection_then_accepts_replacement() {
        let cfg = ServerConfig {
            addr: "127.0.0.1:0".into(),
            data_file: tmp_file("connection-limit"),
        };
        let server = Server::bind_with_max_clients(&cfg, 1).unwrap();
        let addr = server.local_addr().unwrap();
        let clients = Arc::clone(&server.clients);
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_shutdown = Arc::clone(&shutdown);
        let worker = thread::spawn(move || server.serve_until(&server_shutdown).unwrap());

        let mut first = TcpStream::connect(addr).unwrap();
        first
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        wait_for_count(&clients, 1);
        assert_eq!(request(&mut first, "PING"), "PONG");

        let rejected = TcpStream::connect(addr).unwrap();
        rejected
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut rejection = String::new();
        BufReader::new(rejected).read_line(&mut rejection).unwrap();
        assert_eq!(rejection.trim_end(), "ERR 服务器连接数已满");

        drop(first);
        wait_for_count(&clients, 0);

        let mut replacement = TcpStream::connect(addr).unwrap();
        replacement
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        assert_eq!(request(&mut replacement, "PING"), "PONG");
        assert_eq!(request(&mut replacement, "QUIT"), "BYE");
        wait_for_count(&clients, 0);

        shutdown.store(true, Ordering::Release);
        worker.join().unwrap();
    }
}
