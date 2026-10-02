//! Async forwarding loops shared by the `tokio` and `async-io` binaries.
//!
//! Both runtimes use the same loops: the raw tun-rs baseline over
//! `tun_rs::AsyncDevice`, and the tunnel-lattice facade
//! (`Handle::packet_stream` / `packet_stream_with_pool` to receive,
//! `Handle::send_async` to send). With `--offload`, the tun-rs loop uses
//! `recv_multiple`/`send_multiple` and the facade loop sends every packet
//! the stream already has ready with one `Handle::send_batch_async` call.
//! Only the executor around them differs, and that lives in each binary.

use std::sync::Arc;

use futures::{FutureExt, StreamExt};
use tun_rs::{AsyncDevice, DeviceBuilder};
use tunnel_lattice::{
    Capability, DeviceConfig, DeviceKind, Error, Handle, PacketBuf, PacketStream, TunRsDevice,
    Tunnel,
};

use crate::cli::{Args, OFFLOAD_BATCH, SkipCounter, fatal};
use crate::tunrs_offload::TunrsBatch;

/// Receive buffer of the raw tun-rs loops, as in tun-benchmark2.
const TUNRS_BUF_LEN: usize = 65536;

static SKIPPED: SkipCounter = SkipCounter::new();

/// Opens both raw tun-rs devices with their addresses (`--ip1`/`--ip2`,
/// prefix 24), with offload if `--offload` was given. With Tokio this must
/// run inside the runtime.
pub fn open_tunrs(args: &Args) -> (Arc<AsyncDevice>, Arc<AsyncDevice>) {
    let open = |name: &str, ip| {
        let device = DeviceBuilder::new()
            .name(name)
            .ipv4(ip, 24, None)
            .mtu(args.mtu)
            .offload(args.offload)
            .build_async()
            .unwrap_or_else(|error| fatal(&format!("open {name}"), error));
        if args.offload {
            if !device.tcp_gso() {
                fatal(&format!("open {name}"), "offload requested but not enabled");
            }
            eprintln!("{name}: offload on, UDP GSO {}", device.udp_gso());
        }
        Arc::new(device)
    };
    (open(&args.iface1, args.ip1), open(&args.iface2, args.ip2))
}

