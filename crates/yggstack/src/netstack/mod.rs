pub mod device;
pub mod frag;

use std::collections::VecDeque;
use std::io;
use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll, Waker};

use smoltcp::iface::{Config as SmolConfig, Interface, SocketHandle, SocketSet};
use smoltcp::socket::{tcp, udp, Socket};
use smoltcp::time::Instant as SmolInstant;
use smoltcp::wire::{HardwareAddress, IpAddress, IpCidr, IpEndpoint, IpListenEndpoint, Ipv6Address, Ipv6Cidr};
#[cfg(feature = "ckr")]
use smoltcp::wire::{Ipv4Address, Ipv4Cidr};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::Notify;

use yggdrasil::ipv6rwc::ReadWriteCloser;

use self::device::YggDevice;
use self::frag::FragReassembler;

// ── Time helper ───────────────────────────────────────────────────────────────

fn smoltcp_now() -> SmolInstant {
    use std::time::{SystemTime, UNIX_EPOCH};
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as i64;
    SmolInstant::from_micros(micros)
}

// ── Pending connect command ───────────────────────────────────────────────────

struct PendingConnect {
    handle: SocketHandle,
    remote: IpEndpoint,
    local_port: u16,
    result_tx: tokio::sync::oneshot::Sender<Result<(), String>>,
}

// ── Listener state ────────────────────────────────────────────────────────────

/// Tracks a TCP listen socket and its accept queue.
struct TcpListenerEntry {
    listen_ep: IpListenEndpoint,
    /// The current "blank" socket waiting for a connection.
    listen_handle: SocketHandle,
    /// Fully-established sockets waiting to be accepted.
    accept_queue: VecDeque<SocketHandle>,
    /// Wakers for tasks blocked on accept().
    accept_wakers: Vec<Waker>,
}

// ── Netstack shared state ─────────────────────────────────────────────────────

struct NetstackState {
    iface: Interface,
    device: YggDevice,
    sockets: SocketSet<'static>,
    /// All tasks that are waiting on any socket event.
    wakers: Vec<Waker>,
    /// Pending TCP connect commands.
    pending_connects: Vec<PendingConnect>,
    /// Active TCP listeners.
    listeners: Vec<TcpListenerEntry>,
}

impl NetstackState {
    /// Return the number of milliseconds until the next mandatory poll.
    fn poll_delay_ms(&mut self) -> u64 {
        match self.iface.poll_delay(smoltcp_now(), &self.sockets) {
            Some(d) => d.total_millis().min(50),
            None => 50,
        }
    }

