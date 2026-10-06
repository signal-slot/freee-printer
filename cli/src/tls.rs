//! rustls-backed dialer for the freee client.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use freee_printer_core::net::{Conn, DIAL_TIMEOUT, Dialer, connect_tcp};

pub struct HostDialer {
    tls_config: Arc<rustls::ClientConfig>,
}

impl HostDialer {
    pub fn new() -> Self {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        HostDialer {
            tls_config: Arc::new(config),
        }
    }
}

struct TlsConn {
    inner: rustls::StreamOwned<rustls::ClientConnection, TcpStream>,
}

impl Read for TlsConn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.inner.read(buf) {
            // Servers that close without close_notify after a complete
            // response are common; the HTTP layer checks lengths itself.
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(0),
            other => other,
        }
    }
}

impl Write for TlsConn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl Conn for TlsConn {
    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        self.inner.sock.set_read_timeout(timeout)
    }
}

impl HostDialer {
    fn tls(&self, sni: &str, target: &str, port: u16) -> io::Result<Box<dyn Conn>> {
        let tcp = connect_tcp(target, port, DIAL_TIMEOUT)?;
        let name = rustls::pki_types::ServerName::try_from(sni.to_string())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "bad sni"))?;
        let conn = rustls::ClientConnection::new(self.tls_config.clone(), name)
            .map_err(|e| io::Error::other(format!("tls: {e}")))?;
        let mut stream = rustls::StreamOwned::new(conn, tcp);
        // Complete the handshake eagerly so later errors are I/O errors only.
        stream.sock.set_read_timeout(Some(DIAL_TIMEOUT))?;
        while stream.conn.is_handshaking() {
            stream.conn.complete_io(&mut stream.sock)?;
        }
        Ok(Box::new(TlsConn { inner: stream }))
    }
}

impl Dialer for HostDialer {
    fn dial_tcp(&self, host: &str, port: u16) -> io::Result<Box<dyn Conn>> {
        Ok(Box::new(connect_tcp(host, port, DIAL_TIMEOUT)?))
    }

    fn dial_tls(&self, host: &str, port: u16) -> io::Result<Box<dyn Conn>> {
        self.tls(host, host, port)
    }
}
