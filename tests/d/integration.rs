//! 集成测试：以真实 TCP 连接驱动服务器，覆盖网络通信、并发、持久化与异常处理。

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use rkv::config::ServerConfig;
use rkv::server::Server;

/// 为每个测试生成独立的数据目录，避免相互干扰
fn temp_data_file(name: &str) -> PathBuf {
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let id = SEQ.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("rkv-it-{}-{}-{}", name, std::process::id(), id));
    let _ = std::fs::remove_dir_all(&dir);
    dir.join("rkv.log")
}

/// 可显式关闭并等待后台线程退出的测试服务器。
struct TestServer {
    addr: String,
    shutdown: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl TestServer {
    fn addr(&self) -> &str {
        &self.addr
    }

    fn stop(mut self) {
        self.shutdown_and_join();
    }

    fn shutdown_and_join(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.join().expect("服务器线程发生 panic");
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.shutdown_and_join();
    }
}

/// 在后台线程启动一台监听随机端口的服务器。
fn start_server(data_file: &Path) -> TestServer {
    let cfg = ServerConfig {
        addr: "127.0.0.1:0".into(), // 端口 0 由系统分配
        data_file: data_file.to_path_buf(),
    };
    let server = Server::bind(&cfg).expect("服务器启动失败");
    let addr = server.local_addr().unwrap().to_string();
    let shutdown = Arc::new(AtomicBool::new(false));
    let server_shutdown = Arc::clone(&shutdown);
    let worker = thread::spawn(move || {
        server
            .serve_until(&server_shutdown)
            .expect("服务器运行失败");
    });
    TestServer {
        addr,
        shutdown,
        worker: Some(worker),
    }
}

/// 测试用的极简客户端：发送一行请求，返回一行响应
struct Client {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl Client {
    fn connect(addr: &str) -> Client {
        let stream = TcpStream::connect(addr).expect("连接服务器失败");
        Client {
            reader: BufReader::new(stream.try_clone().unwrap()),
            writer: stream,
        }
    }

    fn send(&mut self, request: &str) -> String {
        self.send_raw(format!("{request}\n").as_bytes())
    }

