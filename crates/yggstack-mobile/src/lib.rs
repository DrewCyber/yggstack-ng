uniffi::include_scaffolding!("yggstack_mobile");

mod mobile;

pub use mobile::{
    YggstackError,
    YggstackMobile,
    LogCallback,
    generate_config,
    get_version,
};
