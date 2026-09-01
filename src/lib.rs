//! rkv：基于 Rust 的可持久化网络键值存储系统。
//!
//! 模块职责划分（各模块单向依赖，数据流自上而下）：
//! - [`protocol`]：命令模型与文本协议的解析 / 序列化
//! - [`store`]：运行时内存数据管理（键值增删改查）
//! - [`persist`]：追加写日志文件，负责落盘与启动恢复
//! - [`engine`]：把 store 与 persist 组合为对外的存储引擎
//! - [`server`]：TCP 监听、连接处理与并发调度
//! - [`config`]：命令行参数解析
//! - [`error`]：统一错误类型

pub mod config;
pub mod engine;
pub mod error;
pub mod persist;
pub mod protocol;
pub mod server;
pub mod store;

pub use error::{Error, Result};
pub mod d;