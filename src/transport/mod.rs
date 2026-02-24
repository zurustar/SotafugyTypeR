// UDP transport module

pub mod batch;

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::net::UdpSocket;

use crate::error::SipLoadTestError;

pub struct UdpTransport {
    sockets: Vec<Arc<UdpSocket>>,
    send_idx: AtomicUsize,
}

impl UdpTransport {
    /// Bind `count` UDP sockets starting from `base_port`.
    /// If `base_port` is 0, the OS assigns ephemeral ports for each socket.
    /// If `base_port` is non-zero, sockets bind to sequential ports
    /// (base_port, base_port+1, ...).
    pub async fn bind(
        base_addr: IpAddr,
        base_port: u16,
        count: u16,
    ) -> Result<Self, SipLoadTestError> {
        if count == 0 {
            return Err(SipLoadTestError::ConfigError(
                "socket count must be at least 1".to_string(),
            ));
        }

        let mut sockets = Vec::with_capacity(count as usize);
        for i in 0..count {
            let port = if base_port == 0 { 0 } else { base_port + i };
            let addr = SocketAddr::new(base_addr, port);
            let socket = UdpSocket::bind(addr).await?;
            sockets.push(Arc::new(socket));
        }

        Ok(Self {
            sockets,
            send_idx: AtomicUsize::new(0),
        })
    }

    /// Send data via round-robin socket selection.
    pub async fn send_to(
        &self,
        data: &[u8],
        addr: SocketAddr,
    ) -> Result<(), SipLoadTestError> {
        let idx = self.send_idx.fetch_add(1, Ordering::Relaxed) % self.sockets.len();
        self.sockets[idx].send_to(data, addr).await?;
        Ok(())
    }

    /// Returns the local address of the specified socket.
    pub fn local_addr(&self, socket_idx: usize) -> Option<SocketAddr> {
        self.sockets.get(socket_idx).and_then(|s| s.local_addr().ok())
    }

    /// Returns the number of bound sockets.
    pub fn socket_count(&self) -> usize {
        self.sockets.len()
    }

