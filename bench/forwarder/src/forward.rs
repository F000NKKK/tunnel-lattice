//! Async forwarding loops shared by the `tokio` and `async-io` binaries.
//!
//! Both runtimes use the same two loops: the raw tun-rs baseline over
//! `tun_rs::AsyncDevice`, and the tunnel-lattice facade
//! (`Handle::packet_stream` / `packet_stream_with_pool` to receive,
//! `Handle::send_async` to send). Only the executor around them differs,
//! and that lives in each binary.

use std::sync::Arc;

use futures::StreamExt;
use tun_rs::{AsyncDevice, DeviceBuilder};
use tunnel_lattice::{DeviceConfig, DeviceKind, Error, Handle, PacketStream, TunRsDevice, Tunnel};

use crate::cli::{Args, SkipCounter, fatal};

/// Receive buffer of the raw tun-rs loops, as in tun-benchmark2.
const TUNRS_BUF_LEN: usize = 65536;

static SKIPPED: SkipCounter = SkipCounter::new();

/// Opens both raw tun-rs devices with their addresses (`--ip1`/`--ip2`,
/// prefix 24). With Tokio this must run inside the runtime.
pub fn open_tunrs(args: &Args) -> (Arc<AsyncDevice>, Arc<AsyncDevice>) {
    let open = |name: &str, ip| {
        let device = DeviceBuilder::new()
            .name(name)
            .ipv4(ip, 24, None)
            .mtu(args.mtu)
            .build_async()
            .unwrap_or_else(|error| fatal(&format!("open {name}"), error));
        Arc::new(device)
    };
    (open(&args.iface1, args.ip1), open(&args.iface2, args.ip2))
}

/// Copies packets from `src` to `dst` until an error, which ends the
/// process.
pub async fn tunrs(src: Arc<AsyncDevice>, dst: Arc<AsyncDevice>, direction: &str) {
    let mut buf = vec![0u8; TUNRS_BUF_LEN];
    loop {
        let len = match src.recv(&mut buf).await {
            Ok(len) => len,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => fatal(&format!("{direction} recv"), error),
        };
        if let Err(error) = dst.send(&buf[..len]).await
            && error.kind() != std::io::ErrorKind::Interrupted
        {
            fatal(&format!("{direction} send"), error);
        }
    }
}

/// Opens both devices through the facade, without addresses (the run
/// script assigns them), and returns them with the per-packet receive
/// buffer length (`recv_buffer_len()`). With Tokio this must run inside
/// the runtime.
pub fn open_facade(args: &Args) -> (Handle<TunRsDevice>, Handle<TunRsDevice>, usize) {
    let tunnel = Tunnel::connect();
    let open = |name: &str| {
        tunnel
            .open(
                DeviceConfig::new(DeviceKind::Tun)
                    .with_name(name)
                    .with_mtu(u32::from(args.mtu)),
            )
            .unwrap_or_else(|error| fatal(&format!("open {name}"), error))
    };
    let dev1 = open(&args.iface1);
    let dev2 = open(&args.iface2);
    let buf_len = dev1
        .snapshot()
        .unwrap_or_else(|error| fatal("snapshot", error))
        .recv_buffer_len();
    (dev1, dev2, buf_len)
}

/// Sends every packet `packets` yields to `dst` with `Handle::send_async`,
/// passing the pooled buffer through without a copy. A packet larger than
/// the receive buffer is counted and skipped; any other error, or the end
/// of the stream, ends the process.
pub async fn facade(mut packets: PacketStream, dst: Handle<TunRsDevice>, direction: &str) {
    while let Some(item) = packets.next().await {
        match item {
            Ok(packet) => {
                if let Err(error) = dst.send_async(&packet).await {
                    fatal(&format!("{direction} send_async"), error);
                }
            }
            Err(Error::BufferTooSmall) => {
                SKIPPED.record(direction);
            }
            Err(error) => fatal(&format!("{direction} packet_stream"), error),
        }
    }
    fatal(direction, "packet stream ended")
}

/// The Tokio runtime for `--threads`: `1` is a `current_thread` runtime,
/// `N` a multi-thread runtime with `N` workers, and unset Tokio's default
/// multi-thread runtime (one worker per CPU), as in tun-benchmark2.
#[cfg(feature = "tokio")]
pub fn tokio_runtime(threads: Option<usize>) -> tokio::runtime::Runtime {
    let mut builder = match threads {
        Some(1) => tokio::runtime::Builder::new_current_thread(),
        Some(workers) => {
            let mut builder = tokio::runtime::Builder::new_multi_thread();
            builder.worker_threads(workers);
            builder
        }
        None => tokio::runtime::Builder::new_multi_thread(),
    };
    builder
        .enable_all()
        .build()
        .unwrap_or_else(|error| fatal("build the Tokio runtime", error))
}