    /// Run one smoltcp poll cycle and return any outgoing raw IPv6 packets.
    /// Also services listeners and drains wakers (caller must fire them after
    /// releasing the lock).
    fn run_poll(&mut self) -> (Vec<Vec<u8>>, Vec<Waker>) {
        static POLL_COUNT: AtomicU64 = AtomicU64::new(0);
        let now = smoltcp_now();
        loop {
            self.iface.poll(now, &mut self.device, &mut self.sockets);
            if self.device.rx_queue.is_empty() {
                break;
            }
        }
        self.service_listeners();
        let tx: Vec<Vec<u8>> = self.device.tx_queue.drain(..).collect();
        let wakers: Vec<Waker> = std::mem::take(&mut self.wakers);
        // Periodic stats: every 500 polls (~5 sec)
        if POLL_COUNT.fetch_add(1, Ordering::Relaxed) % 500 == 0 {
            let mut tcp_count = 0u32;
            let mut udp_count = 0u32;
            for (_, socket) in self.sockets.iter() {
                match socket {
                    Socket::Tcp(_) => tcp_count += 1,
                    Socket::Udp(_) => udp_count += 1,
                    _ => {}
                }
            }
            // Read RSS from /proc/self/statm (pages, page_size=4096)
            let rss_mb = std::fs::read_to_string("/proc/self/statm")
                .ok()
                .and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok())
                .map(|pages| pages * 4096 / 1024 / 1024)
                .unwrap_or(0);
            tracing::info!(
                "netstack: rss={}MB tcp={} udp={} rx_q={} tx_q={} wakers={}",
                rss_mb, tcp_count, udp_count,
                self.device.rx_queue.len(), tx.len(),
                wakers.len(),
            );
        }
        (tx, wakers)
    }

    /// Process pending TCP connect commands (must be called before poll()).
    fn process_connects(&mut self) {
        let connects: Vec<PendingConnect> = std::mem::take(&mut self.pending_connects);
        for cmd in connects {
            let cx = self.iface.context();
            let socket = self.sockets.get_mut::<tcp::Socket>(cmd.handle);
            let result = socket
                .connect(cx, cmd.remote, cmd.local_port)
                .map_err(|e| format!("{:?}", e));
            let _ = cmd.result_tx.send(result);
        }
    }

    /// Create a new TCP socket with standard buffers.
    fn new_tcp_socket(&mut self) -> SocketHandle {
        let rx = tcp::SocketBuffer::new(vec![0u8; 65536]);
        let tx = tcp::SocketBuffer::new(vec![0u8; 65536]);
        self.sockets.add(tcp::Socket::new(rx, tx))
    }

    /// Create a new UDP socket with standard buffers.
    fn new_udp_socket(&mut self) -> SocketHandle {
        let rx_meta = vec![udp::PacketMetadata::EMPTY; 64];
        let tx_meta = vec![udp::PacketMetadata::EMPTY; 64];
        let rx_data = vec![0u8; 65536];
        let tx_data = vec![0u8; 65536];
        self.sockets.add(udp::Socket::new(
            udp::PacketBuffer::new(rx_meta, rx_data),
            udp::PacketBuffer::new(tx_meta, tx_data),
        ))
    }

    /// Service all listeners: if a listening socket just got established,
    /// move it to the accept queue and create a new blank listener.
    fn service_listeners(&mut self) {
        for entry in &mut self.listeners {
            let is_established = {
                let socket = self.sockets.get_mut::<tcp::Socket>(entry.listen_handle);
                socket.may_recv() && socket.may_send()
            };
            if is_established {
                // Move the established socket to the accept queue.
                entry.accept_queue.push_back(entry.listen_handle);
                // Wake tasks waiting for accept().
                for w in entry.accept_wakers.drain(..) {
                    w.wake();
                }
                // Create a new blank listening socket.
                let new_handle = {
                    let rx = tcp::SocketBuffer::new(vec![0u8; 65536]);
                    let tx = tcp::SocketBuffer::new(vec![0u8; 65536]);
                    let mut sock = tcp::Socket::new(rx, tx);
                    let _ = sock.listen(entry.listen_ep);
                    self.sockets.add(sock)
                };
                entry.listen_handle = new_handle;
            }
        }
    }

    /// Remove TCP sockets that are fully closed (state Closed or TimeWait).
    /// This frees 128 KB of buffers per socket.
    fn gc_closed_tcp(&mut self) {
        // Collect listener handles so we don't remove them.
        let listener_handles: Vec<SocketHandle> = self
            .listeners
            .iter()
            .flat_map(|e| {
                let mut v = vec![e.listen_handle];
                v.extend(e.accept_queue.iter().cloned());
                v
            })
            .collect();

        let mut to_remove = Vec::new();
        for (handle, socket) in self.sockets.iter() {
            if listener_handles.contains(&handle) {
                continue;
            }
            if let Socket::Tcp(tcp_socket) = socket {
                if matches!(tcp_socket.state(), tcp::State::Closed | tcp::State::TimeWait) {
                    to_remove.push(handle);
                }
            }
        }
        for handle in to_remove {
            self.sockets.remove(handle);
        }
    }
}

// ── YggNetstack ───────────────────────────────────────────────────────────────

pub struct YggNetstack {
    state: Arc<Mutex<NetstackState>>,
    /// Signal the poll loop to run immediately.
    poll_wakeup: Arc<Notify>,
    pub our_addr: Ipv6Addr,
    pub mtu: usize,
}

