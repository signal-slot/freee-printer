//! Inbound connections. The printer serves the same way on an OS socket and
//! on a userspace TCP stack.

use std::io::{self, Read, Write};
use std::time::{Duration, Instant};

/// A blocking byte stream whose reads time out.
pub trait Stream: Send {
    /// Returns `Ok(0)` once the peer has closed and `TimedOut` if nothing
    /// arrived within `timeout`.
    fn read(&mut self, buf: &mut [u8], timeout: Duration) -> io::Result<usize>;
    fn write_all(&mut self, data: &[u8]) -> io::Result<()>;
    fn peer(&self) -> String;
}

pub trait Listener: Send + Sync {
    /// Waits up to `timeout` for a new connection.
    fn accept(&self, timeout: Duration) -> Option<Box<dyn Stream>>;
}

impl Stream for std::net::TcpStream {
    fn read(&mut self, buf: &mut [u8], timeout: Duration) -> io::Result<usize> {
        self.set_read_timeout(Some(timeout))?;
        match Read::read(self, buf) {
            // Unix reports an expired SO_RCVTIMEO as WouldBlock.
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Err(io::ErrorKind::TimedOut.into()),
            other => other,
        }
    }

    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        Write::write_all(self, data)
    }

    fn peer(&self) -> String {
        self.peer_addr().map(|a| a.to_string()).unwrap_or_default()
    }
}

/// An OS listener polled without blocking so that `accept` can time out.
pub struct OsListener(std::net::TcpListener);

impl OsListener {
    pub fn bind(addr: std::net::SocketAddr) -> io::Result<Self> {
        let listener = std::net::TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        Ok(OsListener(listener))
    }

    pub fn local_addr(&self) -> io::Result<std::net::SocketAddr> {
        self.0.local_addr()
    }
}

impl Listener for OsListener {
    fn accept(&self, timeout: Duration) -> Option<Box<dyn Stream>> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.0.accept() {
                Ok((stream, _)) => {
                    // The accepted socket may inherit the non-blocking flag.
                    stream.set_nonblocking(false).ok()?;
                    stream.set_nodelay(true).ok();
                    return Some(Box::new(stream));
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return None;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => {
                    log::warn!("accept: {e}");
                    std::thread::sleep(Duration::from_millis(200));
                    return None;
                }
            }
        }
    }
}
