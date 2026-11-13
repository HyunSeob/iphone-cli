//! A userspace TCP stack that runs over any [`ReadWrite`] transport.
//!
//! iphone-cli: this is jktcp 0.1.8 by Jackson Coxson (MIT, see LICENSE.jktcp),
//! taken into this crate so that it can be published without a `[patch]`.
//! Changed here: two retransmission constants in `adapter.rs`, the capture's
//! microseconds in `log_packet`, packet capture always built in, and the
//! TUN-device tests left out.
//!
//! `jktcp` is intentionally simplified: it targets **reliable, ordered transports**
//! (e.g. CDTunnel, a Unix socket carrying raw IP frames) where real packet loss is
//! rare. It still implements stop-and-wait retransmission with exponential back-off
//! so that transient glitches do not silently drop data.
//!
//! # Two usage patterns
//!
//! ## 1. `AdapterStream` — single-threaded, lifetime-bound
//!
//! [`stream::AdapterStream`] holds a `&mut` reference to the [`adapter::Adapter`],
//! so it can only be used from the same task that owns the adapter.  One stream is
//! alive at a time; while it exists the adapter is exclusively borrowed.
//!
//! **Choose this when** you open exactly one connection, drive it entirely from a
//! single async task, and do not need to move the stream across tasks or store it
//! on the heap alongside other things that reference the adapter.
//!
//! ```rust,ignore
//! use iphone_cli::tcp::adapter::Adapter;
//! use iphone_cli::tcp::stream::AdapterStream;
//! use tokio::io::{AsyncReadExt, AsyncWriteExt};
//! # use std::net::{IpAddr, Ipv6Addr};
//!
//! # async fn example(transport: impl iphone_cli::tcp::ReadWrite + 'static) -> std::io::Result<()> {
//! let mut adapter = Adapter::new(Box::new(transport), todo!(), todo!());
//!
//! let mut stream = AdapterStream::connect(&mut adapter, 1234).await?;
//! stream.write_all(b"hello").await?;
//!
//! let mut buf = [0u8; 32];
//! let n = stream.read(&mut buf).await?;
//! println!("received: {:?}", &buf[..n]);
//!
//! stream.close().await?;
//! # Ok(())
//! # }
//! ```
//!
//! ## 2. `AdapterHandle` / `StreamHandle` — multi-threaded, `'static`
//!
//! [`handle::AdapterHandle`] spawns the adapter's I/O loop onto the Tokio runtime.
//! All interaction happens through channels, so the resulting [`handle::StreamHandle`]
//! is `Send + Sync + 'static` and can be stored in `Arc`, passed across tasks, or
//! boxed as a trait object.  Multiple streams can be open simultaneously.
//!
//! **Choose this when** any of the following apply:
//! - You need to open more than one connection on the same adapter.
//! - The stream must cross an `async` task boundary (e.g. `tokio::spawn`).
//! - You store the stream in a struct alongside other owned data (no lifetime
//!   parameter on the struct).
//! - You expose the stream through an interface that requires `'static` bounds
//!   (FFI, trait objects, etc.).
//!
//! ```rust,ignore
//! use iphone_cli::tcp::adapter::Adapter;
//! use tokio::io::{AsyncReadExt, AsyncWriteExt};
//! # use std::net::{IpAddr, Ipv6Addr};
//!
//! # async fn example(transport: impl iphone_cli::tcp::ReadWrite + 'static) -> std::io::Result<()> {
//! let adapter = Adapter::new(Box::new(transport), todo!(), todo!());
//! let mut handle = adapter.to_async_handle();
//!
//! let mut stream = handle.connect(1234).await?;
//!
//! // stream is Send + 'static — spawn it freely
//! tokio::spawn(async move {
//!     stream.write_all(b"hello").await.unwrap();
//!     let mut buf = [0u8; 32];
//!     stream.read(&mut buf).await.unwrap();
//! });
//! # Ok(())
//! # }
//! ```
//!
//! # Retransmission behaviour
//!
//! Both paths share the same [`adapter::Adapter`] logic:
//!
//! - Data is sent stop-and-wait: a second segment is not put on the wire until
//!   the first is acknowledged.
//! - If no ACK arrives within the RTO (200 ms initially, doubling on each retry),
//!   the segment is retransmitted.
//! - After 5 failed attempts the connection is closed with
//!   `ErrorKind::TimedOut`.
//! - Out-of-order segments (sequence number ahead of expected) are dropped
//!   silently; duplicate segments (already acknowledged) are re-ACKed.
//!
//! # PCAP capture
//!
//! Call [`adapter::Adapter::pcap`] (or [`handle::AdapterHandle::pcap`]) with a
//! file path before connecting to write a `.pcap` file that Wireshark can open.

