pub mod device;

use device::YggDevice;

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context as AsyncContext, Poll};
use std::time::Duration;

use bytes::Bytes;
use crossbeam_channel as chan;
use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::socket::tcp::{Socket as TcpSocket, SocketBuffer as TcpSocketBuffer};
use smoltcp::socket::udp::{
    PacketBuffer as UdpPacketBuffer, PacketMetadata as UdpPacketMetadata,
    Socket as UdpSocket,
};
use smoltcp::time::Instant as SmolInstant;
use smoltcp::wire::{
    HardwareAddress, IpAddress, IpCidr, IpEndpoint, IpListenEndpoint, Ipv6Address, Ipv6Cidr,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{mpsc, oneshot};

use yggdrasil::ipv6rwc::ReadWriteCloser;

// ── Public types ──────────────────────────────────────────────────────────────

/// Async TCP stream through the Yggdrasil netstack.
pub struct NetTcpStream {
    read_rx: mpsc::UnboundedReceiver<Bytes>,
    write_tx: chan::Sender<Bytes>,
    close: Arc<AtomicBool>,
    leftover: Bytes,
}

impl AsyncRead for NetTcpStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut AsyncContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if !self.leftover.is_empty() {
            let n = self.leftover.len().min(buf.remaining());
            buf.put_slice(&self.leftover[..n]);
            self.leftover = self.leftover.slice(n..);
            return Poll::Ready(Ok(()));
        }
        match self.read_rx.poll_recv(cx) {
            Poll::Ready(Some(data)) => {
                let n = data.len().min(buf.remaining());
                buf.put_slice(&data[..n]);
                if n < data.len() {
                    self.leftover = data.slice(n..);
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(None) => Poll::Ready(Ok(())),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for NetTcpStream {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut AsyncContext<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.close.load(Ordering::Relaxed) {
            return Poll::Ready(Err(io::Error::from(io::ErrorKind::BrokenPipe)));
        }
        match self.write_tx.send(Bytes::copy_from_slice(buf)) {
            Ok(_) => Poll::Ready(Ok(buf.len())),
            Err(_) => Poll::Ready(Err(io::Error::from(io::ErrorKind::BrokenPipe))),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut AsyncContext<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        _cx: &mut AsyncContext<'_>,
    ) -> Poll<io::Result<()>> {
        self.close.store(true, Ordering::Relaxed);
        Poll::Ready(Ok(()))
    }
}

/// Accepts incoming TCP connections through the Yggdrasil netstack.
pub struct NetTcpListener {
    pub accept_rx: mpsc::UnboundedReceiver<NetTcpStream>,
}

impl NetTcpListener {
    pub async fn accept(&mut self) -> Option<NetTcpStream> {
        self.accept_rx.recv().await
    }
}

/// UDP socket through the Yggdrasil netstack.
pub struct NetUdpSocket {
    pub recv_rx: mpsc::UnboundedReceiver<(Bytes, SocketAddr)>,
    pub send_tx: chan::Sender<(Bytes, SocketAddr)>,
}

impl NetUdpSocket {
    pub async fn recv(&mut self) -> Option<(Bytes, SocketAddr)> {
        self.recv_rx.recv().await
    }

    pub fn try_send(&self, data: Bytes, to: SocketAddr) -> Result<(), String> {
        self.send_tx
            .send((data, to))
            .map_err(|_| "udp send channel closed".to_string())
    }
}

// ── Commands (Tokio → smoltcp thread) ────────────────────────────────────────

enum Cmd {
    DialTcp {
        remote: SocketAddr,
        reply: oneshot::Sender<Result<NetTcpStream, String>>,
    },
    ListenTcp {
        port: u16,
        reply: oneshot::Sender<Result<NetTcpListener, String>>,
    },
    DialUdp {
        local_port: u16,
        reply: oneshot::Sender<Result<NetUdpSocket, String>>,
    },
    ListenUdp {
        port: u16,
        reply: oneshot::Sender<Result<NetUdpSocket, String>>,
    },
    /// Graceful shutdown (currently triggered by dropping the sender; kept for explicit use).
    #[allow(dead_code)]
    Stop,
}

// ── Internal bridge structs ───────────────────────────────────────────────────

struct TcpBridge {
    /// smoltcp → app (sends received data to Tokio side)
    to_app: mpsc::UnboundedSender<Bytes>,
    /// app → smoltcp (receives data written by Tokio side)
    from_app: chan::Receiver<Bytes>,
    close: Arc<AtomicBool>,
    /// Held until connection established, then taken to build NetTcpStream
    pending_parts: Option<(mpsc::UnboundedReceiver<Bytes>, chan::Sender<Bytes>)>,
}

struct TcpDialEntry {
    bridge: TcpBridge,
    reply: Option<oneshot::Sender<Result<NetTcpStream, String>>>,
}

struct TcpListenEntry {
    port: u16,
    accept_tx: mpsc::UnboundedSender<NetTcpStream>,
}

struct UdpEntry {
    to_app: mpsc::UnboundedSender<(Bytes, SocketAddr)>,
    from_app: chan::Receiver<(Bytes, SocketAddr)>,
}

// ── Netstack handle ───────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct Netstack {
    cmd_tx: chan::Sender<Cmd>,
}

impl Netstack {
    pub async fn dial_tcp(&self, remote: SocketAddr) -> Result<NetTcpStream, String> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(Cmd::DialTcp { remote, reply: tx })
            .map_err(|_| "netstack stopped")?;
        rx.await.map_err(|_| "netstack stopped".to_string())?
    }

    pub async fn listen_tcp(&self, port: u16) -> Result<NetTcpListener, String> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(Cmd::ListenTcp { port, reply: tx })
            .map_err(|_| "netstack stopped")?;
        rx.await.map_err(|_| "netstack stopped".to_string())?
    }

    pub async fn dial_udp(
        &self,
        local_port: u16,
    ) -> Result<NetUdpSocket, String> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(Cmd::DialUdp { local_port, reply: tx })
            .map_err(|_| "netstack stopped")?;
        rx.await.map_err(|_| "netstack stopped".to_string())?
    }

    pub async fn listen_udp(&self, port: u16) -> Result<NetUdpSocket, String> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(Cmd::ListenUdp { port, reply: tx })
            .map_err(|_| "netstack stopped")?;
        rx.await.map_err(|_| "netstack stopped".to_string())?
    }
}

