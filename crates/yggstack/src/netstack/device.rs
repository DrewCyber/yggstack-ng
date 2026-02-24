use std::collections::VecDeque;

use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::time::Instant;
use tokio::sync::mpsc;

/// A smoltcp `Device` that exchanges raw IPv6 packets with the Yggdrasil
/// `ReadWriteCloser`.
///
/// * **RX** – The smoltcp poll loop injects packets via [`YggDevice::inject`].
///   `Interface::poll` drains them through [`Device::receive`].
/// * **TX** – smoltcp calls [`Device::transmit`]; the resulting [`YggTxToken`]
///   sends each packet to the async Tokio side via an `UnboundedSender`.
pub struct YggDevice {
    /// Inbound packets waiting to be consumed by smoltcp.
    pub rx_queue: VecDeque<Vec<u8>>,
    /// Channel for outbound packets (smoltcp → Tokio → rwc.write).
    tx_sender: mpsc::UnboundedSender<Vec<u8>>,
    mtu: usize,
}

impl YggDevice {
    pub fn new(mtu: usize, tx_sender: mpsc::UnboundedSender<Vec<u8>>) -> Self {
        Self {
            rx_queue: VecDeque::new(),
            tx_sender,
            mtu,
        }
    }

    /// Enqueue a raw IPv6 packet for smoltcp to process.
    pub fn inject(&mut self, packet: Vec<u8>) {
        self.rx_queue.push_back(packet);
    }
}

// ── RX token ─────────────────────────────────────────────────────────────────

pub struct YggRxToken(pub Vec<u8>);

impl RxToken for YggRxToken {
    fn consume<R, F>(mut self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        f(&mut self.0)
    }
}

// ── TX token ─────────────────────────────────────────────────────────────────

/// Holds a clone of the channel sender; sending happens inside `consume`.
pub struct YggTxToken(mpsc::UnboundedSender<Vec<u8>>);

impl TxToken for YggTxToken {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buf = vec![0u8; len];
        let result = f(&mut buf);
        // Best-effort send; ignore errors on shutdown.
        let _ = self.0.send(buf);
        result
    }
}

// ── Device impl ───────────────────────────────────────────────────────────────

impl Device for YggDevice {
    type RxToken<'a> = YggRxToken where Self: 'a;
    type TxToken<'a> = YggTxToken where Self: 'a;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        self.rx_queue.pop_front().map(|pkt| {
            (YggRxToken(pkt), YggTxToken(self.tx_sender.clone()))
        })
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(YggTxToken(self.tx_sender.clone()))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ip;
        caps.max_transmission_unit = self.mtu;
        caps
    }
}
