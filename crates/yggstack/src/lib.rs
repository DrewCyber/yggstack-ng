pub mod config;
pub mod forward;
pub mod mapping;
pub mod netstack;
pub mod resolver;
pub mod socks;

/// Auto-incremented build number (set by build.rs).
pub const BUILD_NUM: &str = env!("BUILD_NUM");