/// Copies packets from `src` to `dst` until an error, which ends the
/// process. With `offload`, uses the batch loop ([`tunrs_offload`]).
pub async fn tunrs(src: Arc<AsyncDevice>, dst: Arc<AsyncDevice>, direction: &str, offload: bool) {
    if offload {
        return tunrs_offload(src, dst, direction).await;
    }
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

/// The tun-rs offload loop: `recv_multiple` splits one kernel frame into a
/// batch, `send_multiple` coalesces and writes it.
async fn tunrs_offload(src: Arc<AsyncDevice>, dst: Arc<AsyncDevice>, direction: &str) {
    let mtu = src
        .mtu()
        .unwrap_or_else(|error| fatal(&format!("{direction} mtu"), error));
    let mut batch = TunrsBatch::new(mtu);
    loop {
        let count = match src
            .recv_multiple(
                &mut batch.original,
                &mut batch.bufs,
                &mut batch.sizes,
                TunrsBatch::OFFSET,
            )
            .await
        {
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => fatal(&format!("{direction} recv_multiple"), error),
        };
        batch.trim(count);
        if let Err(error) = dst
            .send_multiple(&mut batch.gro, &mut batch.bufs[..count], TunrsBatch::OFFSET)
            .await
            && error.kind() != std::io::ErrorKind::Interrupted
        {
            fatal(&format!("{direction} send_multiple"), error);
        }
        batch.restore(count);
    }
}

/// Opens both devices through the facade, without addresses (the run
/// script assigns them), and returns them with the per-packet receive
/// buffer length (`recv_buffer_len()`). With `--offload` both are opened
/// with `DeviceConfig::with_offload(true)` and must report
/// `Capability::SEGMENTATION_OFFLOAD`. With Tokio this must run inside the
/// runtime.
pub fn open_facade(args: &Args) -> (Handle<TunRsDevice>, Handle<TunRsDevice>, usize) {
    let tunnel = Tunnel::connect();
    let open = |name: &str| {
        let device = tunnel
            .open(
                DeviceConfig::new(DeviceKind::Tun)
                    .with_name(name)
                    .with_mtu(u32::from(args.mtu))
                    .with_offload(args.offload),
            )
            .unwrap_or_else(|error| fatal(&format!("open {name}"), error));
        check_offload(name, &device, args.offload);
        device
    };
    let dev1 = open(&args.iface1);
    let dev2 = open(&args.iface2);
    let buf_len = dev1
        .snapshot()
        .unwrap_or_else(|error| fatal("snapshot", error))
        .recv_buffer_len();
    (dev1, dev2, buf_len)
}

/// Ends the process if `--offload` was given but `device` did not grant
/// segmentation offload (an offload request is a hint, not a guarantee).
pub fn check_offload(name: &str, device: &Handle<TunRsDevice>, offload: bool) {
    if !offload {
        return;
    }
    if !device
        .capabilities()
        .contains(Capability::SEGMENTATION_OFFLOAD)
    {
        fatal(
            &format!("open {name}"),
            "offload requested but SEGMENTATION_OFFLOAD not reported",
        );
    }
    eprintln!("{name}: SEGMENTATION_OFFLOAD on");
}

/// Sends every packet `packets` yields to `dst`, passing the pooled
/// buffers through without a copy: one `Handle::send_async` per packet, or
/// with `offload` a batch per wake-up ([`facade_offload`]). A packet larger
/// than the receive buffer is counted and skipped; any other error, or the
/// end of the stream, ends the process.
pub async fn facade(
    mut packets: PacketStream,
    dst: Handle<TunRsDevice>,
    direction: &str,
    offload: bool,
) {
    if offload {
        return facade_offload(packets, dst, direction).await;
    }
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

/// The facade offload loop. Awaits one packet, then takes every packet the
/// stream has ready without waiting (up to [`OFFLOAD_BATCH`]; on an offload
/// handle these are mostly the remaining segments of one kernel
/// super-packet, split lazily behind `recv`), and sends them with
/// `Handle::send_batch_async`, looping on a short batch. The held buffers
/// are released before the next await on the stream, so the loop never
/// waits for a pool slot it holds itself.
async fn facade_offload(mut packets: PacketStream, dst: Handle<TunRsDevice>, direction: &str) {
    let mut held: Vec<PacketBuf> = Vec::with_capacity(OFFLOAD_BATCH);
    loop {
        let first = packets.next().await;
        take(first, &mut held, direction);
        while held.len() < OFFLOAD_BATCH {
            match packets.next().now_or_never() {
                Some(item) => take(item, &mut held, direction),
                None => break,
            }
        }
        if !held.is_empty() {
            send_all(&dst, &held, direction).await;
            held.clear();
        }
    }
}

/// Stores one stream item in `held`, skips a too-large packet, and ends the
/// process on any other error or the end of the stream.
fn take(
    item: Option<tunnel_lattice::Result<PacketBuf>>,
    held: &mut Vec<PacketBuf>,
    direction: &str,
) {
    match item {
        Some(Ok(packet)) => held.push(packet),
        Some(Err(Error::BufferTooSmall)) => {
            SKIPPED.record(direction);
        }
        Some(Err(error)) => fatal(&format!("{direction} packet_stream"), error),
        None => fatal(direction, "packet stream ended"),
    }
}

/// Sends every packet in `held`, in order, with as many
/// `Handle::send_batch_async` calls as the prefix contract needs.
async fn send_all(dst: &Handle<TunRsDevice>, held: &[PacketBuf], direction: &str) {
    let mut refs: [&[u8]; OFFLOAD_BATCH] = [&[]; OFFLOAD_BATCH];
    for (slot, packet) in refs.iter_mut().zip(held) {
        *slot = packet;
    }
    let refs = &refs[..held.len()];
    let mut sent = 0;
    while sent < refs.len() {
        match dst.send_batch_async(&refs[sent..]).await {
            Ok(0) => fatal(&format!("{direction} send_batch_async"), "sent nothing"),
            Ok(n) => sent += n,
            Err(error) => fatal(&format!("{direction} send_batch_async"), error),
        }
    }
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
