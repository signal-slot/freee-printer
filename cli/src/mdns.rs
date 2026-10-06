//! DNS-SD announcement so that other devices on the network find the printer.

use anyhow::Result;
use mdns_sd::{ServiceDaemon, ServiceInfo};
use uuid::Uuid;

use freee_printer_core::printer::URF_CAPABILITIES;

/// Keeps the announcement alive; the service is withdrawn on drop.
pub struct Advertisement {
    daemon: ServiceDaemon,
    fullname: String,
}

pub fn advertise(name: &str, port: u16, uuid: Uuid) -> Result<Advertisement> {
    let host = crate::host_name();
    let host = host
        .split('.')
        .next()
        .unwrap_or("freee-printer")
        .to_string();
    let admin_url = format!("http://{host}.local:{port}/");
    let urf = URF_CAPABILITIES.join(",");
    let properties = [
        ("txtvers", "1"),
        ("qtotal", "1"),
        ("rp", "ipp/print"),
        ("ty", "freee File Box"),
        ("product", "(freee File Box)"),
        ("note", ""),
        ("adminurl", admin_url.as_str()),
        (
            "pdl",
            "application/pdf,image/jpeg,image/png,image/pwg-raster,image/urf",
        ),
        ("kind", "document"),
        ("Color", "T"),
        ("Duplex", "F"),
        ("URF", urf.as_str()),
        ("UUID", &uuid.to_string()),
        ("priority", "50"),
    ];
    // The _universal subtype is what AirPrint clients browse for; everyone
    // else finds the plain _ipp._tcp record.
    let info = ServiceInfo::new(
        "_universal._sub._ipp._tcp.local.",
        name,
        &format!("{host}.local."),
        "",
        port,
        &properties[..],
    )?
    .enable_addr_auto();
    let fullname = info.get_fullname().to_string();
    let daemon = ServiceDaemon::new()?;
    daemon.register(info)?;
    Ok(Advertisement { daemon, fullname })
}

impl Drop for Advertisement {
    fn drop(&mut self) {
        // Wait briefly so the goodbye packet actually goes out.
        if let Ok(done) = self.daemon.unregister(&self.fullname) {
            done.recv_timeout(std::time::Duration::from_secs(1)).ok();
        }
        self.daemon.shutdown().ok();
    }
}
