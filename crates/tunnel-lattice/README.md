<div align="center">

# 🕸️ tunnel-lattice

### Typed, Cross-Platform TUN/TAP Interfaces for Rust

[![crates.io](https://img.shields.io/crates/v/tunnel-lattice.svg?cacheSeconds=86400)](https://crates.io/crates/tunnel-lattice)
[![docs.rs](https://img.shields.io/docsrs/tunnel-lattice?cacheSeconds=86400)](https://docs.rs/tunnel-lattice)
[![Downloads](https://img.shields.io/crates/d/tunnel-lattice.svg?cacheSeconds=86400)](https://crates.io/crates/tunnel-lattice)
[![License: MPL 2.0](https://img.shields.io/badge/license-MPL--2.0-blue.svg)](https://github.com/F000NKKK/tunnel-lattice/blob/main/LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.93-lightgrey.svg)](https://github.com/F000NKKK/tunnel-lattice)

![Linux](https://img.shields.io/badge/Linux-supported-success)
![Windows](https://img.shields.io/badge/Windows-supported-success)
![macOS](https://img.shields.io/badge/macOS-supported-success)

[Overview](#-overview) • [Platforms](#-supported-platforms) • [Installation](#-installation)
• [Quick Start](#-quick-start) • [Batch Send](#-batch-send-and-segmentation-offload)
• [Privileges](#-privileges-and-safety) • [Performance](#-performance)

</div>

---

## 📖 Overview

The application-facing crate of
[Tunnel Lattice](https://github.com/F000NKKK/tunnel-lattice): it creates and
drives TUN (raw IP) and TAP (Ethernet) virtual network interfaces on Linux,
Windows, and macOS with one typed API and one typed `Error`. It does not
assign IP addresses; use [`net-lattice`](https://crates.io/crates/net-lattice)
for that once a device exists.

Main surface:

- `Tunnel::connect()` (default `tun-rs` backend) or `Tunnel::new(backend)`
  for any backend implementing the `tunnel-lattice-platform` traits;
  `Tunnel::capabilities()` answers for the host before anything is opened.
- `Tunnel::open(DeviceConfig)` returns a `Handle`: `recv`/`send`,
  `recv_batch`/`send_batch`, `snapshot` (name, MTU, admin state, TAP MAC), and
  `apply(DeviceConfigPatch)` (MTU, admin state, TAP MAC where
  `Capability::MAC_MUTATION` is reported: Linux and macOS).
- Linux only: `Handle::persist`/`unpersist`/`additional_queue`, and opt-in
  TUN segmentation offload via `DeviceConfig::with_offload(true)`.
- With `tokio` or `async-io`: `packet_stream`/`packet_stream_with_pool` (a
  `futures::Stream` of `PacketBuf` views into a `PacketPool`, no allocation
  or copy per packet), `send_async`, `send_batch_async`, and
  `recv_batch_async`.

`Handle` is `Clone`: clones share one device, `recv`/`send` take `&self`,
and the device closes when the last clone (and any stream) is dropped.

## 💻 Supported Platforms

All three are tested in CI on real devices (TUN and TAP, sync, Tokio, and
async-io):

- **Linux** — `CAP_NET_ADMIN` (root or `setcap`); the `tun` kernel module
  (`/dev/net/tun`) serves TUN and TAP.
- **Windows** — Administrator. TUN needs Wintun (`wintun.dll` next to the
  executable or on `PATH`); TAP needs the tap-windows6 (`tap0901`) driver
  staged in the driver store.
- **macOS** — root. TUN is `utun`, TAP is a `feth` pair; both built in.

`tun-rs` runs on more platforms, but this crate claims only the ones CI tests.

## 📦 Installation

```toml
[dependencies]
tunnel-lattice = "0.6"                                          # sync only
tunnel-lattice = { version = "0.6", features = ["tokio"] }     # or Tokio
tunnel-lattice = { version = "0.6", features = ["async-io"] }  # or async-io
```

- **`tun-rs`** (default) — selects `tunnel-lattice-backend-tunrs`.
- **`tokio`** / **`async-io`** — **mutually exclusive** (enabling both is a
  compile error from `tun-rs`). Either adds the stream and async sends,
  using the backend's native async path (`Capability::NATIVE_ASYNC`).
  Neither imposes an async runtime when off.

## 🎓 Quick Start

```rust,no_run
use tunnel_lattice::{DeviceConfig, DeviceKind, Result, Tunnel};

fn main() -> Result<()> {
    let device = Tunnel::connect().open(DeviceConfig::new(DeviceKind::Tun).with_mtu(1500))?;
    let mut buf = vec![0u8; device.snapshot()?.recv_buffer_len()];
    let len = device.recv(&mut buf)?;
    println!("{len} bytes: {:?}", &buf[..len]);
    Ok(())
}
```

`recv` never truncates: an oversize packet is discarded as
`Error::BufferTooSmall` and the next `recv` works. `recv_buffer_len()` (MTU
for TUN, MTU + 18 for TAP) fits any packet except a double-tagged TAP frame.

`recv_batch(bufs, lens)` (and `recv_batch_async`) receives up to
`min(bufs.len(), lens.len())` packets into caller-provided buffers, one per
buffer, and returns how many: packet `i` is `bufs[i][..lens[i]]`, and only
those bytes are meaningful. It waits for the first packet only and never
truncates; how many packets one call returns is up to the backend, and the
default takes exactly one. `Err` means nothing was received; a failure
after some packets is reported as that count and appears on the next call.

Forwarding inside async code (Tokio feature); a `PacketBuf` derefs to `[u8]`:

```rust,ignore
use futures::StreamExt;
use tunnel_lattice::{DeviceConfig, DeviceKind, Tunnel};

#[tokio::main]
async fn main() -> tunnel_lattice::Result<()> {
    let tunnel = Tunnel::connect();
    let inside = tunnel.open(DeviceConfig::new(DeviceKind::Tun))?;
    let outside = tunnel.open(DeviceConfig::new(DeviceKind::Tun))?;
    let mut packets = inside.packet_stream(inside.snapshot()?.recv_buffer_len())?;
    while let Some(packet) = packets.next().await {
        outside.send_async(&packet?).await?; // dropping the PacketBuf frees its slot
    }
    Ok(())
}
```

A stream yields `BufferTooSmall` and keeps going; any other error is yielded
once and ends it, so call `packet_stream` again after recovering.

## 📨 Batch Send and Segmentation Offload

```rust,no_run
use tunnel_lattice::{Capability, DeviceConfig, DeviceKind, Tunnel};

fn main() -> tunnel_lattice::Result<()> {
    let config = DeviceConfig::new(DeviceKind::Tun).with_offload(true); // a request
    let device = Tunnel::connect().open(config)?;
    let offload = device.capabilities().contains(Capability::SEGMENTATION_OFFLOAD);
    println!("segmentation offload in use: {offload}");

    let packets: Vec<&[u8]> = Vec::new(); // whole IP packets
    let mut rest = &packets[..];
    while !rest.is_empty() {
        let sent = device.send_batch(rest)?; // may be a short prefix
        rest = &rest[sent..];
    }
    Ok(())
}
```

- `send_batch` / `send_batch_async` send a prefix in order: `Ok(n)` means
  `packets[..n]` went out whole; `Err` means nothing was sent. A dropped
  async batch sent an unknown prefix, each packet whole and at most once.
- Offload is **Linux TUN only**, off by default, and a request: it is
  ignored without error on Windows, macOS, and TAP. Check
  `Capability::SEGMENTATION_OFFLOAD` on the open handle (never on
  `Tunnel::capabilities()`).
- `recv` still returns one IP packet per call; the backend splits kernel
  super-packets itself (about 64 KiB of buffer per offload queue). On an
  offload queue `send_batch` coalesces adjacent packets of one flow into one
  write, at most 128 packets per call.
- The kernel's offload setting is **device-wide**: a later open of the same
  device without offload switches it off for every queue, and nothing
  restores it when a handle drops.

## 🔐 Privileges and Safety

- Opening a device needs `CAP_NET_ADMIN` (Linux), Administrator (Windows),
  or root (macOS). Capabilities describe surfaces, not authorization;
  `Tunnel::capabilities()` itself needs no privilege and opens nothing.
- On Windows, a missing `wintun.dll` (TUN) or tap-windows6 driver (TAP)
  makes `open` fail with `Error::DriverUnavailable`. Ship `wintun.dll` from
  [wintun.net](https://www.wintun.net/) with your application.
- **With `tokio`**, `open` needs an entered Tokio runtime, and the blocking
  `recv`/`send` need a **multi-threaded** one: on `current_thread` they hang
  forever. Inside async code never call blocking `recv`/`send` (Tokio panics;
  async-io parks the executor); use `packet_stream`, `send_async`, and
  `send_batch_async`, which also work on `current_thread`.
- `persist`, `unpersist`, and `additional_queue` exist only on Linux; code
  calling them does not compile elsewhere. A persistent device outlives the
  process until `unpersist` and its last handle closes.

## 🚀 Performance

Forwarding throughput between two TUN devices with `iperf3`, raw `tun-rs`
measured in the same run on a GitHub Actions Linux runner:

- **Without offload**: 91–96 % of `tun-rs` (sync, Tokio, and async-io).
- **With offload**: Tokio **111 %**, async-io **124 %** of `tun-rs`'s own
  `recv_multiple`/`send_multiple`; sync **29 %** (one packet per `recv`,
  sends not coalesced).

Full table, method, and reproduction steps:
[repository README, "Forwarding throughput"](https://github.com/F000NKKK/tunnel-lattice/blob/main/README.md#forwarding-throughput-iperf3-linux).

## 📖 Documentation

- **API reference**: [docs.rs/tunnel-lattice](https://docs.rs/tunnel-lattice)
- **Comparison with `tun-rs`**:
  [repository README](https://github.com/F000NKKK/tunnel-lattice/blob/main/README.md#-comparison)
- **Design, errors, offload, ownership**:
  [ARCHITECTURE.md](https://github.com/F000NKKK/tunnel-lattice/blob/main/ARCHITECTURE.md)
- **Default backend**: [`tunnel-lattice-backend-tunrs`](https://crates.io/crates/tunnel-lattice-backend-tunrs)
- **Async adapter**: [`tunnel-lattice-async`](https://crates.io/crates/tunnel-lattice-async)

## 📄 License

Licensed under the
[Mozilla Public License 2.0](https://github.com/F000NKKK/tunnel-lattice/blob/main/LICENSE).