// Jackson Coxson

use tokio::io::{AsyncRead, AsyncWrite};

pub mod adapter;
pub mod handle;
pub mod packets;
pub mod stream;

/// `Option<PcapLog>` is the type of the optional pcap log handle threaded through
/// packet parsing.
pub type PcapLog = std::sync::Arc<tokio::sync::Mutex<tokio::fs::File>>;

/// Time primitives that work across native and wasm32-unknown-unknown.
///
/// On native this is `tokio::time`; on wasm32 we route to `wasmtimer` because
/// tokio's timer panics at runtime there (no timer backend available).
/// `Instant` lives under `wasmtimer::std` rather than `wasmtimer::tokio`, so
/// it's re-exported explicitly.
#[allow(unused_imports)]
pub(crate) mod time {
    #[cfg(not(target_arch = "wasm32"))]
    pub use tokio::time::*;
    #[cfg(target_arch = "wasm32")]
    pub use wasmtimer::std::Instant;
    #[cfg(target_arch = "wasm32")]
    pub use wasmtimer::tokio::*;
}

/// Spawn a `'static + Send` future on whatever executor is current. On native
/// this is `tokio::spawn`; on wasm32 it's `wasm_bindgen_futures::spawn_local`,
/// which doesn't require Send but accepts Send futures fine.
#[allow(dead_code)]
pub(crate) fn spawn<F>(fut: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    #[cfg(not(target_arch = "wasm32"))]
    {
        tokio::spawn(fut);
    }
    #[cfg(target_arch = "wasm32")]
    {
        wasm_bindgen_futures::spawn_local(fut);
    }
}

/// A marker trait for types that can act as the underlying transport.
///
/// Any type that implements [`tokio::io::AsyncRead`] + [`tokio::io::AsyncWrite`]
/// + [`Unpin`] + [`Send`] + [`Sync`] + [`std::fmt::Debug`] automatically
///   satisfies this bound via the blanket impl below.  Tokio's `TcpStream`,
///   `UnixStream`, and the TUN-device wrappers used in tests all qualify.
pub trait ReadWrite: AsyncRead + AsyncWrite + Unpin + Send + Sync + std::fmt::Debug {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send + Sync + std::fmt::Debug> ReadWrite for T {}

pub(crate) fn log_packet(file: &PcapLog, packet: &[u8]) {
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::io::AsyncWriteExt;
    use tracing::trace;
    trace!("Logging {} byte packet", packet.len());
    let packet = packet.to_vec();
    let file = file.to_owned();
    let now = SystemTime::now();
    tokio::task::spawn(async move {
        let mut file = file.lock().await;
        file.write_all(&(now.duration_since(UNIX_EPOCH).unwrap().as_secs() as u32).to_le_bytes())
            .await
            .unwrap();
        let micros = now.duration_since(UNIX_EPOCH).unwrap().subsec_micros();
        file.write_all(&micros.to_le_bytes())
            .await
            .unwrap();
        file.write_all(&(packet.len() as u32).to_le_bytes())
            .await
            .unwrap();
        file.write_all(&(packet.len() as u32).to_le_bytes())
            .await
            .unwrap();
        file.write_all(&packet).await.unwrap();
    });
}

/// idevice opens every service through this: RSD, the accessibility service,
/// DVT and the rest connect over the tunnel by port.
impl idevice::provider::RsdProvider for handle::AdapterHandle {
    async fn connect_to_service_port(&mut self, port: u16) -> Result<Box<dyn idevice::ReadWrite>, idevice::IdeviceError> {
        let s = self.connect(port).await?;
        Ok(Box::new(s))
    }
}
