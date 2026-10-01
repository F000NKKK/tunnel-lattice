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

[Overview](#-overview) • [Features](#-key-features) • [Installation](#-installation) • [Quick Start](#-quick-start) • [Feature Flags](#-feature-flags) • [Ownership](#-ownership-handle-is-clone) • [Privileges](#-platform-and-privilege-notes)

</div>

---

## 📖 Overview

The application-facing crate of
[Tunnel Lattice](https://github.com/F000NKKK/tunnel-lattice): it creates and
drives TUN (raw IP) and TAP (Ethernet) virtual network interfaces on Linux,
Windows, and macOS with one typed API, and is designed to compose with the
rest of the Lattice networking stack.

This crate does not assign IP addresses to the interfaces it creates — use
[`net-lattice`](https://crates.io/crates/net-lattice) for OS network
configuration once a device exists.

## 🌟 Key Features

- ✅ `Tunnel::connect()`, a stateless connection to the default
  `tun-rs`-backed backend (the `tun-rs` feature, enabled by default);
- ✅ `Tunnel::new(backend)`, wrapping any backend that implements the
  `tunnel-lattice-platform` traits, including your own or a test double,
  and `Tunnel::capabilities()`, what the host supports before a device is
  opened;
- ✅ `Handle::id`/`kind`, the identity and kind captured at open (no
  native call; read the current name with `snapshot()`);
- ✅ `Tunnel::open(DeviceConfig)`, creating a new TUN or TAP device and
  returning a `Handle` to it;
- ✅ `Handle::recv`/`send`, blocking packet transfer on an open device;
- ✅ `Handle::snapshot`, re-reading a device's current name/MTU/administrative
  state and, for TAP, its MAC address;
- ✅ `Handle::apply(DeviceConfigPatch)`, changing an open device's MTU,
  administrative state, or TAP MAC address (the last where the handle
  reports `Capability::MAC_MUTATION`: Linux and macOS; on Windows a TAP
  MAC address can only be requested at open with `DeviceConfig::with_mac`);
- 🐧 `Handle::persist`/`Handle::additional_queue`, on backends that implement
  `PersistentDevice`/`MultiQueueProvider` (Linux only, via
  `tunnel-lattice-backend-tunrs` — see
  [Persistent devices and multi-queue](#-persistent-devices-and-multi-queue));
- ⚡ with the `async-io` or `tokio` feature (mutually exclusive):
  `Handle::packet_stream` and `Handle::packet_stream_with_pool`, a
  `futures::Stream` of received packets, each a `PacketBuf` view into a
  `PacketPool` slot (both re-exported here), with no allocation or copy per
  packet.

## 💻 Supported Platforms

| Platform    | TUN | TAP | Sync | Tokio | async-io |
|-------------|:---:|:---:|:----:|:-----:|:--------:|
| **Linux**   | ✅  | ✅  | ✅   | ✅    | ✅       |
| **Windows** | ✅  | ⚠️  | ✅   | ✅    | ✅       |
| **macOS**   | ✅  | ✅  | ✅   | ✅    | ✅       |

✅ tested in CI on real devices. ⚠️ supported, but only the missing-driver
path is tested in CI so far. See
[`tunnel-lattice-backend-tunrs`](https://crates.io/crates/tunnel-lattice-backend-tunrs)
for the per-OS mechanisms and error mapping.

## 📦 Installation

```toml
[dependencies]
# Synchronous API, no async runtime
tunnel-lattice = "0.4"

# Async packet stream on Tokio (multi-threaded runtime)
tunnel-lattice = { version = "0.4", features = ["tokio"] }

# Async packet stream on async-io (smol, async-std, ...)
tunnel-lattice = { version = "0.4", features = ["async-io"] }
```

`tokio` and `async-io` are mutually exclusive.

## 🎓 Quick Start

### Synchronous TUN

```rust,no_run
use tunnel_lattice::{DeviceConfig, DeviceKind, Result, Tunnel};

fn main() -> Result<()> {
    let tunnel = Tunnel::connect();
    let device = tunnel.open(DeviceConfig::new(DeviceKind::Tun).with_mtu(1500))?;
    let mut buf = vec![0u8; device.snapshot()?.recv_buffer_len()];
    let len = device.recv(&mut buf)?;
    println!("{len} bytes: {:?}", &buf[..len]);
    Ok(())
}
```

`recv` never truncates a packet. One that does not fit in the buffer is
discarded and reported as `Error::BufferTooSmall`, and the next `recv`
works normally. `Device::recv_buffer_len()` (the MTU for TUN, MTU + 18 for
TAP's Ethernet header and one VLAN tag) is large enough at the device's
current MTU, except for a double-tagged (QinQ) TAP frame.

### Async Packet Stream (Tokio)

```rust,ignore
use futures::StreamExt;
use tunnel_lattice::{DeviceConfig, DeviceKind, Tunnel};

#[tokio::main]
async fn main() -> tunnel_lattice::Result<()> {
    let device = Tunnel::connect().open(DeviceConfig::new(DeviceKind::Tun))?;
    let mut packets = device.packet_stream(device.snapshot()?.recv_buffer_len())?;
    while let Some(packet) = packets.next().await {
        let packet = packet?; // a PacketBuf: derefs to [u8]
        println!("{} bytes", packet.len());
        // Dropping `packet` returns its slot to the pool.
    }
    Ok(())
}
```

## 🚩 Feature Flags

- **`tun-rs`** (default) — selects `tunnel-lattice-backend-tunrs`. A Cargo
  feature, not a `target_os` cfg gate, so a future non-`tun-rs` backend can
  be added alongside it rather than replacing it (see the project's
  `ARCHITECTURE.md`, "Backend replacement plan").
- **`async-io`** / **`tokio`** (mutually exclusive; enabling both is a
  compile error from `tun-rs`) — either adds `Handle::packet_stream` and
  `Handle::packet_stream_with_pool`. No async runtime dependency is imposed
  when neither feature is enabled.
  - `async-io` selects `tun-rs`'s `async-io`/`blocking`-based backend (no
    tokio dependency); `tokio` selects its tokio-based one.
  - The stream uses a backend's native async path (no worker thread;
    dropping the stream genuinely cancels the in-flight `recv`) when it
    reports `Capability::NATIVE_ASYNC` — `tunnel-lattice-backend-tunrs`
    does with either feature, which is the case whenever this method is
    reachable at all — otherwise it falls back to `tunnel-lattice-async`'s
    thread-based adapter, whose shutdown is best-effort only.

### Packet stream behaviour

- `packet_stream`'s `buf_len` argument is the per-packet buffer size; pass
  `handle.snapshot()?.recv_buffer_len()`. It returns
  `Err(Error::InvalidState)` only for a rejected `buf_len` (zero, or too
  large for a pool slot). `packet_stream_with_pool` takes a `PacketPool`
  you built instead and cannot fail.
- Each item is exactly one packet (one IP packet for TUN, one Ethernet
  frame for TAP) received straight into a pool slot, with no allocation or
  copy per packet. Dropping the `PacketBuf` returns the slot, and a
  consumer holding every slot pauses the stream until it drops one. Call
  `to_vec()` for an owned copy.
- An oversize packet yields `Err(Error::BufferTooSmall)` and the stream
  keeps receiving. Every other error (`Err(Error::Disconnected)` for a
  deleted device, or a recoverable one such as `Err(Error::InvalidState)`
  for a disabled interface) is yielded once and ends the stream, so call
  `packet_stream` again for a new stream after recovering.

### `tokio` requires a multi-threaded runtime

**With `tokio`, `Handle::recv`/`send`/`snapshot`/`apply` all require a
multi-threaded Tokio runtime entered on the calling thread**
(`#[tokio::main]`'s default flavor, or `Builder::new_multi_thread()`) —
not only `packet_stream`. `tunnel-lattice-backend-tunrs`'s `PacketIo`
drives tun-rs's Tokio-backed handle through
`tokio::runtime::Handle::current().block_on`, which only polls that
runtime's I/O driver on the `multi_thread` flavor; on `current_thread` the
first `recv`/`send` call hangs forever. See that crate's README, "`tokio`
requires a multi-threaded runtime," for why. Prefer `async-io` if a
single-threaded runtime is a hard requirement.

## 🤝 Ownership: `Handle` is `Clone`

`Handle<D>` wraps its device in an `Arc` and can be cloned cheaply to share
one open device across threads — `recv`/`send` take `&self`, so concurrent
calls through separate clones are always safe. The device stays open until
every `Handle` clone (and any `PacketStream` derived from one) has been
dropped; there is no explicit close method. See the project's
`ARCHITECTURE.md`, "Ownership and concurrency contract", for the full
write-up, including how this differs from `additional_queue`'s independent
hardware queue.

## 🔀 Persistent Devices and Multi-Queue

Both Linux-only, reported by `Capability::PERSISTENT_DEVICES`/
`Capability::MULTI_QUEUE`. The tun-rs backend implements
`PersistentDevice`/`MultiQueueProvider` only on Linux, so on Windows and
macOS these methods do not exist and the example below does not compile:

```rust,no_run
use tunnel_lattice::{DeviceConfig, DeviceKind, Tunnel};

fn main() -> tunnel_lattice::Result<()> {
    let tunnel = Tunnel::connect();
    let device = tunnel.open(DeviceConfig::new(DeviceKind::Tun).with_multi_queue(true))?;
    device.persist()?;                             // survives this process exiting
    let second_queue = device.additional_queue()?; // independent hardware queue
    Ok(())
}
```

`additional_queue` on a device not opened with `with_multi_queue(true)`
returns `Error::Unsupported`. Multi-queue is a throughput optimization,
not a requirement for sharing a device across threads — see [Ownership](#-ownership-handle-is-clone) above.

## 🔐 Platform and Privilege Notes

Creating a TUN/TAP device generally requires `CAP_NET_ADMIN` on Linux,
Administrator on Windows, or root on macOS. Runtime `Capability` flags
describe implemented surfaces, not a guarantee the current process is
authorized.

`Tunnel::connect().capabilities()` needs no privilege and opens nothing.
It reports `Capability::TAP_DEVICES` on Linux and macOS, and on Windows
only when the tap-windows6 driver is installed (detected once per process
without creating an adapter). `open` stays authoritative either way.

On Windows, `TunRsBackend::open` for a TUN device also requires
`wintun.dll` to be present next to your application's executable or on
`PATH` — `tun-rs` loads it at runtime rather than linking it at build time,
and does not vendor it. Without it, `open` fails with
`Error::DriverUnavailable` (as it does for a TAP device when the
tap-windows driver is not installed). Download it from
[wintun.net](https://www.wintun.net/) and ship it with your application;
see `tunnel-lattice-backend-tunrs`'s README for details.

## 📖 Documentation

- **API reference**: [docs.rs/tunnel-lattice](https://docs.rs/tunnel-lattice)
- **Async adapter**: [`tunnel-lattice-async`](https://crates.io/crates/tunnel-lattice-async)
- **Default backend**: [`tunnel-lattice-backend-tunrs`](https://crates.io/crates/tunnel-lattice-backend-tunrs)
- **Project**: [github.com/F000NKKK/tunnel-lattice](https://github.com/F000NKKK/tunnel-lattice)

## 📄 License

Licensed under the [Mozilla Public License 2.0](https://github.com/F000NKKK/tunnel-lattice/blob/main/LICENSE).