impl YggNetstack {
    /// Create a netstack backed by the given `ReadWriteCloser` and start
    /// background tasks.  `our_addr` is our Yggdrasil IPv6 address and `mtu`
    /// is the transport MTU (from `Core::mtu()`).
    pub fn new(
        rwc: Arc<ReadWriteCloser>,
        our_addr: Ipv6Addr,
        mtu: u64,
        #[cfg(feature = "ckr")] ckr_config: Option<&yggdrasil::config::TunnelRoutingConfig>,
    ) -> Arc<Self> {
        let mtu = (mtu as usize).min(65535);

        let mut device = YggDevice::new(mtu);

        // Configure smoltcp interface.
        let mut cfg = SmolConfig::new(HardwareAddress::Ip);
        cfg.random_seed = rand::random();

        let now = smoltcp_now();
        let mut iface = Interface::new(cfg, &mut device, now);

        // Assign our Yggdrasil address.
        let ip6 = Ipv6Address::from_bytes(&our_addr.octets());
        iface.update_ip_addrs(|addrs| {
            let _ = addrs.push(IpCidr::Ipv6(Ipv6Cidr::new(ip6, 128)));
        });

        // Default IPv6 route so smoltcp routes outgoing packets through our device.
        iface
            .routes_mut()
            .add_default_ipv6_route(ip6)
            .expect("failed to add default route");

        // CKR: if an IPv4 address is configured, assign it and add a default route.
        #[cfg(feature = "ckr")]
        if let Some(ckr) = ckr_config.filter(|c| c.enable && !c.ipv4_address.is_empty()) {
            if let Some((ip4, prefix)) = parse_ipv4_cidr(&ckr.ipv4_address) {
                iface.update_ip_addrs(|addrs| {
                    let _ = addrs.push(IpCidr::Ipv4(Ipv4Cidr::new(ip4, prefix)));
                });
                iface
                    .routes_mut()
                    .add_default_ipv4_route(ip4)
                    .expect("failed to add default IPv4 route");
                tracing::info!("CKR: assigned IPv4 address {}", ckr.ipv4_address);
            } else {
                tracing::warn!("CKR: could not parse ipv4_address '{}'", ckr.ipv4_address);
            }
        }

        let state = Arc::new(Mutex::new(NetstackState {
            iface,
            device,
            sockets: SocketSet::new(vec![]),
            wakers: Vec::new(),
            pending_connects: Vec::new(),
            listeners: Vec::new(),
        }));

        let poll_wakeup = Arc::new(Notify::new());

        let ns = Arc::new(Self {
            state: state.clone(),
            poll_wakeup: poll_wakeup.clone(),
            our_addr,
            mtu,
        });

        ns.spawn_tasks(rwc);
        ns
    }

