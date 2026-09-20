uniffi::include_scaffolding!("yggstack_mobile");

mod mobile;
mod quic_check;

pub use mobile::{
    YggstackError,
    YggstackMobile,
    LogCallback,
    generate_config,
    get_version,
    check_quic_peer,
};
