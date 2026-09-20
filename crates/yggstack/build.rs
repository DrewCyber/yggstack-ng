use std::fs;
use std::path::Path;

fn main() {
    let counter_file = Path::new("build_counter.txt");
    let count: u32 = fs::read_to_string(counter_file)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
        + 1;
    fs::write(counter_file, count.to_string()).unwrap();
    println!("cargo:rustc-env=BUILD_NUM={}", count);
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=build_counter.txt");
}