    fn spawn_tasks(self: &Arc<Self>, rwc: Arc<ReadWriteCloser>) {
        // Task 1: continuously read from RWC → queue packets for smoltcp.
        let (pkt_tx, mut pkt_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(128);
        {
            let rwc2 = rwc.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 65536];
                loop {
                    match rwc2.read(&mut buf).await {
                        Ok(n) if n > 0
                            && pkt_tx.send(buf[..n].to_vec()).await.is_err() =>
                        {
                            break;
                        }
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
            });
        }

        // Task 2: smoltcp poll loop.
        let state = self.state.clone();
        let poll_wakeup = self.poll_wakeup.clone();
        let mut frag_reassembler = FragReassembler::new();

        tokio::spawn(async move {
            loop {
                // Determine how long to sleep before the next forced poll.
                let delay_ms: u64 = {
                    let mut s = state.lock().unwrap();
                    s.poll_delay_ms()
                };

                // Wait for a new packet, the timeout, or an explicit wakeup.
                tokio::select! {
                    maybe = pkt_rx.recv() => {
                        match maybe {
                            Some(pkt) => {
                                if let Some(reassembled) = frag_reassembler.feed(pkt) {
                                    let mut s = state.lock().unwrap();
                                    s.device.rx_queue.push_back(reassembled);
                                }
                            }
                            None => break, // RWC reader task closed
                        }
                    }
                    _ = tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)) => {}
                    _ = poll_wakeup.notified() => {}
                }

                // Drain any additional packets that arrived concurrently.
                while let Ok(pkt) = pkt_rx.try_recv() {
                    if let Some(reassembled) = frag_reassembler.feed(pkt) {
                        let mut s = state.lock().unwrap();
                        s.device.rx_queue.push_back(reassembled);
                    }
                }

                // Run smoltcp, collect outgoing packets and wakers.
                let (tx_pkts, wakers) = {
                    let mut s = state.lock().unwrap();
                    s.process_connects();
                    s.run_poll()
                };

                // Fire wakers outside the lock.
                for w in wakers {
                    w.wake();
                }

                // Send outgoing packets to Yggdrasil (outside lock).
                for pkt in tx_pkts {
                    let _ = rwc.write(&pkt).await;
                }
            }
        });
    }

    // ── Public dial / listen API ──────────────────────────────────────────────

    /// Dial a TCP connection to a remote address.
    /// For Yggdrasil-only builds accepts IPv6 only; with the `ckr` feature
    /// also accepts IPv4 (routed via Crypto-Key Routing).
    pub async fn dial_tcp(&self, remote: SocketAddr) -> io::Result<TcpStream> {
        let remote_ep = match remote {
            SocketAddr::V6(a) => {
                let ip6 = Ipv6Address::from_bytes(&a.ip().octets());
                IpEndpoint::new(IpAddress::Ipv6(ip6), a.port())
            }
            #[cfg(feature = "ckr")]
            SocketAddr::V4(a) => {
                let ip4 = Ipv4Address::from_bytes(&a.ip().octets());
                IpEndpoint::new(IpAddress::Ipv4(ip4), a.port())
            }
            #[cfg(not(feature = "ckr"))]
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "expected IPv6 address",
                ))
            }
        };

        // Pick a random local ephemeral port.
        let local_port: u16 = rand::random::<u16>() % 16384 + 49152;

        // Create TCP socket.
        let handle = {
            let mut s = self.state.lock().unwrap();
            s.new_tcp_socket()
        };

        // Queue the connect command for the poll loop.
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        {
            let mut s = self.state.lock().unwrap();
            s.pending_connects.push(PendingConnect {
                handle,
                remote: remote_ep,
                local_port,
                result_tx,
            });
        }
        self.poll_wakeup.notify_one();

        // Wait for the connect() call to be processed.
        result_rx
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "netstack closed"))?
            .map_err(|e| io::Error::new(io::ErrorKind::ConnectionRefused, e))?;

        // Wait for ESTABLISHED state.
        let stream = TcpStream {
            handle,
            state: self.state.clone(),
            poll_wakeup: self.poll_wakeup.clone(),
        };

        stream.wait_connected().await?;
        Ok(stream)
    }

    /// Dial a generic connection (network can be "tcp", "tcp6").
    pub async fn dial_context(&self, network: &str, address: &str) -> io::Result<TcpStream> {
        let addr: SocketAddr = address
            .parse()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("{}", e)))?;
        match network {
            "tcp" | "tcp6" => self.dial_tcp(addr).await,
            _ => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("unsupported network {}", network),
            )),
        }
    }

    /// Start a TCP listener on the given Yggdrasil address/port.
    pub fn listen_tcp(&self, port: u16) -> io::Result<TcpListener> {
        let ip6 = Ipv6Address::from_bytes(&self.our_addr.octets());
        let listen_ep = IpListenEndpoint {
            addr: Some(IpAddress::Ipv6(ip6)),
            port,
        };

        {
            let mut s = self.state.lock().unwrap();
            let h = s.new_tcp_socket();
            s.sockets
                .get_mut::<tcp::Socket>(h)
                .listen(listen_ep)
                .map_err(|e| io::Error::new(io::ErrorKind::AddrInUse, format!("{:?}", e)))?;
            s.listeners.push(TcpListenerEntry {
                listen_ep,
                listen_handle: h,
                accept_queue: VecDeque::new(),
                accept_wakers: Vec::new(),
            });
        }

        Ok(TcpListener {
            port,
            state: self.state.clone(),
            poll_wakeup: self.poll_wakeup.clone(),
        })
    }

    /// Bind a UDP socket on the given Yggdrasil address/port.
    pub fn bind_udp(&self, port: u16) -> io::Result<UdpSocket> {
        let ip6 = Ipv6Address::from_bytes(&self.our_addr.octets());
        let listen_ep = IpListenEndpoint {
            addr: Some(IpAddress::Ipv6(ip6)),
            port,
        };

        let handle = {
            let mut s = self.state.lock().unwrap();
            let h = s.new_udp_socket();
            s.sockets
                .get_mut::<udp::Socket>(h)
                .bind(listen_ep)
                .map_err(|e| io::Error::new(io::ErrorKind::AddrInUse, format!("{:?}", e)))?;
            h
        };

        Ok(UdpSocket {
            handle,
            state: self.state.clone(),
            poll_wakeup: self.poll_wakeup.clone(),
        })
    }

    /// Create a UDP socket bound to an ephemeral local port, suitable for
    /// sending to a remote Yggdrasil address and receiving replies.
    pub fn open_udp(&self) -> io::Result<UdpSocket> {
        let handle = {
            let mut s = self.state.lock().unwrap();
            let h = s.new_udp_socket();
            // smoltcp requires a socket to be bound before it can send; pick a
            // random ephemeral local port (bind addr = None means any interface).
            let local_port: u16 = rand::random::<u16>() % 16384 + 49152;
            let listen_ep = IpListenEndpoint {
                addr: None,
                port: local_port,
            };
            s.sockets
                .get_mut::<udp::Socket>(h)
                .bind(listen_ep)
                .map_err(|e| io::Error::new(io::ErrorKind::AddrInUse, format!("{:?}", e)))?;
            h
        };
        Ok(UdpSocket {
            handle,
            state: self.state.clone(),
            poll_wakeup: self.poll_wakeup.clone(),
        })
    }
}

