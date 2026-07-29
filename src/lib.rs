pub mod app;
pub mod azure;
pub mod cache;
pub mod config;
pub mod logs;
pub mod model;
pub mod render;
pub mod sanitize;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
