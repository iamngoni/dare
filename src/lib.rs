//! dare.run - Multi-agent orchestration for OpenClaw/Claude agents
//!
//! This library provides the core components for dare.run:
//! - Configuration management
//! - Task DAG representation and execution
//! - Database persistence
//! - Gateway communication
//! - Dashboard server

pub mod config;
pub mod dag;
pub mod db;
pub mod executor;
pub mod gateway;
pub mod message_bus;
pub mod models;
pub mod planner;
pub mod server;