// ── TcpStream ─────────────────────────────────────────────────────────────────

pub struct TcpStream {
    handle: SocketHandle,
    state: Arc<Mutex<NetstackState>>,
    poll_wakeup: Arc<Notify>,
}

impl TcpStream {
    /// Wait until the socket reaches ESTABLISHED state.
    async fn wait_connected(&self) -> io::Result<()> {
        std::future::poll_fn(|cx| {
            let mut s = self.state.lock().unwrap();
            let sock = s.sockets.get::<tcp::Socket>(self.handle);
            match sock.state() {
                tcp::State::Established => Poll::Ready(Ok(())),
                tcp::State::Closed
                | tcp::State::TimeWait
                | tcp::State::CloseWait => Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    "connection refused",
                ))),
                _ => {
                    s.wakers.push(cx.waker().clone());
                    Poll::Pending
                }
            }
        })
        .await
    }
}

impl Drop for TcpStream {
    fn drop(&mut self) {
        if let Ok(mut s) = self.state.lock() {
            let sock = s.sockets.get_mut::<tcp::Socket>(self.handle);
            sock.abort();
            let socket_count = s.sockets.iter().count();
            s.sockets.remove(self.handle);
            tracing::debug!("TcpStream dropped, sockets remaining: {}", socket_count - 1);
        }
        self.poll_wakeup.notify_one();
    }
}

impl AsyncRead for TcpStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let mut s = self.state.lock().unwrap();
        let sock = s.sockets.get_mut::<tcp::Socket>(self.handle);

        if sock.can_recv() {
            let filled = buf.initialize_unfilled();
            match sock.recv_slice(filled) {
                Ok(0) => {}
                Ok(n) => {
                    buf.advance(n);
                    return Poll::Ready(Ok(()));
                }
                Err(e) => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        format!("{:?}", e),
                    )))
                }
            }
        }

        if !sock.may_recv() {
            return Poll::Ready(Ok(())); // EOF — covers all closing/closed states
        }

        s.wakers.push(cx.waker().clone());
        Poll::Pending
    }
}

impl AsyncWrite for TcpStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut s = self.state.lock().unwrap();
        let sock = s.sockets.get_mut::<tcp::Socket>(self.handle);

        if !sock.may_send() {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "socket closed",
            )));
        }

        if sock.can_send() {
            match sock.send_slice(data) {
                Ok(n) => {
                    drop(s);
                    self.poll_wakeup.notify_one();
                    return Poll::Ready(Ok(n));
                }
                Err(e) => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        format!("{:?}", e),
                    )))
                }
            }
        }

        s.wakers.push(cx.waker().clone());
        Poll::Pending
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        self.poll_wakeup.notify_one();
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        let mut s = self.state.lock().unwrap();
        s.sockets.get_mut::<tcp::Socket>(self.handle).close();
        drop(s);
        self.poll_wakeup.notify_one();
        Poll::Ready(Ok(()))
    }
}

