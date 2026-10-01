<div align="center">

<a id="top"></a>

# 🕸️ Tunnel Lattice

### Typed, Cross-Platform TUN/TAP Interfaces for Rust

[![crates.io](https://img.shields.io/crates/v/tunnel-lattice.svg?cacheSeconds=86400)](https://crates.io/crates/tunnel-lattice)
[![docs.rs](https://img.shields.io/docsrs/tunnel-lattice?cacheSeconds=86400)](https://docs.rs/tunnel-lattice)
[![Downloads](https://img.shields.io/crates/d/tunnel-lattice.svg?cacheSeconds=86400)](https://crates.io/crates/tunnel-lattice)
[![CI](https://github.com/F000NKKK/tunnel-lattice/actions/workflows/ci.yml/badge.svg)](https://github.com/F000NKKK/tunnel-lattice/actions/workflows/ci.yml)
[![License: MPL 2.0](https://img.shields.io/badge/license-MPL--2.0-blue.svg)](LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.93-lightgrey.svg)](Cargo.toml)

![Linux](https://img.shields.io/badge/Linux-supported-success)
![Windows](https://img.shields.io/badge/Windows-supported-success)
![macOS](https://img.shields.io/badge/macOS-supported-success)

🇺🇸 **English** | 🇷🇺 [Русский](README.ru.md)

[Features](#-key-features) • [Platforms](#-supported-platforms) • [Performance](#-performance) • [Installation](#-installation) • [Quick Start](#-quick-start) • [Comparison](#-comparison)

</div>

---

## 📖 Overview

**Tunnel Lattice** creates and drives TUN (raw IP) and TAP (Ethernet)
virtual network interfaces on Linux, Windows, and macOS, with one typed API
on every platform. It is the tunnel layer of the Lattice networking stack
and composes with [net-lattice](https://github.com/F000NKKK/net-lattice) for
addresses and routes.

### 🎯 Why Tunnel Lattice?

- **🧭 Typed errors everywhere**: every OS failure becomes one
  `tunnel_lattice::Error`. `Disconnected`, `BufferTooSmall`,
  `DriverUnavailable`, and `PermissionDenied` mean the same thing on Linux,
  Windows, and macOS, so you never match on raw errno or Win32 codes.
- **🛑 A deleted device always ends `recv`**: removing the interface ends a
  pending `recv` or `PacketStream` with `Disconnected` on all three OSes and
  every feature set, including the async macOS TAP and Linux Tokio cases
  where the underlying library waits forever.
- **📦 No silent truncation**: a packet that does not fit your buffer is
  reported as `BufferTooSmall` and dropped. You never get half a packet.
- **⚡ Runtime-agnostic async**: a `futures::Stream` of packets on Tokio or
  `async-io`, with no async runtime pulled in unless you ask for one.
- **♻️ Zero allocations per packet**: the stream receives straight into a
  reusable `PacketPool` slot. A test in the repository pins this.
- **🔌 Swappable backends**: provider traits plus runtime `Capability` flags
  separate the API from the implementation. Today it runs on
  [`tun-rs`](https://github.com/tun-rs/tun-rs); native per-OS backends can
  slot in without changing your code.

> **Status:** `0.4.0` is published and development is active. Nothing is
> API-frozen before `1.0`; see [ARCHITECTURE.md](ARCHITECTURE.md).

## 🌟 Key Features

### Core Capabilities
- ✅ **TUN and TAP**: raw IP (Layer 3) and Ethernet (Layer 2) devices
- ✅ **Sync and async**: blocking `recv`/`send`, or a `futures::Stream` with
  the `tokio` or `async-io` feature
- ✅ **Observe and mutate**: re-read name, MTU, and administrative state;
  change MTU and up/down on an open device
- ✅ **Cheap sharing**: `Handle` is `Clone` and `recv`/`send` take `&self`,
  so one device can be used from many threads

### Platform-Specific Features
- 🐧 **Linux persistence**: keep a device after the process exits
- 🔀 **Linux multi-queue**: independent kernel-scheduled queues on one device
- 🍎 **macOS TAP**: `feth` pairs, with a bounded wait so a destroyed
  interface ends `recv`
- 🪟 **Windows TUN**: Wintun, with a missing `wintun.dll` reported as
  `DriverUnavailable`

### Developer Experience
- 🎯 **Honest names**: `open` refuses a name it cannot honor exactly, and
  never adopts and then destroys an interface it did not create
- 🧪 **Tested on real devices**: privileged CI creates and deletes real
  devices on Linux, Windows, and macOS in every feature set
- 🧩 **Capability flags**: ask the device what it supports at runtime
  instead of guessing per OS

## 💻 Supported Platforms

| Platform    | TUN | TAP | Sync | Tokio | async-io | Notes |
|-------------|:---:|:---:|:----:|:-----:|:--------:|-------|
| **Linux**   | ✅  | ✅  | ✅   | ✅    | ✅       | Persistent devices and multi-queue |
| **Windows** | ✅  | ⚠️  | ✅   | ✅    | ✅       | TUN needs `wintun.dll`; TAP needs the tap-windows6 driver |
| **macOS**   | ✅  | ✅  | ✅   | ✅    | ✅       | TUN via `utun`, TAP via `feth` pairs |

✅ tested in CI on real devices. ⚠️ supported, but only the missing-driver
path is tested in CI so far.

> Creating a device needs `CAP_NET_ADMIN` on Linux, Administrator on
> Windows, or root on macOS.

## 🚀 Performance

### 🏆 Design Highlights

- **Zero allocations and zero copies per packet** on the stream: the device
  writes straight into a pooled slot, and `tests/alloc_count.rs` fails if a
  steady stream allocates.
- **No worker thread on the async path**: with `tokio` or `async-io` the
  stream uses the backend's native async I/O, and dropping it cancels the
  pending `recv`.
- **Back-pressure instead of buffering**: when every slot is held, the
  stream pauses until you release one; memory use is bounded by the pool.
- **Multi-queue on Linux** for spreading traffic across cores.

### 📊 Benchmarks

Micro-benchmarks run on in-memory mock devices, with no privileges needed:

```bash
cargo bench -p tunnel-lattice-async                # stream paths vs a plain recv loop
cargo bench -p tunnel-lattice-async --bench pool   # PacketPool overhead and contention
```

An end-to-end `iperf3` throughput harness in the style of
[tun-benchmark2](https://github.com/tun-rs/tun-benchmark2) is in progress.
It measures Tunnel Lattice and raw `tun-rs` side by side on the same
machine. Its results table will appear here.

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
    println!("{len} bytes");
    Ok(())
}
```

`recv_buffer_len()` is the MTU for TUN and MTU + 18 for TAP (the Ethernet
header and one VLAN tag), which fits one packet at the current MTU except
a double-tagged (QinQ) TAP frame. A packet that does not fit is never
truncated: it is discarded and reported as `Error::BufferTooSmall`.

### Async Packet Stream (Tokio)

```rust,ignore
use futures::StreamExt;
use tunnel_lattice::{DeviceConfig, DeviceKind, Tunnel};

#[tokio::main]
async fn main() -> tunnel_lattice::Result<()> {
    let device = Tunnel::connect().open(DeviceConfig::new(DeviceKind::Tun))?;
    let mut packets = device.packet_stream(device.snapshot()?.recv_buffer_len())?;
    while let Some(packet) = packets.next().await {
        let packet = packet?; // a pooled view; dropping it returns the slot
        println!("{} bytes", packet.len());
    }
    Ok(())
}
```

The stream yields one error and ends when the device goes away; an
oversize packet yields `BufferTooSmall` and the stream keeps going.

## 📚 Examples

### Persistent Device and Multi-Queue (Linux)

```rust,no_run
use tunnel_lattice::{DeviceConfig, DeviceKind, Tunnel};

fn main() -> tunnel_lattice::Result<()> {
    let device = Tunnel::connect()
        .open(DeviceConfig::new(DeviceKind::Tun).with_name("tl0").with_multi_queue(true))?;
    device.persist()?;                             // survives this process exiting
    let second_queue = device.additional_queue()?; // for another thread
    Ok(())
}
```

### Assigning an Address with net-lattice

Tunnel Lattice creates the interface; `net-lattice` configures it. The two
crates share no object IDs on purpose: a `tunnel_lattice::DeviceId` and a
`net_lattice::InterfaceId` are distinct phantom-typed wrappers even when
their native index happens to coincide, so the compiler never lets you
pass one where the other is expected. Bridge them by the OS-assigned
interface name, the one field both sides expose in the same shape:

```rust,no_run
use net_lattice::Lattice;
use tunnel_lattice::{DeviceConfig, DeviceKind, Tunnel};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let device = Tunnel::connect().open(DeviceConfig::new(DeviceKind::Tun))?;
    let snapshot = device.snapshot()?; // `snapshot.name`, e.g. "tun0"

    let interface = Lattice::connect()?
        .interfaces()?
        .into_iter()
        .find(|i| i.name == snapshot.name)
        .ok_or(net_lattice::Error::NotFound)?;
    // assign an address, bring it up, add routes through net-lattice from here.
    Ok(())
}
```

With no `name` on `DeviceConfig`, the backend or OS picks one. A requested
name the platform cannot honor as given is rejected by `open` with
`Error::InvalidState`, and a name already in use with `Error::AlreadyExists`;
see `DeviceConfig::name` for the accepted formats and the cases where `open`
attaches to an existing device instead (a Linux persistent or multi-queue
device, a Windows Wintun adapter). Always read the actual name back from
`snapshot()`/the returned `Device`, never from the `DeviceConfig` you
passed in.

## 🔧 Platform-Specific Setup

### Linux

```bash
sudo modprobe tun                          # if /dev/net/tun is missing
sudo setcap cap_net_admin+ep ./your-app    # or run with sudo
```

A missing `tun` module is reported as `Error::DriverUnavailable`.

### Windows

- **TUN**: download `wintun.dll` for your architecture from
  [wintun.net](https://www.wintun.net/) and place it next to your executable
  or on `PATH`.
- **TAP**: install the
  [tap-windows6](https://build.openvpn.net/downloads/releases/) driver.
- Run as Administrator. Without the DLL or driver, `open` returns
  `Error::DriverUnavailable`.

### macOS

- Run as root.
- **TUN** devices are `utun<N>`. **TAP** devices are `feth<N>` pairs
  (`N` ≤ 32767); leave the name unset to let the OS pick.

## 🤝 Comparison

Tunnel Lattice currently runs on `tun-rs`, so this compares what the
Tunnel Lattice layer adds and what it does not have yet.

| Feature | Tunnel Lattice | tun-rs (the backend it wraps) |
|---------|----------------|-------------------------------|
| **Error type** | ✅ One typed `Error`, same meaning on every OS | ⚠️ `std::io::Error` with platform codes |
| **Device deleted while `recv` waits** | ✅ Ends with `Disconnected` on every OS and feature set | ⚠️ Waits forever with Tokio on Linux and async TAP on macOS |
| **Oversize packet** | ✅ `BufferTooSmall`, never truncated | ⚠️ Behaviour differs per platform |
| **Async API** | ✅ `futures::Stream`, Tokio or async-io | ✅ async `recv`/`send`, Tokio or async-io |
| **Allocation-free packet stream** | ✅ Built-in `PacketPool` | ➖ Caller-managed buffers |
| **Runtime capability flags** | ✅ | ❌ |
| **Swappable backend** | ✅ Provider traits | ❌ |
| **Hardware offload (TSO/GSO)** | 🚧 Planned for 0.6 | ✅ Linux |
| **Address and route setup** | ➖ Delegated to net-lattice | ✅ Built in |
| **Platforms** | Linux, Windows, macOS | 11+, including BSD, iOS, Android |

## 🛠️ API Overview

| Item | Purpose |
|------|---------|
| `Tunnel::connect()` | Connect to the default backend |
| `Tunnel::new(backend)` | Use any backend, including your own or a test double |
| `Tunnel::capabilities` | What the host supports before opening a device |
| `Tunnel::open(DeviceConfig)` | Create a TUN/TAP device and return a `Handle` |
| `Handle::id` / `kind` | The identity and kind captured at open (no native call) |
| `Handle::recv` / `send` | Blocking packet transfer |
| `Handle::snapshot` | Current name, MTU, and administrative state |
| `Handle::apply(DeviceConfigPatch)` | Change MTU or up/down |
| `Handle::capabilities` | What this device supports at runtime |
| `Handle::persist` / `additional_queue` | Linux persistence and multi-queue |
| `Handle::packet_stream` | Async `Stream` of pooled packets (`tokio` / `async-io`) |

### Workspace Crates

| Crate | Purpose |
| --- | --- |
| [`tunnel-lattice`](crates/tunnel-lattice/README.md) | Public facade: `Tunnel`/`Handle`, feature-selected backend |
| [`tunnel-lattice-model`](crates/tunnel-lattice-model/README.md) | Observed and desired device types (`Device`, `DeviceConfig`, `DeviceConfigPatch`) |
| [`tunnel-lattice-platform`](crates/tunnel-lattice-platform/README.md) | Provider traits and the `Capability` contract |
| [`tunnel-lattice-core`](crates/tunnel-lattice-core/README.md) | Shared errors, results, and IDs |
| [`tunnel-lattice-async`](crates/tunnel-lattice-async/README.md) | Runtime-independent packet `Stream` and `PacketPool` |
| [`tunnel-lattice-backend-tunrs`](crates/tunnel-lattice-backend-tunrs/README.md) | `tun-rs`-backed TUN/TAP implementation |

## 📖 Documentation

- **API reference**: [docs.rs/tunnel-lattice](https://docs.rs/tunnel-lattice)
- **Architecture**: [ARCHITECTURE.md](ARCHITECTURE.md)
- **Changelog**: [CHANGELOG.md](CHANGELOG.md)
- **Support and security**: [SUPPORT.md](SUPPORT.md), [SECURITY.md](SECURITY.md)

## 🐛 Troubleshooting

<details>
<summary><b><code>PermissionDenied</code> when opening a device</b></summary>

Creating a device needs `CAP_NET_ADMIN` on Linux (`sudo`, or
`sudo setcap cap_net_admin+ep ./your-app`), Administrator on Windows, or
root on macOS.
</details>

<details>
<summary><b><code>DriverUnavailable</code> on Windows or Linux</b></summary>

On Windows, `wintun.dll` (TUN) or the tap-windows6 driver (TAP) is
missing; see [Windows setup](#windows). On Linux, the `tun` module is not
loaded or `/dev/net/tun` is missing; run `sudo modprobe tun`.
</details>

<details>
<summary><b><code>recv</code> hangs with the <code>tokio</code> feature</b></summary>

With `tokio`, every device call needs a **multi-threaded** Tokio runtime
entered on the calling thread (`#[tokio::main]`'s default flavor). A
`current_thread` runtime never drives the device's I/O. Use `async-io` if
you need a single-threaded runtime.
</details>

<details>
<summary><b><code>InvalidState</code> for a device name</b></summary>

`open` rejects a name it cannot honor exactly: over 15 bytes or containing
`%` on Linux, anything but `utun<N>` / `feth<N>` on macOS. Leave the name
unset to let the OS pick one.
</details>

## 🌐 The Lattice Ecosystem

| Crate | Purpose |
| --- | --- |
| [net-lattice](https://github.com/F000NKKK/net-lattice) | OS networking inspection and configuration (routes, DNS, interfaces) |
| [tunnel-lattice](https://github.com/F000NKKK/tunnel-lattice) | TUN/TAP tunnel interfaces |
| [dns-lattice](https://github.com/F000NKKK/dns-lattice) | Programmable DNS control plane |
| [flow-lattice](https://github.com/F000NKKK/flow-lattice) | Policy compiler: rules to platform-neutral network plans |
| [sdk-lattice](https://github.com/F000NKKK/sdk-lattice) | Application-facing SDK composing the crates above |

## 🙏 Contributing

Contributions are welcome; see [CONTRIBUTING.md](CONTRIBUTING.md). Feedback
on the architecture and API shape in [ARCHITECTURE.md](ARCHITECTURE.md) is
the most valuable contribution at this stage.

```bash
git clone https://github.com/F000NKKK/tunnel-lattice.git
cd tunnel-lattice
cargo test --workspace                                      # unprivileged tests
sudo -E cargo test -p tunnel-lattice-backend-tunrs -- --ignored   # real devices
```

## 📄 License

Licensed under the [Mozilla Public License 2.0](LICENSE).

## 🌟 Acknowledgments

- [`tun-rs`](https://github.com/tun-rs/tun-rs), the cross-platform TUN/TAP
  library Tunnel Lattice currently runs on
- [Wintun](https://www.wintun.net/) and the Rust async ecosystem (Tokio,
  async-io)

---

<div align="center">

**[⬆ Back to Top](#top)**

Part of the Lattice networking stack

</div>
