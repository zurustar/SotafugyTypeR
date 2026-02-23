// UDP transport module

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use tokio::net::UdpSocket;

use crate::error::SipLoadTestError;

pub struct UdpTransport {
    sockets: Vec<Arc<UdpSocket>>,
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

        Ok(Self { sockets })
    }

    /// Send data via the first socket.
    pub async fn send_to(
        &self,
        data: &[u8],
        addr: SocketAddr,
    ) -> Result<(), SipLoadTestError> {
        self.sockets[0].send_to(data, addr).await?;
        Ok(())
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

        let mut buf = vec![0u8; 65535];
        let (len, from) = socket.recv_from(&mut buf).await?;
        buf.truncate(len);
        Ok((buf, from))
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
}
