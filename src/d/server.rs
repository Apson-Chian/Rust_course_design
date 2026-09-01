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

/// 服务器实例：持有监听套接字与共享的存储引擎
pub struct Server {
    listener: TcpListener,
    engine: Arc<Mutex<Engine>>,
    /// 当前在线连接数，仅用于 STATS 展示
    clients: Arc<AtomicUsize>,
}

impl Server {
    /// 恢复数据并绑定监听地址
    pub fn bind(cfg: &ServerConfig) -> Result<Server> {
        let engine = Engine::open(&cfg.data_file)?;
        let listener = TcpListener::bind(&cfg.addr)?;
        Ok(Server {
            listener,
            engine: Arc::new(Mutex::new(engine)),
            clients: Arc::new(AtomicUsize::new(0)),
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
                    self.spawn_client(s);
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
                    workers.push(self.spawn_client(stream));
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

    fn spawn_client(&self, stream: TcpStream) -> thread::JoinHandle<()> {
        let engine = Arc::clone(&self.engine);
        let clients = Arc::clone(&self.clients);
        // 单个连接的错误被隔离在自己的线程内，不影响服务器与其他客户端
        thread::spawn(move || {
            if let Err(e) = handle_conn(stream, engine, clients) {
                eprintln!("[rkv-server] 连接异常结束: {e}");
            }
        })
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
    fn new(counter: Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        ClientGuard(counter)
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
) -> Result<()> {
    let peer = stream.peer_addr()?;
    let _guard = ClientGuard::new(Arc::clone(&clients));
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
