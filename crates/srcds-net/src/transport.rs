//! Blocking UDP transport with a bounded receive timeout, used by the
//! single-threaded server loop.

use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::time::Duration;

use srcds_protocol::MAX_DATAGRAM;

pub struct UdpTransport {
    socket: UdpSocket,
    buf: Box<[u8]>,
}

/// Result of one receive attempt.
#[derive(Debug)]
pub enum RecvOutcome<'a> {
    Datagram {
        from: SocketAddr,
        data: &'a [u8],
    },
    /// Timeout elapsed, or a spurious wakeup (e.g. Windows ICMP reset) occurred.
    Idle,
}

impl UdpTransport {
    pub fn bind(addr: SocketAddr) -> io::Result<Self> {
        let socket = UdpSocket::bind(addr)?;
        Ok(Self {
            socket,
            buf: vec![0u8; MAX_DATAGRAM].into_boxed_slice(),
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// Waits at most `timeout` (clamped to >= 1ms) for one datagram. The returned
    /// slice borrows the transport's reusable buffer (no per-packet allocation).
    pub fn recv(&mut self, timeout: Duration) -> io::Result<RecvOutcome<'_>> {
        let timeout = timeout.max(Duration::from_millis(1));
        self.socket.set_read_timeout(Some(timeout))?;
        match self.socket.recv_from(&mut self.buf) {
            Ok((n, from)) => Ok(RecvOutcome::Datagram {
                from,
                data: &self.buf[..n],
            }),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                Ok(RecvOutcome::Idle)
            }
            // On Windows an ICMP "port unreachable" for a previous send surfaces as
            // WSAECONNRESET on the next recv. It is not fatal for a server socket.
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => Ok(RecvOutcome::Idle),
            // Datagram larger than the buffer (cannot happen with MAX_DATAGRAM, but on
            // Windows WSAEMSGSIZE is reported as an error rather than truncation).
            Err(e) if e.raw_os_error() == Some(10040) => Ok(RecvOutcome::Idle),
            Err(e) => Err(e),
        }
    }

    pub fn send_to(&self, data: &[u8], to: SocketAddr) -> io::Result<usize> {
        self.socket.send_to(data, to)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_send_receive() {
        let mut server = UdpTransport::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        client
            .send_to(b"hello", server.local_addr().unwrap())
            .unwrap();
        let from = match server.recv(Duration::from_secs(2)).unwrap() {
            RecvOutcome::Datagram { from, data } => {
                assert_eq!(data, b"hello");
                from
            }
            RecvOutcome::Idle => panic!("timed out"),
        };
        assert_eq!(from, client.local_addr().unwrap());
        server.send_to(b"world", from).unwrap();
        let mut b = [0u8; 16];
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let (n, _) = client.recv_from(&mut b).unwrap();
        assert_eq!(&b[..n], b"world");
    }

    #[test]
    fn idle_on_timeout() {
        let mut server = UdpTransport::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        assert!(matches!(
            server.recv(Duration::from_millis(20)).unwrap(),
            RecvOutcome::Idle
        ));
    }
}