    fn send_raw(&mut self, bytes: &[u8]) -> String {
        self.writer.write_all(bytes).unwrap();
        self.writer.flush().unwrap();
        let mut line = String::new();
        self.reader.read_line(&mut line).unwrap();
        line.trim_end().to_string()
    }
}

#[test]
fn single_client_full_workflow() {
    let data = temp_data_file("workflow");
    let server = start_server(&data);
    let mut c = Client::connect(server.addr());

    assert_eq!(c.send("PING"), "PONG");
    assert_eq!(c.send("GET 课程名称"), "NOT_FOUND");
    assert_eq!(c.send("SET 课程名称 Rust程序设计"), "OK");
    assert_eq!(c.send("GET 课程名称"), "VALUE Rust程序设计");
    assert_eq!(c.send("SET 课程名称 高级系统编程"), "OK"); // 覆盖
    assert_eq!(c.send("GET 课程名称"), "VALUE 高级系统编程");
    assert_eq!(c.send("LIST"), "KEYS 课程名称");
    assert!(c.send("STATS").starts_with("STATS keys=1"));
    assert_eq!(c.send("DEL 课程名称"), "OK");
    assert_eq!(c.send("DEL 课程名称"), "NOT_FOUND");
    assert_eq!(c.send("QUIT"), "BYE");
}

#[test]
fn connection_survives_bad_commands() {
    let data = temp_data_file("badcmd");
    let server = start_server(&data);
    let mut c = Client::connect(server.addr());

    assert!(c.send("FOO bar").starts_with("ERR"));
    assert!(c.send("SET onlykey").starts_with("ERR"));
    assert!(c.send("GET a b").starts_with("ERR"));
    assert!(c.send_raw(b"\n").starts_with("ERR"));
    // 出错后连接仍可正常工作
    assert_eq!(c.send("SET k v"), "OK");
    assert_eq!(c.send("GET k"), "VALUE v");
}

#[test]
fn oversized_and_invalid_input_are_rejected() {
    let data = temp_data_file("oversize");
    let server = start_server(&data);
    let mut c = Client::connect(server.addr());

    // 超长请求：被拒绝且连接可继续使用
    let huge = format!("SET k {}\n", "x".repeat(rkv::protocol::MAX_LINE));
    assert!(c.send_raw(huge.as_bytes()).starts_with("ERR"));
    assert_eq!(c.send("PING"), "PONG");

    // 非 UTF-8 字节
    assert!(c.send_raw(&[0xff, 0xfe, b'\n']).starts_with("ERR"));
    assert_eq!(c.send("PING"), "PONG");
}

#[test]
fn multiple_clients_share_data() {
    let data = temp_data_file("concurrent");
    let server = start_server(&data);

    // 8 个客户端各写 20 个键
    let mut handles = Vec::new();
    for t in 0..8 {
        let addr = server.addr().to_string();
        handles.push(thread::spawn(move || {
            let mut c = Client::connect(&addr);
            for i in 0..20 {
                assert_eq!(c.send(&format!("SET k{t}_{i} v{t}_{i}")), "OK");
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    // 另一个客户端能看到全部数据
    let mut checker = Client::connect(server.addr());
    assert!(checker.send("STATS").starts_with("STATS keys=160"));
    assert_eq!(checker.send("GET k3_7"), "VALUE v3_7");
}

#[test]
fn data_survives_server_restart() {
    let data = temp_data_file("restart");
    {
        let server = start_server(&data);
        let mut c = Client::connect(server.addr());
        c.send("SET 课程名称 Rust程序设计");
        c.send("SET 临时键 临时值");
        c.send("DEL 临时键");
        c.send("QUIT");
        // 确认旧监听器、连接线程和日志句柄均已退出，再执行真正的重启。
        server.stop();
    }
    // 使用同一数据文件重新启动
    let server = start_server(&data);
    let mut c = Client::connect(server.addr());
    assert_eq!(c.send("GET 课程名称"), "VALUE Rust程序设计");
    assert_eq!(c.send("GET 临时键"), "NOT_FOUND");
}

#[test]
fn expired_key_disappears() {
    let data = temp_data_file("ttl");
    let server = start_server(&data);
    let mut c = Client::connect(server.addr());

    assert_eq!(c.send("SETEX 验证码 1 123456"), "OK");
    assert_eq!(c.send("GET 验证码"), "VALUE 123456");
    thread::sleep(Duration::from_millis(1100));
    assert_eq!(c.send("GET 验证码"), "NOT_FOUND");
    assert!(c
        .send(&format!("SETEX huge {} value", u64::MAX))
        .starts_with("ERR"));
    assert_eq!(c.send("PING"), "PONG");
}

#[test]
fn compact_keeps_latest_data() {
    let data = temp_data_file("compact");
    let server = start_server(&data);
    let mut c = Client::connect(server.addr());

    for i in 0..10 {
        c.send(&format!("SET k {i}"));
    }
    assert_eq!(c.send("COMPACT"), "OK");
    assert_eq!(c.send("GET k"), "VALUE 9");
    // 压缩后文件只保留一条有效记录
    let content = std::fs::read_to_string(&data).unwrap();
    assert_eq!(content.lines().count(), 1);
}

#[test]
fn corrupt_data_file_blocks_startup() {
    let data = temp_data_file("corrupt");
    std::fs::create_dir_all(data.parent().unwrap()).unwrap();
    std::fs::write(&data, "这不是合法记录\n").unwrap();

    let cfg = ServerConfig {
        addr: "127.0.0.1:0".into(),
        data_file: data,
    };
    // 明确报错而不是静默清空数据
    assert!(matches!(Server::bind(&cfg), Err(rkv::Error::Corrupt(_))));
}
