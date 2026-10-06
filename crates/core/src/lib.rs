//! `freee-printer-core`: an IPP Everywhere printer whose output tray is the
//! freee file box.
//!
//! Everything here is blocking code on plain threads, so the same crate can
//! run in the CLI and in a firmware for a dedicated device. Layers, bottom up:
//! - [`ipp`], [`raster`], [`icon`]: formats.
//! - [`net`], [`stream`], [`http`]: connections and the HTTP/1.1 server and client.
//! - [`freee`]: OAuth2 and the file box API.
//! - [`printer`]: IPP operations and job handling.
//! - [`ui`]: the web pages for status, setup and settings.
//! - [`setup`]: the interactive setup wizard.

pub mod freee;
pub mod http;
pub mod icon;
pub mod ipp;
pub mod net;
pub mod printer;
pub mod raster;
pub mod setup;
pub mod stream;
pub mod ui;
