//! Internal ping tests over the netstack: self-ping through a real node
//! (the stack answers echo requests to its own address), plus session
//! lifecycle — stop, validation, replacement.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use yggstack_mobile::{generate_config, PingCallback, YggstackMobile};

const SOCKS_ADDR: &str = "127.0.0.1:0";

#[derive(Default)]
struct Collector {
    events: Mutex<Vec<String>>,
}

impl Collector {
    fn probe_count(&self, success: Option<bool>) -> usize {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| {
                e.contains(r#""Type":"probe""#)
                    && match success {
                        Some(want) => e.contains(&format!(r#""Success":{want}"#)),
                        None => true,
                    }
            })
            .count()
    }

    fn done_reason(&self) -> Option<String> {
        let events = self.events.lock().unwrap();
        let done = events
            .iter()
            .filter(|e| e.contains(r#""Type":"done""#))
            .last()?;
        let idx = done.find("\"Reason\":\"")? + "\"Reason\":\"".len();
        let rest = &done[idx..];
        Some(rest[..rest.find('"')?].to_string())
    }
}

impl PingCallback for Collector {
    fn on_result(&self, result: String) {
        self.events.lock().unwrap().push(result);
    }
}

fn started_engine() -> (YggstackMobile, String) {
    let m = YggstackMobile::new();
    m.set_socks(SOCKS_ADDR.to_string());
    m.load_config(generate_config()).expect("load_config");
    m.start().expect("start");
    let addr = m.get_address().expect("address");
    (m, addr)
}

/// Wait until the collector observed a done event (or fail after `secs`).
fn await_done(collector: &Collector, secs: u64) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while collector.done_reason().is_none() {
        assert!(Instant::now() < deadline, "ping session never finished");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn self_ping_replies_through_the_stack() {
    let (m, addr) = started_engine();
    let collector = Arc::new(Collector::default());

    m.start_ping(addr, 3, 2_000, 200, Box::new(CloneCb(collector.clone())))
        .expect("start_ping");

    await_done(&collector, 15);
    assert_eq!(collector.done_reason().as_deref(), Some("completed"));
    // The very first probe to any destination can be lost to the overlay's
    // key lookup (by design: it is reported as a plain timeout). Every
    // probe after the first must be answered in-stack.
    assert!(
        collector.probe_count(Some(true)) >= 2,
        "expected at most the first probe to fail: {:?}",
        collector.events.lock().unwrap()
    );

    m.stop_ping();
    m.stop();
}

#[test]
fn stop_ping_ends_the_session() {
    let (m, addr) = started_engine();
    let collector = Arc::new(Collector::default());

    m.start_ping(addr, 0, 2_000, 200, Box::new(CloneCb(collector.clone())))
        .expect("start_ping");
    // Let a few infinite-mode probes through, then stop.
    std::thread::sleep(Duration::from_millis(700));
    m.stop_ping();
    await_done(&collector, 5);
    assert_eq!(collector.done_reason().as_deref(), Some("stopped"));

    m.stop();
}

#[test]
fn start_ping_validates_input_and_state() {
    let (m, addr) = started_engine();

    assert!(m
        .start_ping("not-an-address".into(), 1, 2_000, 200, Box::new(Noop))
        .is_err());
    assert!(m
        .start_ping("2001:db8::1".into(), 1, 2_000, 200, Box::new(Noop))
        .is_err());

    m.stop();
    assert!(m.start_ping(addr, 1, 2_000, 200, Box::new(Noop)).is_err());
}

#[test]
fn new_session_replaces_the_running_one() {
    let (m, addr) = started_engine();
    let first = Arc::new(Collector::default());
    let second = Arc::new(Collector::default());

    m.start_ping(
        addr.clone(),
        0,
        2_000,
        200,
        Box::new(CloneCb(first.clone())),
    )
    .expect("first start_ping");
    std::thread::sleep(Duration::from_millis(300));
    m.start_ping(addr, 1, 2_000, 200, Box::new(CloneCb(second.clone())))
        .expect("second start_ping");

    // The replaced session reports it was stopped; the new one completes.
    await_done(&first, 5);
    assert_eq!(first.done_reason().as_deref(), Some("stopped"));
    await_done(&second, 15);
    assert_eq!(second.done_reason().as_deref(), Some("completed"));
    assert_eq!(second.probe_count(Some(true)), 1);

    m.stop();
}

/// Clone an `Arc<Collector>` into a `PingCallback`.
struct CloneCb(Arc<Collector>);

impl PingCallback for CloneCb {
    fn on_result(&self, result: String) {
        self.0.on_result(result);
    }
}

struct Noop;

impl PingCallback for Noop {
    fn on_result(&self, _result: String) {}
}