// ── TcpListener ───────────────────────────────────────────────────────────────

pub struct TcpListener {
    port: u16,
    state: Arc<Mutex<NetstackState>>,
    poll_wakeup: Arc<Notify>,
}

impl TcpListener {
    pub async fn accept(&self) -> io::Result<TcpStream> {
        let port = self.port;
        let state = self.state.clone();
        let poll_wakeup = self.poll_wakeup.clone();
        std::future::poll_fn(move |cx| {
            let mut s = state.lock().unwrap();
            let idx = s.listeners.iter().position(|e| e.listen_ep.port == port);
            if let Some(i) = idx {
                if let Some(handle) = s.listeners[i].accept_queue.pop_front() {
                    return Poll::Ready(Ok(TcpStream {
                        handle,
                        state: state.clone(),
                        poll_wakeup: poll_wakeup.clone(),
                    }));
                }
                // Register waker so service_listeners() can wake us.
                s.listeners[i].accept_wakers.push(cx.waker().clone());
            }
            Poll::Pending
        })
        .await
    }
}

// ── UdpSocket ─────────────────────────────────────────────────────────────────

pub struct UdpSocket {
    handle: SocketHandle,
    state: Arc<Mutex<NetstackState>>,
    poll_wakeup: Arc<Notify>,
}

impl UdpSocket {
    pub async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        std::future::poll_fn(|cx| {
            let mut s = self.state.lock().unwrap();
            let sock = s.sockets.get_mut::<udp::Socket>(self.handle);
            if sock.can_recv() {
                return Poll::Ready(match sock.recv_slice(buf) {
                    Ok((n, meta)) => {
                        let addr = match meta.endpoint.addr {
                            IpAddress::Ipv6(a) => {
                                let octets: [u8; 16] = a.as_bytes().try_into().unwrap();
                                SocketAddr::V6(SocketAddrV6::new(
                                    Ipv6Addr::from(octets),
                                    meta.endpoint.port,
                                    0,
                                    0,
                                ))
                            }
                            _ => {
                                return Poll::Ready(Err(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "non-IPv6 source",
                                )))
                            }
                        };
                        Ok((n, addr))
                    }
                    Err(e) => Err(io::Error::other(format!("{:?}", e))),
                });
            }
            // Register waker so we're woken after the next smoltcp poll.
            s.wakers.push(cx.waker().clone());
            Poll::Pending
        })
        .await
    }

    pub async fn send_to(&self, buf: &[u8], target: SocketAddr) -> io::Result<usize> {
        let target_ip = match target {
            SocketAddr::V6(a) => Ipv6Address::from_bytes(&a.ip().octets()),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "expected IPv6 address",
                ))
            }
        };
        let endpoint = IpEndpoint::new(IpAddress::Ipv6(target_ip), target.port());

        std::future::poll_fn(|cx| {
            let mut s = self.state.lock().unwrap();
            let sock = s.sockets.get_mut::<udp::Socket>(self.handle);
            if sock.can_send() {
                return Poll::Ready(match sock.send_slice(buf, endpoint) {
                    Ok(()) => {
                        drop(s);
                        self.poll_wakeup.notify_one();
                        Ok(buf.len())
                    }
                    Err(e) => Err(io::Error::other(format!("{:?}", e))),
                });
            }
            s.wakers.push(cx.waker().clone());
            Poll::Pending
        })
        .await
    }
}

impl Drop for UdpSocket {
    fn drop(&mut self) {
        if let Ok(mut s) = self.state.lock() {
            s.sockets.remove(self.handle);
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Parse an IPv4 CIDR string like "10.99.0.1/24" into a smoltcp address +
/// prefix length.  Returns `None` on any parse error.
#[cfg(feature = "ckr")]
fn parse_ipv4_cidr(cidr: &str) -> Option<(Ipv4Address, u8)> {
    let (addr_str, prefix_str) = cidr.split_once('/')?;
    let addr: std::net::Ipv4Addr = addr_str.trim().parse().ok()?;
    let prefix: u8 = prefix_str.trim().parse().ok()?;
    Some((Ipv4Address::from_bytes(&addr.octets()), prefix))
}
