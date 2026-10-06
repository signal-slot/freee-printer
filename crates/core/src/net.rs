//! Outbound connections, threads and persistent settings are supplied by the
//! host: rustls and files on a PC, `esp_tls` and NVS on a microcontroller.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

pub const DIAL_TIMEOUT: Duration = Duration::from_secs(15);

/// A blocking, timeout-capable byte stream.
pub trait Conn: Read + Write + Send {
    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()>;
}

impl Conn for TcpStream {
    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        TcpStream::set_read_timeout(self, timeout)
    }
}

/// Opens outbound TCP and TLS connections.
pub trait Dialer: Send + Sync {
    fn dial_tcp(&self, host: &str, port: u16) -> io::Result<Box<dyn Conn>>;
    /// TLS with the certificate checked against `host`.
    fn dial_tls(&self, host: &str, port: u16) -> io::Result<Box<dyn Conn>>;
}

/// Persistent key/value settings. Keys stay within 15 characters so that
/// NVS on the ESP32 can hold them.
pub trait Store: Send + Sync {
    fn get(&self, key: &str) -> Option<String>;
    fn set(&self, key: &str, value: &str) -> io::Result<()>;
    fn remove(&self, key: &str) -> io::Result<()>;
    /// Keeps other processes that share the same storage out until the
    /// returned guard is dropped. Token refreshes run under it, because a
    /// refresh token works only once.
    fn lock(&self) -> io::Result<Box<dyn std::any::Any + Send>> {
        Ok(Box::new(()))
    }
}

/// Spawns a named thread with the given stack size. Embedded ports use this
/// to choose where stacks live.
pub type Spawner =
    Arc<dyn Fn(&str, usize, Box<dyn FnOnce() + Send>) -> io::Result<()> + Send + Sync>;

pub fn std_spawner() -> Spawner {
    Arc::new(|name: &str, stack: usize, f: Box<dyn FnOnce() + Send>| {
        std::thread::Builder::new()
            .name(name.to_string())
            .stack_size(stack)
            .spawn(f)
            .map(|_| ())
    })
}

/// Resolves `host:port` and connects with a timeout, trying IPv4 first.
pub fn connect_tcp(host: &str, port: u16, timeout: Duration) -> io::Result<TcpStream> {
    let mut addrs: Vec<SocketAddr> = (host, port).to_socket_addrs()?.collect();
    addrs.sort_by_key(|a| a.is_ipv6());
    let mut last = io::Error::new(io::ErrorKind::NotFound, "no addresses");
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(stream) => {
                stream.set_nodelay(true).ok();
                stream.set_write_timeout(Some(timeout)).ok();
                return Ok(stream);
            }
            Err(e) => last = e,
        }
    }
    Err(last)
}

pub fn is_timeout(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}
