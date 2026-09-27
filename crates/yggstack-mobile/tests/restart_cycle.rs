//! Same-process stop/start cycle regression test.
//!
//! The Android service stops an engine and starts a new one in the same
//! process. Historically the old engine's tokio runtime was only torn down
//! at GC time, racing the new start (native crash on start-after-stop).
//! These tests exercise the exact cycle the app performs:
//! start → stop → drop (uniffi destroy) → new engine → start again, plus
//! stop → start on the same instance.

use std::time::Duration;

use yggstack_mobile::{generate_config, YggstackMobile};

/// SOCKS on an ephemeral localhost port; nothing ever connects to it.
const SOCKS_ADDR: &str = "127.0.0.1:0";

fn started_engine() -> YggstackMobile {
    let m = YggstackMobile::new();
    m.set_socks(SOCKS_ADDR.to_string());
    m.load_config(generate_config()).expect("load_config");
    m.start().expect("start");
    assert!(m.is_running());
    m
}

/// Drop the engine on a helper thread with a watchdog: a hung teardown
/// fails the test instead of hanging the whole test binary.
fn drop_with_watchdog(engine: YggstackMobile, what: &str) {
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    std::thread::Builder::new()
        .name("engine-drop".into())
        .spawn(move || {
            drop(engine);
            let _ = tx.send(());
        })
        .expect("spawn drop thread");
    rx.recv_timeout(Duration::from_secs(30))
        .unwrap_or_else(|_| panic!("{what}: engine teardown hung"));
}

#[test]
fn start_stop_cycle_across_engines() {
    for round in 0..3 {
        let m = started_engine();
        m.stop();
        assert!(!m.is_running());
        drop_with_watchdog(m, &format!("round {round}"));
    }
}

#[test]
fn restart_on_same_instance() {
    let m = started_engine();
    m.stop();
    assert!(!m.is_running());
    m.start().expect("start after stop on the same instance");
    assert!(m.is_running());
    m.stop();
    drop_with_watchdog(m, "same instance");
}
