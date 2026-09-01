# rkv 课堂演示稿

## 一条命令自动演示

在项目根目录运行：

```bash
bash scripts/demo.sh
```

脚本会自动完成 release 构建、启动本地服务器、CRUD、4 客户端并发写入、
TTL、错误隔离、日志压缩、服务器重启和数据恢复，结束时自动停止服务器并清理临时数据。

端口被占用时可更换端口：

```bash
RKV_DEMO_PORT=18888 bash scripts/demo.sh
```

需要在演示后检查日志文件时，可保留临时目录：

```bash
RKV_DEMO_KEEP=1 bash scripts/demo.sh
```

## 四人讲解顺序

### A：协议与客户端（约 1 分钟）

讲解要点：

- 一行表示一条请求或响应，客户端和服务器共用 `protocol` 模块；
- 键最大 256 字节，值最大 4 KB，单条消息最大 8 KB；
- 非法命令在客户端或服务端返回错误，不影响后续请求。

对应自动演示：`PING`、`SET`、`GET`、`LIST`、非法 `SET only-key` 后继续 `PING`。

### B：内存存储与持久化（约 2 分钟）

讲解要点：

- 内存层使用 `BTreeMap`，因此 `LIST` 天然按键排序；
- 写操作先追加日志并执行 `sync_data`，成功后才修改内存；
- `SETEX` 保存绝对过期时间，重启后 TTL 语义不变；
- `COMPACT` 只保留当前有效记录，减少历史覆盖和删除记录。

对应自动演示：`SETEX code 2 123456`、等待过期、`COMPACT`、重启后读取数据。

### C：执行引擎与网络并发（约 2 分钟）

讲解要点：

- 每个 TCP 客户端由独立线程处理；
- 多个连接通过 `Arc<Mutex<Engine>>` 共享同一个引擎；
- 网络读写期间不持有引擎锁，一次命令执行结束后立即释放；
- `STATS` 展示有效键数量、在线客户端和服务器运行时间。

对应自动演示：4 个客户端并发写入 20 个键，然后读取 `worker_3_4` 并查看 `STATS`。

### D：测试、质量与总结（约 1 分钟）

讲解要点：

- 23 个单元测试覆盖协议、存储、日志恢复、压缩和执行引擎；
- 8 个集成测试使用真实 TCP，覆盖并发、异常输入、TTL 和真实重启；
- 格式检查和 Clippy 均通过；
- 多人协作的文件边界、分支策略和评审规则记录在 `TEAMWORK.md`。

现场可运行：

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## 手动双终端演示

如果老师希望看到交互过程，可先构建：

```bash
cargo build --release
```

终端 1 启动服务器：

```bash
./target/release/rkv-server --addr 127.0.0.1:7878 --data data/class-demo.log
```

终端 2 启动客户端：

```bash
./target/release/rkv-client --addr 127.0.0.1:7878
```

依次输入：

```text
PING
SET course Rust-programming
GET course
SET student 张三
LIST
STATS
SETEX code 3 123456
GET code
COMPACT
QUIT
```

演示持久化时，在终端 1 按 `Ctrl+C`，使用相同的数据文件重新启动服务器，
重新连接后执行 `GET course`。永久键仍然存在，等待超过 TTL 后 `GET code`
会返回键不存在。

## 结束总结

一句话总结：rkv 使用共享内存存储提供并发访问，通过追加日志保证重启恢复，
协议、存储、持久化、执行引擎和网络层保持单向依赖，方便多人按模块并行开发。
