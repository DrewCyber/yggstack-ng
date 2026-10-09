uniffi::include_scaffolding!("yggstack_mobile");

mod mobile;
mod quic_check;

pub use mobile::{
    check_quic_peer, generate_config, get_version, LogCallback, PingCallback, YggstackError,
    YggstackMobile,
};