    /// Receive data from the specified socket index.
    pub async fn recv_from(
        &self,
        socket_idx: usize,
    ) -> Result<(Vec<u8>, SocketAddr), SipLoadTestError> {
        let socket = self.sockets.get(socket_idx).ok_or_else(|| {
            SipLoadTestError::ConfigError(format!(
                "socket index {} out of range (have {})",
                socket_idx,
                self.sockets.len()
            ))
        })?;

        // Stack-allocated buffer to avoid heap allocation per recv
        let mut buf = [0u8; 65535];
        let (len, from) = socket.recv_from(&mut buf).await?;
        Ok((buf[..len].to_vec(), from))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[tokio::test]
    async fn bind_single_socket() {
        let transport = UdpTransport::bind(
            IpAddr::V4(Ipv4Addr::LOCALHOST), 0, 1,
        ).await.expect("should bind one socket");
        assert_eq!(transport.sockets.len(), 1);
    }

    #[tokio::test]
    async fn bind_multiple_sockets() {
        let transport = UdpTransport::bind(
            IpAddr::V4(Ipv4Addr::LOCALHOST), 0, 3,
        ).await.expect("should bind three sockets");
        assert_eq!(transport.sockets.len(), 3);
    }

    #[tokio::test]
    async fn socket_count_returns_number_of_sockets() {
        let t1 = UdpTransport::bind(
            IpAddr::V4(Ipv4Addr::LOCALHOST), 0, 1,
        ).await.expect("bind 1");
        assert_eq!(t1.socket_count(), 1);

        let t3 = UdpTransport::bind(
            IpAddr::V4(Ipv4Addr::LOCALHOST), 0, 3,
        ).await.expect("bind 3");
        assert_eq!(t3.socket_count(), 3);
    }

    #[tokio::test]
    async fn bind_zero_count_returns_error() {
        let result = UdpTransport::bind(
            IpAddr::V4(Ipv4Addr::LOCALHOST), 0, 0,
        ).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn send_and_recv_roundtrip() {
        let sender = UdpTransport::bind(
            IpAddr::V4(Ipv4Addr::LOCALHOST), 0, 1,
        ).await.expect("sender bind");
        let receiver = UdpTransport::bind(
            IpAddr::V4(Ipv4Addr::LOCALHOST), 0, 1,
        ).await.expect("receiver bind");
        let recv_addr = receiver.sockets[0].local_addr().unwrap();

        let payload = b"SIP/2.0 200 OK\r\n\r\n";
        sender.send_to(payload, recv_addr).await.expect("send");

        let (data, from) = receiver.recv_from(0).await.expect("recv");
        assert_eq!(&data, payload);
        assert_eq!(from.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
    }

    #[tokio::test]
    async fn recv_from_invalid_index_returns_error() {
        let transport = UdpTransport::bind(
            IpAddr::V4(Ipv4Addr::LOCALHOST), 0, 1,
        ).await.expect("bind");
        let result = transport.recv_from(5).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn send_to_uses_first_socket() {
        let transport = UdpTransport::bind(
            IpAddr::V4(Ipv4Addr::LOCALHOST), 0, 3,
        ).await.expect("bind");
        let receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let recv_addr = receiver.local_addr().unwrap();

        transport.send_to(b"test", recv_addr).await.expect("send");

        let mut buf = vec![0u8; 1500];
        let (len, from) = receiver.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..len], b"test");
        let first_addr = transport.sockets[0].local_addr().unwrap();
        assert_eq!(from, first_addr);
    }

    #[tokio::test]
    async fn bind_with_specific_port() {
        let tmp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = tmp.local_addr().unwrap().port();
        drop(tmp);

        let transport = UdpTransport::bind(
            IpAddr::V4(Ipv4Addr::LOCALHOST), port, 1,
        ).await.expect("bind to specific port");
        assert_eq!(transport.sockets[0].local_addr().unwrap().port(), port);
    }

    #[tokio::test]
    async fn multiple_sockets_have_sequential_ports() {
        let tmp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = tmp.local_addr().unwrap().port();
        drop(tmp);

        if let Ok(transport) = UdpTransport::bind(
            IpAddr::V4(Ipv4Addr::LOCALHOST), port, 2,
        ).await {
            let p0 = transport.sockets[0].local_addr().unwrap().port();
            let p1 = transport.sockets[1].local_addr().unwrap().port();
            assert_eq!(p0, port);
            assert_eq!(p1, port + 1);
        }
    }

    #[tokio::test]
    async fn send_empty_data() {
        let sender = UdpTransport::bind(
            IpAddr::V4(Ipv4Addr::LOCALHOST), 0, 1,
        ).await.expect("bind");
        let receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let recv_addr = receiver.local_addr().unwrap();

        sender.send_to(b"", recv_addr).await.expect("send empty");
        let mut buf = vec![0u8; 1500];
        let (len, _) = receiver.recv_from(&mut buf).await.unwrap();
        assert_eq!(len, 0);
    }

    #[tokio::test]
    async fn send_to_round_robin_distributes_across_sockets() {
        let transport = UdpTransport::bind(
            IpAddr::V4(Ipv4Addr::LOCALHOST), 0, 3,
        ).await.expect("bind");

        // Create 3 receivers
        let r0 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let r1 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let r2 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr0 = r0.local_addr().unwrap();
        let addr1 = r1.local_addr().unwrap();
        let addr2 = r2.local_addr().unwrap();

        // Send 3 messages — each should go via a different socket
        transport.send_to(b"msg0", addr0).await.expect("send 0");
        transport.send_to(b"msg1", addr1).await.expect("send 1");
        transport.send_to(b"msg2", addr2).await.expect("send 2");

        let mut buf = vec![0u8; 1500];

        let (len, from0) = r0.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..len], b"msg0");

        let (len, from1) = r1.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..len], b"msg1");

        let (len, from2) = r2.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..len], b"msg2");

        // Each message should come from a different socket (round-robin)
        let sock0_addr = transport.sockets[0].local_addr().unwrap();
        let sock1_addr = transport.sockets[1].local_addr().unwrap();
        let sock2_addr = transport.sockets[2].local_addr().unwrap();
        assert_eq!(from0, sock0_addr, "first send should use socket 0");
        assert_eq!(from1, sock1_addr, "second send should use socket 1");
        assert_eq!(from2, sock2_addr, "third send should use socket 2");
    }

    #[tokio::test]
    async fn send_to_round_robin_wraps_around() {
        let transport = UdpTransport::bind(
            IpAddr::V4(Ipv4Addr::LOCALHOST), 0, 2,
        ).await.expect("bind");

        let receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let recv_addr = receiver.local_addr().unwrap();

        let sock0_addr = transport.sockets[0].local_addr().unwrap();
        let sock1_addr = transport.sockets[1].local_addr().unwrap();

        let mut buf = vec![0u8; 1500];

        // Send 4 messages with 2 sockets: should cycle 0, 1, 0, 1
        for i in 0..4u8 {
            transport.send_to(&[i], recv_addr).await.expect("send");
            let (len, from) = receiver.recv_from(&mut buf).await.unwrap();
            assert_eq!(len, 1);
            let expected_addr = if i % 2 == 0 { sock0_addr } else { sock1_addr };
            assert_eq!(from, expected_addr, "message {} should use socket {}", i, i % 2);
        }
    }

}

#[cfg(test)]
mod proptests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use proptest::prelude::*;

    /// Simulate the round-robin index selection logic used by UdpTransport::send_to.
    /// Returns a vector of usage counts per socket.
    fn simulate_round_robin(n_sockets: usize, n_sends: usize) -> Vec<usize> {
        let counter = AtomicUsize::new(0);
        let mut counts = vec![0usize; n_sockets];
        for _ in 0..n_sends {
            let idx = counter.fetch_add(1, Ordering::Relaxed) % n_sockets;
            counts[idx] += 1;
        }
        counts
    }

    proptest! {
        /// Feature: performance-bottleneck-optimization, Property 4: ラウンドロビン送信の均等性
        /// **Validates: Requirements 3.1**
        #[test]
        fn prop_round_robin_balanced(
            n_sockets in 2usize..=64,
            n_sends in 0usize..=1000,
        ) {
            let counts = simulate_round_robin(n_sockets, n_sends);

            // All counts should sum to n_sends
            let total: usize = counts.iter().sum();
            prop_assert_eq!(total, n_sends, "total sends must equal n_sends");

            if n_sends > 0 {
                let max_count = *counts.iter().max().unwrap();
                let min_count = *counts.iter().min().unwrap();
                prop_assert!(
                    max_count - min_count <= 1,
                    "usage difference must be at most 1, got max={} min={} (n_sockets={}, n_sends={})",
                    max_count, min_count, n_sockets, n_sends
                );
            }
        }
    }
}