// ── Constructor ───────────────────────────────────────────────────────────────

/// Create a `Netstack` backed by a `ReadWriteCloser`.
///
/// `our_addr` must be the node's Yggdrasil IPv6 address bytes (`core.address().0`).
pub fn create_netstack(rwc: Arc<ReadWriteCloser>, our_addr: [u8; 16]) -> Netstack {
    let mtu = rwc.mtu() as usize;

    // ygg RX: Tokio task reads raw packets from rwc → sends to smoltcp thread
    let (ygg_rx_tx, ygg_rx_rx) = chan::unbounded::<Vec<u8>>();

    // ygg TX: smoltcp thread writes packets to tokio mpsc → Tokio task writes to rwc
    let (ygg_tx_to_tok, mut ygg_tx_from_smol) = mpsc::unbounded_channel::<Vec<u8>>();

    // Commands: Tokio → smoltcp thread
    let (cmd_tx, cmd_rx) = chan::unbounded::<Cmd>();

    // Tokio task: read from rwc and forward to smoltcp
    {
        let rwc2 = rwc.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 65536];
            loop {
                match rwc2.read(&mut buf).await {
                    Ok(n) if n > 0 => {
                        let _ = ygg_rx_tx.send(buf[..n].to_vec());
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!("ygg rwc read error: {}", e);
                        break;
                    }
                }
            }
        });
    }

    // Tokio task: receive TX packets from smoltcp thread and write to rwc
    {
        let rwc2 = rwc.clone();
        tokio::spawn(async move {
            while let Some(pkt) = ygg_tx_from_smol.recv().await {
                if let Err(e) = rwc2.write(&pkt).await {
                    tracing::warn!("ygg rwc write error: {}", e);
                }
            }
        });
    }

    // Spawn smoltcp poll OS thread
    std::thread::spawn(move || {
        run_poll_loop(mtu, our_addr, ygg_rx_rx, ygg_tx_to_tok, cmd_rx);
    });

    Netstack { cmd_tx }
}

