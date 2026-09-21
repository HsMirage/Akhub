//! Akhub：AI 协议网关与组内负载均衡工具。
//!
//! 只负责三件事（§一）：把下游协议无损转换到上游协议、在分组内按严格优先级
//! 阶梯调度、在不重复浪费慢请求的前提下完成故障切换。

pub mod admin;
pub mod app;
pub mod auth;
pub mod capability;
pub mod config;
pub mod credential;
pub mod discovery;
pub mod domain;
pub mod gateway;
pub mod health;
pub mod multiplier;
pub mod protocol;
pub mod routing;
pub mod security;
pub mod server;
pub mod storage;
pub mod sync;
pub mod update;
pub mod upstream;
