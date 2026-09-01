//! 命令行客户端：读取用户输入 → 发送请求 → 显示服务器返回的结果。

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;

use rkv::config::ClientConfig;
use rkv::protocol::{Command, Response};

const HELP: &str = "\
可用命令:
  SET <键> <值>          写入或覆盖
  SETEX <键> <秒> <值>   写入并设置过期时间
  GET <键>               查询
  DEL <键>               删除
  LIST                   列出全部键
  STATS                  查看服务器状态
  PING                   连通性检查
  COMPACT                压缩数据文件
  HELP                   显示本帮助
  QUIT                   退出";

fn main() {
    let cfg = match ClientConfig::from_args(std::env::args().skip(1)) {
        Ok(cfg) => cfg,
        Err(msg) => {
            eprintln!("{msg}");
            std::process::exit(1);
        }
    };

    let stream = match TcpStream::connect(&cfg.addr) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("连接服务器 {} 失败: {e}", cfg.addr);
            std::process::exit(1);
        }
    };
    println!("已连接到 {}，输入 HELP 查看命令，QUIT 退出。", cfg.addr);

    if let Err(e) = interact(stream) {
        eprintln!("连接中断: {e}");
        std::process::exit(1);
    }
}

fn interact(stream: TcpStream) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;
    let stdin = std::io::stdin();

    print_prompt();
    for input in stdin.lock().lines() {
        let input = input?;
        let command_line = input.trim_start();
        if command_line.trim().is_empty() {
            print_prompt();
            continue;
        }
        if command_line.trim().eq_ignore_ascii_case("HELP") {
            println!("{HELP}");
            print_prompt();
            continue;
        }

        // 先在本地校验，非法输入无需往返服务器
        let cmd = match Command::parse(command_line) {
            Ok(cmd) => cmd,
            Err(e) => {
                println!("{e}（输入 HELP 查看用法）");
                print_prompt();
                continue;
            }
        };

        writeln!(writer, "{}", cmd.encode())?;
        writer.flush()?;

        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            println!("服务器已关闭连接");
            return Ok(());
        }
        match Response::parse(&line) {
            Ok(resp) => println!("{}", resp.display()),
            Err(e) => println!("无法解析服务器响应: {e}"),
        }
        if cmd == Command::Quit {
            return Ok(());
        }
        print_prompt();
    }
    Ok(())
}

fn print_prompt() {
    print!("rkv> ");
    let _ = std::io::stdout().flush();
}