// ── Poll loop (OS thread) ─────────────────────────────────────────────────────

fn run_poll_loop(
    mtu: usize,
    our_addr: [u8; 16],
    ygg_rx: chan::Receiver<Vec<u8>>,
    ygg_tx: mpsc::UnboundedSender<Vec<u8>>,
    cmd_rx: chan::Receiver<Cmd>,
) {
    let mut device = YggDevice::new(mtu, ygg_tx);

    let config = Config::new(HardwareAddress::Ip);
    let now = smol_now();
    let mut iface = Interface::new(config, &mut device, now);

    let our_ipv6 = Ipv6Address::from_bytes(&our_addr);
    iface.update_ip_addrs(|addrs| {
        let _ = addrs.push(IpCidr::Ipv6(Ipv6Cidr::new(our_ipv6, 128)));
    });
    // Route all Yggdrasil addresses (0200::/7) through this interface.
    // With Medium::Ip the gateway field is ignored; we use our own address.
    iface.routes_mut().add_default_ipv6_route(our_ipv6).ok();
    // Add an explicit route for the Yggdrasil 0200::/7 prefix
    {
        let ygg_prefix_bytes = [0x02u8, 0,0,0,0,0,0,0,0,0,0,0,0,0,0,0];
        let ygg_cidr = Ipv6Cidr::new(Ipv6Address::from_bytes(&ygg_prefix_bytes), 7);
        iface.update_ip_addrs(|addrs| {
            // Also advertise our own address so smoltcp accepts packets destined to us
            let _ = addrs.push(IpCidr::Ipv6(ygg_cidr));
        });
    }

    let mut sockets = SocketSet::new(Vec::new());
    let mut next_port: u16 = 49152;

    let mut tcp_dial: HashMap<SocketHandle, TcpDialEntry> = HashMap::new();
    // Handles of listener sockets (entries may become active when connection arrives)
    let mut tcp_listen: HashMap<SocketHandle, TcpListenEntry> = HashMap::new();
    // Active connected sockets that were born from a listener
    let mut tcp_accepted: HashMap<SocketHandle, TcpBridge> = HashMap::new();
    // Pending connect requests: (handle, remote, local_port)
    let mut pending_connect: Vec<(SocketHandle, IpEndpoint, u16)> = Vec::new();

    let mut udp_sockets: HashMap<SocketHandle, UdpEntry> = HashMap::new();

    'main: loop {
        let now = smol_now();

        // ── 1. Handle pending connect calls (need iface.context()) ────────────
        for (handle, remote, local_port) in pending_connect.drain(..) {
            let mut cx = iface.context();
            let socket = sockets.get_mut::<TcpSocket>(handle);
            let local: IpListenEndpoint = local_port.into();
            if let Err(e) = socket.connect(&mut cx, remote, local) {
                tracing::warn!("TcpSocket::connect failed: {:?}", e);
                // Notify dial waiter of the error
                if let Some(mut entry) = tcp_dial.remove(&handle) {
                    if let Some(reply) = entry.reply.take() {
                        let _ = reply.send(Err(format!("{:?}", e)));
                    }
                }
                sockets.remove(handle);
            }
        }

        // ── 2. Inject pending RX packets ──────────────────────────────────────
        while let Ok(pkt) = ygg_rx.try_recv() {
            device.inject(pkt);
        }

        // ── 3. Poll smoltcp ───────────────────────────────────────────────────
        let _ = iface.poll(now, &mut device, &mut sockets);

        // ── 4. Process TCP dial sockets ───────────────────────────────────────
        let dial_handles: Vec<SocketHandle> = tcp_dial.keys().cloned().collect();
        for handle in dial_handles {
            let socket = sockets.get_mut::<TcpSocket>(handle);
            let entry = tcp_dial.get_mut(&handle).unwrap();

            // Connection established?
            if socket.may_send() && entry.reply.is_some() {
                // Build the NetTcpStream and send to waiter
                let reply = entry.reply.take().unwrap();
                if let Some((read_rx, write_tx)) = entry.bridge.pending_parts.take() {
                    let stream = NetTcpStream {
                        read_rx,
                        write_tx,
                        close: entry.bridge.close.clone(),
                        leftover: Bytes::new(),
                    };
                    let _ = reply.send(Ok(stream));
                }
            }

            // Check for connect failure
            if entry.reply.is_some() {
                let state = socket.state();
                if state == smoltcp::socket::tcp::State::Closed
                    || state == smoltcp::socket::tcp::State::TimeWait
                {
                    let reply = entry.reply.take().unwrap();
                    let _ = reply.send(Err("connection refused or timed out".into()));
                    tcp_dial.remove(&handle);
                    sockets.remove(handle);
                    continue;
                }
            }

            // Bridge data in both directions
            bridge_tcp(socket, &mut entry.bridge);

            // Check close
            if entry.bridge.close.load(Ordering::Relaxed) {
                socket.close();
            }

            // Remove if closed
            let state = socket.state();
            if state == smoltcp::socket::tcp::State::Closed
                || (state == smoltcp::socket::tcp::State::TimeWait && entry.bridge.close.load(Ordering::Relaxed))
            {
                tcp_dial.remove(&handle);
                sockets.remove(handle);
            }
        }

        // ── 5. Process TCP listener sockets ───────────────────────────────────
        let listen_handles: Vec<SocketHandle> = tcp_listen.keys().cloned().collect();
        for handle in listen_handles {
            let socket = sockets.get_mut::<TcpSocket>(handle);

            if socket.is_active() {
                // A new connection arrived – transition this socket to accepted
                let entry = tcp_listen.remove(&handle).unwrap();
                let port = entry.port;

                // Create bridge channels
                let (to_app, read_rx) = mpsc::unbounded_channel::<Bytes>();
                let (write_tx, from_app) = chan::unbounded::<Bytes>();
                let close = Arc::new(AtomicBool::new(false));

                let stream = NetTcpStream {
                    read_rx,
                    write_tx: write_tx.clone(),
                    close: close.clone(),
                    leftover: Bytes::new(),
                };
                let bridge = TcpBridge {
                    to_app,
                    from_app,
                    close,
                    pending_parts: None,
                };
                tcp_accepted.insert(handle, bridge);
                let _ = entry.accept_tx.send(stream);

                // Re-create a fresh listener at the same port
                let new_handle = new_tcp_listen_socket(&mut sockets, port);
                tcp_listen.insert(new_handle, TcpListenEntry { port, accept_tx: entry.accept_tx });
            }
        }

        // ── 6. Process TCP accepted (server-side) sockets ────────────────────
        let accepted_handles: Vec<SocketHandle> = tcp_accepted.keys().cloned().collect();
        for handle in accepted_handles {
            let socket = sockets.get_mut::<TcpSocket>(handle);
            let bridge = tcp_accepted.get_mut(&handle).unwrap();
            bridge_tcp(socket, bridge);
            if bridge.close.load(Ordering::Relaxed) {
                socket.close();
            }
            let state = socket.state();
            if state == smoltcp::socket::tcp::State::Closed {
                tcp_accepted.remove(&handle);
                sockets.remove(handle);
            }
        }

        // ── 7. Process UDP sockets ────────────────────────────────────────────
        let udp_handles: Vec<SocketHandle> = udp_sockets.keys().cloned().collect();
        for handle in udp_handles {
            let socket = sockets.get_mut::<UdpSocket>(handle);
            let entry = udp_sockets.get_mut(&handle).unwrap();

            // Receive datagrams from the Yggdrasil network
            while socket.can_recv() {
                match socket.recv() {
                    Ok((data, meta)) => {
                        let src = ipendpoint_to_sockaddr(meta.endpoint);
                        let _ = entry.to_app.send((Bytes::copy_from_slice(data), src));
                    }
                    Err(_) => break,
                }
            }

            // Send datagrams to the Yggdrasil network
            while let Ok((data, to)) = entry.from_app.try_recv() {
                let ep = sockaddr_to_ipendpoint(to);
                let _ = socket.send_slice(&data, ep);
            }
        }

        // ── 8. Compute delay and block ────────────────────────────────────────
        let poll_at = iface.poll_at(now, &sockets);
        let delay = match poll_at {
            Some(t) if t > now => {
                let millis = (t - now).total_millis().max(0) as u64;
                Duration::from_millis(millis.min(5))
            }
            Some(_) => Duration::ZERO,
            None => Duration::from_millis(5),
        };

        // Block until a new ygg packet, a command, or the timer fires.
        // IMPORTANT: if we receive a command here we handle it immediately
        // (instead of just waking up) to avoid losing it.
        chan::select! {
            recv(ygg_rx) -> res => {
                if let Ok(pkt) = res { device.inject(pkt); }
            }
            recv(cmd_rx) -> res => {
                match res {
                    Ok(Cmd::Stop) => break 'main,
                    Ok(cmd) => {
                        // Process the command inline
                        process_cmd(
                            cmd,
                            &mut sockets,
                            &mut tcp_dial,
                            &mut tcp_listen,
                            &mut udp_sockets,
                            &mut pending_connect,
                            &mut next_port,
                            our_ipv6,
                        );
                    }
                    Err(_) => break 'main,
                }
            }
            default(delay) => {}
        }
    }
}

