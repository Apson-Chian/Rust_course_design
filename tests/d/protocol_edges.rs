//! 协议边界集成测试：只依赖公开 API，通过真实 TCP 连接验证拆包、粘包与连接生命周期。

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use rkv::config::ServerConfig;
use rkv::server::Server;

fn temp_data_file(name: &str) -> PathBuf {
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let id = SEQ.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir()
        .join(format!(
            "rkv-protocol-{}-{}-{}",
            name,
            std::process::id(),
            id
        ))
        .join("rkv.log")
}

struct TestServer {
    addr: String,
    shutdown: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl TestServer {
    fn addr(&self) -> &str {
        &self.addr
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.join().expect("服务器线程发生 panic");
        }
    }
}

fn start_server(data_file: &Path) -> TestServer {
    let cfg = ServerConfig {
        addr: "127.0.0.1:0".into(),
        data_file: data_file.to_path_buf(),
    };
    let server = Server::bind(&cfg).expect("服务器启动失败");
    let addr = server.local_addr().unwrap().to_string();
    let shutdown = Arc::new(AtomicBool::new(false));
    let worker_shutdown = Arc::clone(&shutdown);
    let worker = thread::spawn(move || {
        server
            .serve_until(&worker_shutdown)
            .expect("服务器运行失败");
    });
    TestServer {
        addr,
        shutdown,
        worker: Some(worker),
    }
}

struct Client {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl Client {
    fn connect(addr: &str) -> Self {
        let stream = TcpStream::connect(addr).expect("连接服务器失败");
        Self {
            reader: BufReader::new(stream.try_clone().unwrap()),
            writer: stream,
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).unwrap();
        self.writer.flush().unwrap();
    }

    fn read_line(&mut self) -> String {
        let mut line = String::new();
        self.reader.read_line(&mut line).unwrap();
        line.trim_end().to_string()
    }

    fn send(&mut self, request: &str) -> String {
        self.write(format!("{request}\n").as_bytes());
        self.read_line()
    }
}

fn clients_in(stats: &str) -> usize {
    stats
        .split_whitespace()
        .find_map(|field| field.strip_prefix("clients="))
        .expect("缺少 clients 字段")
        .parse()
        .expect("clients 不是整数")
}

#[test]
fn pipelined_requests_preserve_response_order() {
    let data = temp_data_file("pipeline");
    let server = start_server(&data);
    let mut client = Client::connect(server.addr());

    client.write(b"SET course rust\nGET course\nPING\n");

    assert_eq!(client.read_line(), "OK");
    assert_eq!(client.read_line(), "VALUE rust");
    assert_eq!(client.read_line(), "PONG");
}

#[test]
fn fragmented_request_waits_for_newline_and_accepts_crlf() {
    let data = temp_data_file("fragments");
    let server = start_server(&data);
    let mut client = Client::connect(server.addr());

    client.write(b"SET fragmented ");
    client
        .reader
        .get_ref()
        .set_read_timeout(Some(Duration::from_millis(80)))
        .unwrap();
    let mut premature = String::new();
    let error = client.reader.read_line(&mut premature).unwrap_err();
    assert!(matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    ));
    client.reader.get_ref().set_read_timeout(None).unwrap();

    client.write(b"value\r\n");
    assert_eq!(client.read_line(), "OK");
    assert_eq!(client.send("GET fragmented\r"), "VALUE value");
}

#[test]
fn blank_request_returns_error_and_connection_remains_usable() {
    let data = temp_data_file("blank");
    let server = start_server(&data);
    let mut client = Client::connect(server.addr());

    client.write(b"\n");
    assert!(client.read_line().starts_with("ERR"));
    assert_eq!(client.send("PING"), "PONG");
}

#[test]
fn several_requests_in_one_packet_are_all_processed() {
    let data = temp_data_file("several");
    let server = start_server(&data);
    let mut client = Client::connect(server.addr());

    client.write(b"SET a 1\nSET b 2\nLIST\nDEL a\nGET a\n");
    let responses: Vec<_> = (0..5).map(|_| client.read_line()).collect();
    assert_eq!(responses, ["OK", "OK", "KEYS a b", "OK", "NOT_FOUND"]);
}

#[test]
fn stats_connection_count_drops_after_disconnect() {
    let data = temp_data_file("clients");
    let server = start_server(&data);
    let idle = Client::connect(server.addr());
    let mut observer = Client::connect(server.addr());

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if clients_in(&observer.send("STATS")) == 2 {
            break;
        }
        assert!(Instant::now() < deadline, "服务器未统计两个连接");
        thread::sleep(Duration::from_millis(10));
    }

    drop(idle);
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if clients_in(&observer.send("STATS")) == 1 {
            break;
        }
        assert!(Instant::now() < deadline, "断开后连接计数未回落");
        thread::sleep(Duration::from_millis(10));
    }
}