/// Process a single command in the smoltcp thread.
#[allow(clippy::too_many_arguments)]
fn process_cmd(
    cmd: Cmd,
    sockets: &mut SocketSet<'_>,
    tcp_dial: &mut HashMap<SocketHandle, TcpDialEntry>,
    tcp_listen: &mut HashMap<SocketHandle, TcpListenEntry>,
    udp_sockets: &mut HashMap<SocketHandle, UdpEntry>,
    pending_connect: &mut Vec<(SocketHandle, IpEndpoint, u16)>,
    next_port: &mut u16,
    our_ipv6: Ipv6Address,
) {
    match cmd {
        Cmd::Stop => {} // handled outside

        Cmd::DialTcp { remote, reply } => {
            let local_port = alloc_port(next_port);
            let handle = new_tcp_socket(sockets);
            let (to_app, read_rx) = mpsc::unbounded_channel::<Bytes>();
            let (write_tx, from_app) = chan::unbounded::<Bytes>();
            let close = Arc::new(AtomicBool::new(false));
            tcp_dial.insert(
                handle,
                TcpDialEntry {
                    bridge: TcpBridge {
                        to_app,
                        from_app,
                        close: close.clone(),
                        pending_parts: Some((read_rx, write_tx)),
                    },
                    reply: Some(reply),
                },
            );
            pending_connect.push((handle, sockaddr_to_ipendpoint(remote), local_port));
        }

        Cmd::ListenTcp { port, reply } => {
            let handle = new_tcp_listen_socket(sockets, port);
            let (accept_tx, accept_rx) = mpsc::unbounded_channel::<NetTcpStream>();
            tcp_listen.insert(handle, TcpListenEntry { port, accept_tx });
            let _ = reply.send(Ok(NetTcpListener { accept_rx }));
        }

        Cmd::DialUdp { local_port, reply } => {
            let handle = new_udp_socket(sockets, local_port, Some(our_ipv6));
            let (to_app, recv_rx) = mpsc::unbounded_channel::<(Bytes, SocketAddr)>();
            let (send_tx, from_app) = chan::unbounded::<(Bytes, SocketAddr)>();
            udp_sockets.insert(handle, UdpEntry { to_app, from_app });
            let _ = reply.send(Ok(NetUdpSocket { recv_rx, send_tx }));
        }

        Cmd::ListenUdp { port, reply } => {
            let handle = new_udp_socket(sockets, port, None);
            let (to_app, recv_rx) = mpsc::unbounded_channel::<(Bytes, SocketAddr)>();
            let (send_tx, from_app) = chan::unbounded::<(Bytes, SocketAddr)>();
            udp_sockets.insert(handle, UdpEntry { to_app, from_app });
            let _ = reply.send(Ok(NetUdpSocket { recv_rx, send_tx }));
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn smol_now() -> SmolInstant {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    SmolInstant::from_millis(d.as_millis() as i64)
}

fn alloc_port(next: &mut u16) -> u16 {
    let p = *next;
    *next = if *next >= 60000 { 49152 } else { *next + 1 };
    p
}

fn new_tcp_socket(sockets: &mut SocketSet<'_>) -> SocketHandle {
    let rx = TcpSocketBuffer::new(vec![0u8; 64 * 1024]);
    let tx = TcpSocketBuffer::new(vec![0u8; 64 * 1024]);
    let mut socket = TcpSocket::new(rx, tx);
    socket.set_nagle_enabled(false);
    socket.set_keep_alive(Some(smoltcp::time::Duration::from_secs(30)));
    sockets.add(socket)
}

fn new_tcp_listen_socket(sockets: &mut SocketSet<'_>, port: u16) -> SocketHandle {
    let handle = new_tcp_socket(sockets);
    let socket = sockets.get_mut::<TcpSocket>(handle);
    socket.listen(port).ok();
    handle
}

fn new_udp_socket(
    sockets: &mut SocketSet<'_>,
    port: u16,
    addr: Option<Ipv6Address>,
) -> SocketHandle {
    let rx_meta = vec![UdpPacketMetadata::EMPTY; 64];
    let rx_data = vec![0u8; 128 * 1024];
    let tx_meta = vec![UdpPacketMetadata::EMPTY; 64];
    let tx_data = vec![0u8; 128 * 1024];
    let mut socket = UdpSocket::new(
        UdpPacketBuffer::new(rx_meta, rx_data),
        UdpPacketBuffer::new(tx_meta, tx_data),
    );
    let ep = IpListenEndpoint {
        addr: addr.map(IpAddress::Ipv6),
        port,
    };
    socket.bind(ep).ok();
    sockets.add(socket)
}

/// Bidirectional bridge between a running TCP socket and a TcpBridge.
fn bridge_tcp(socket: &mut TcpSocket, bridge: &mut TcpBridge) {
    // smoltcp → app
    if socket.can_recv() {
        let _ = socket.recv(|data| {
            if !data.is_empty() {
                let _ = bridge.to_app.send(Bytes::copy_from_slice(data));
            }
            (data.len(), ())
        });
    }
    // app → smoltcp
    if socket.can_send() {
        while let Ok(data) = bridge.from_app.try_recv() {
            match socket.send_slice(&data) {
                Ok(n) if n == data.len() => {}
                _ => break, // buffer full or error; drop remaining
            }
        }
    }
}

fn sockaddr_to_ipendpoint(addr: SocketAddr) -> IpEndpoint {
    match addr {
        SocketAddr::V6(v6) => IpEndpoint {
            addr: IpAddress::Ipv6(Ipv6Address::from_bytes(&v6.ip().octets())),
            port: v6.port(),
        },
        SocketAddr::V4(_v4) => {
            // Yggdrasil is IPv6-only; IPv4 targets are not supported.
            IpEndpoint {
                addr: IpAddress::Ipv6(Ipv6Address::UNSPECIFIED),
                port: 0,
            }
        }
    }
}

fn ipendpoint_to_sockaddr(ep: IpEndpoint) -> SocketAddr {
    let port = ep.port;
    match ep.addr {
        IpAddress::Ipv6(a) => SocketAddr::new(IpAddr::V6(Ipv6Addr::from(a.0)), port),
        #[allow(unreachable_patterns)]
        _ => SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), port),
    }
}
