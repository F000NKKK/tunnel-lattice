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

> **Status:** `0.6.0` is published and development is active. Nothing is
> API-frozen before `1.0`; see [ARCHITECTURE.md](ARCHITECTURE.md).

## 🌟 Key Features

### Core Capabilities
- ✅ **TUN and TAP**: raw IP (Layer 3) and Ethernet (Layer 2) devices
- ✅ **Sync and async**: blocking `recv`/`send`, or a `futures::Stream`
  and a non-blocking `send_async` with the `tokio` or `async-io` feature
- ✅ **Observe and mutate**: re-read name, MTU, administrative state, and
  a TAP device's MAC address; change MTU, MAC, and up/down on an open
  device
- ✅ **Cheap sharing**: `Handle` is `Clone` and `recv`/`send` take `&self`,
  so one device can be used from many threads
- ✅ **Batch send**: `send_batch` (and `send_batch_async`) sends a prefix
  of a packet list in one call on every OS; `Ok(n)` says how many went out

### Platform-Specific Features
- 🐧 **Linux persistence**: keep a device after the process exits,
  re-attach to it by name later, and un-persist it
- 🔀 **Linux multi-queue**: independent kernel-scheduled queues on one device
- 📦 **Linux TUN segmentation offload**: opt in with `with_offload(true)`;
  the kernel moves TCP/UDP traffic in super-packets of up to 64 KiB, while
  `recv` still returns one packet per call and `send_batch` coalesces
  same-flow packets
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

| Platform    | TUN | TAP | Sync | Tokio | async-io | Privilege | Driver |
|-------------|:---:|:---:|:----:|:-----:|:--------:|-----------|--------|
| **Linux**   | ✅  | ✅  | ✅   | ✅    | ✅       | `CAP_NET_ADMIN` (root or `setcap`) | `tun` kernel module (`/dev/net/tun`) for TUN and TAP |
| **Windows** | ✅  | ✅  | ✅   | ✅    | ✅       | Administrator | TUN: Wintun, `wintun.dll` next to the executable or on `PATH`; TAP: the tap-windows6 (`tap0901`) driver staged in the driver store |
| **macOS**   | ✅  | ✅  | ✅   | ✅    | ✅       | root | None: TUN is `utun`, TAP is a `feth` pair, both built in |

✅ tested in CI on real devices. The privileged jobs on `ubuntu-latest`,
`windows-latest`, and `macos-latest` open, use, and delete TUN and TAP
devices in every feature set (default, `tokio`, `async-io`). On Windows
the job first downloads Wintun 0.14.1 and stages the tap-windows6 9.27.0
driver package, without creating an adapter. A separate Linux job checks
that a persistent device survives its process and that a second process
re-attaches to it and un-persists it.

Linux also has persistent devices, multi-queue, and TUN segmentation
offload. Only platforms CI tests
are listed: `tun-rs` runs on more (BSD, iOS, Android, ...), but Tunnel
Lattice does not claim them.

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

#### Forwarding throughput (iperf3, Linux)

The [`bench/forwarder`](bench/forwarder/README.md) harness uses the method
of tun-rs's [tun-benchmark2](https://github.com/tun-rs/tun-benchmark2): a
forwarder copies packets between two TUN devices while `iperf3` measures
TCP throughput across them. Every run measures raw `tun-rs` (sync, Tokio,
and async-io) next to Tunnel Lattice on the same machine, and each Tunnel
Lattice row is compared with the `tun-rs` row above it. The first seven
rows use no offload. The six offload rows open both devices with GSO/GRO
offload (Linux TUN only) and compare Tunnel Lattice with `tun-rs`'s own
offload path, `recv_multiple`/`send_multiple`. The table and its notes are
copied unchanged from the recorded
[workflow run](https://github.com/F000NKKK/tunnel-lattice/actions/runs/36986771210):

| Configuration | Throughput (median Gbps) | vs tun-rs, same run | CPU avg | RSS max | Retransmissions |
|---|---:|---:|---:|---:|---:|
| tun-rs sync | 2.72 | — | 181 % | 2.6 MB | 83 |
| tunnel-lattice sync (Handle::recv/send) | 2.51 | 90.9 % (89.8–92.6) | 181 % | 2.4 MB | 66 |
| tun-rs async (Tokio) | 1.85 | — | 178 % | 3.6 MB | 55 |
| tunnel-lattice async (Tokio, packet_stream + send_async) | 1.70 | 92.2 % (89.5–94.0) | 181 % | 3.5 MB | 48 |
| tunnel-lattice async (Tokio, one shared PacketPool) | 1.72 | 93.7 % (89.3–93.9) | 180 % | 3.5 MB | 48 |
| tun-rs async (async-io) | 2.82 | — | 190 % | 2.8 MB | 78 |
| tunnel-lattice async (async-io, packet_stream + send_async) | 2.58 | 95.8 % (90.1–102.3) | 192 % | 2.7 MB | 71 |
| tun-rs sync offload (recv_multiple/send_multiple) | 9.58 | — | 135 % | 6.2 MB | 0 |
| tunnel-lattice sync offload (Handle::recv split, per-packet Handle::send) | 2.74 | 28.6 % (26.3–30.0) | 163 % | 2.5 MB | 0 |
| tun-rs async offload (Tokio, recv_multiple/send_multiple) | 9.84 | — | 144 % | 7.2 MB | 0 |
| tunnel-lattice async offload (Tokio, packet_stream + send_batch_async) | 10.95 | 111.3 % (109.7–112.9) | 150 % | 4.1 MB | 0 |
| tun-rs async offload (async-io, recv_multiple/send_multiple) | 9.19 | — | 144 % | 6.5 MB | 0 |
| tunnel-lattice async offload (async-io, packet_stream + send_batch_async) | 11.47 | 124.2 % (119.9–137.0) | 137 % | 3.1 MB | 0 |

Recorded 2026-10-02T08:56:32Z on GitHub Actions ubuntu24 20260927.320.1 runner, AMD EPYC 7763 64-Core Processor, 4 CPUs, Linux 6.17.0-1022-azure.
Code fdfc0e1; tun-rs 2.8.11, tunnel-lattice 0.5.0, iperf3 3.16; rustc 1.99.0 (b940084d7 2026-09-28), RUSTFLAGS `-C target-cpu=native`.
Method: two TUN devices (one moved into a network namespace) joined by the forwarder; `iperf3 -t 10` TCP from the host to a server in the namespace. Each row is the median of 5 runs (order rotated every repetition, 1 warm-up run(s) discarded). Throughput is iperf3's receiver rate. "vs tun-rs, same run" is the median (min–max) of the per-repetition ratio to the tun-rs baseline above it, measured in the same repetition. CPU is the forwarder process's user+system time per second sampled at 1 Hz (100 % = one core); RSS is the largest resident set sampled.
Reproduce: `scripts/bench-forward.sh build && sudo scripts/bench-forward.sh run && scripts/bench-forward.sh report <dir>`.
Absolute Gbps from shared or virtual runners are not comparable with numbers published on other hardware; compare the same-run ratio.

To reproduce on GitHub instead, start the manual **Forwarder benchmark**
workflow (`workflow_dispatch` in `.github/workflows/bench-forward.yml`); it
writes the table to the job summary and uploads `results.md`,
`results.json`, and every raw run as an artifact. Running locally needs
root, `iperf3`, and Linux. CI's `bench-forwarder` job (in
`.github/workflows/ci.yml`) only builds, lints, and tests the harness on
every push and pull request; it does not run the benchmark.

**What this can and cannot show.** Without offload, while the only backend
wraps `tun-rs`, Tunnel Lattice can at best match `tun-rs` minus its own
overhead; the ratio column measures that overhead. With offload, Tunnel
Lattice splits and coalesces super-packets itself, so the async offload
rows are not bounded by `tun-rs`'s offload path and exceeded it in this
run. The sync offload row receives one packet per call and sends each
packet on its own, so nothing is coalesced on the way out; batched receive
is not part of this release. The range in brackets is the spread over the
repetitions of this one recorded run only; it says nothing about how much
the ratio varies between runs or machines. Native per-OS backends are
planned after this.
`tun-rs`'s own published numbers come from different hardware, so they are
not comparable with this table.

#### Micro-benchmarks

These run on in-memory mock devices, with no privileges needed:

```bash
cargo bench -p tunnel-lattice-async                # stream paths vs a plain recv loop
cargo bench -p tunnel-lattice-async --bench pool   # PacketPool overhead and contention
```

## 📦 Installation

```toml
[dependencies]
# Synchronous API, no async runtime
tunnel-lattice = "0.6"

# Async packet stream on Tokio (multi-threaded runtime)
tunnel-lattice = { version = "0.6", features = ["tokio"] }

# Async packet stream on async-io (smol, async-std, ...)
tunnel-lattice = { version = "0.6", features = ["async-io"] }
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

Inside async code, send with `send_async(&packet).await`, not the blocking
`send` (which panics inside a Tokio task). It takes the received
`PacketBuf` without a copy and has the same errors as `send`. A dropped
`send_async` never sends part of a packet; on Windows whether it sent the
packet is unknown.

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

`persist` and `additional_queue` exist only on Linux, so this example does
not compile on Windows or macOS.

A later process re-attaches by opening the same name, kind, and
multi-queue setting, and can clear persistence so the device goes away
with its last handle. `unpersist` is Linux-only too, so this example does
not compile on Windows or macOS either:

```rust,no_run
use tunnel_lattice::{DeviceConfig, DeviceKind, Tunnel};

fn main() -> tunnel_lattice::Result<()> {
    let device = Tunnel::connect()
        .open(DeviceConfig::new(DeviceKind::Tun).with_name("tl0").with_multi_queue(true))?;
    device.unpersist()?; // removed once every handle to it is closed
    Ok(())
}
```

`open` does not report whether it attached or created a new device, and a
kind or multi-queue mismatch fails with `Error::AlreadyExists`.

### Batch Send and Segmentation Offload

`send_batch` sends a prefix of a packet list and returns how many packets
went out. It may send fewer than you passed (a short batch), so loop:

```rust,no_run
use tunnel_lattice::{Capability, DeviceConfig, DeviceKind, Tunnel};

fn main() -> tunnel_lattice::Result<()> {
    let config = DeviceConfig::new(DeviceKind::Tun).with_offload(true); // a request
    let device = Tunnel::connect().open(config)?;
    let offload = device.capabilities().contains(Capability::SEGMENTATION_OFFLOAD);
    println!("segmentation offload in use: {offload}");

    let packets: Vec<&[u8]> = Vec::new(); // whole IP packets, e.g. from recv
    let mut rest = &packets[..];
    while !rest.is_empty() {
        let sent = device.send_batch(rest)?; // Ok(n): packets[..n] went out
        rest = &rest[sent..];
    }
    Ok(())
}
```

- **Prefix contract.** `Ok(n)` means the first `n` packets were each sent
  whole and in order and the rest were not touched. `Err` means nothing
  was sent and the error belongs to the first packet. A failure after at
  least one packet was sent is reported as `Ok(k)`; the next call,
  starting at that packet, returns the error. An empty list gives `Ok(0)`.
- **Cancellation.** A dropped `send_batch_async` future has sent an
  unknown prefix of the list: each packet whole and at most once, never a
  packet after one that was not sent.
- **Offload is Linux TUN only, opt-in, and a request rather than a
  guarantee.** It is ignored without an error on Windows, macOS, and for
  TAP. `Capability::SEGMENTATION_OFFLOAD` on the open handle (never on
  `Tunnel::capabilities()`) says whether the queue uses it; a queue
  attached to an existing multi-queue device follows that device's
  framing, whatever this open asked for.
- **What offload changes.** Nothing a caller sees: `recv` and
  `packet_stream` still return one IP packet at a time, split out of the
  kernel's super-packets, and an async `recv` dropped mid-way loses no
  segment. On an offload queue `send_batch` sends at most 128 packets per
  call and merges adjacent packets of one TCP or UDP flow into one write;
  if the kernel refuses a merged write, those packets are resent one by
  one. Without offload, and on every other OS, `send_batch` sends packet
  by packet.
- **Device-wide side effect.** The kernel's offload setting belongs to
  the whole device, not one queue. Opening the device without offload,
  including a plain queue on a shared multi-queue offload device, turns
  super-packets off for every queue of it, and nothing restores the
  setting when a handle is dropped. Those queues keep working, one packet
  per read.

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
  [tap-windows6](https://github.com/OpenVPN/tap-windows6/releases) driver.
  Staging its package in the driver store is enough (for example
  `pnputil /add-driver OemVista.inf` from the release's `dist.win10.zip`):
  `open` creates its own adapter, so none has to be created in advance.
  `Tunnel::capabilities()` reports `Capability::TAP_DEVICES` on Windows
  only when this driver is installed; the check needs no Administrator
  rights, creates no adapter, and runs once per process.
- Run as Administrator. Without the DLL or driver, `open` returns
  `Error::DriverUnavailable`.

### macOS

- Run as root.
- **TUN** devices are `utun<N>`. **TAP** devices are `feth<N>` pairs
  (`N` ≤ 32767); leave the name unset to let the OS pick.

## 🤝 Comparison

Tunnel Lattice currently runs on `tun-rs`, so this compares what the
Tunnel Lattice layer adds, what it inherits from `tun-rs`, and what it does
not have yet.

| Feature | Tunnel Lattice | Source | tun-rs (the backend it wraps) |
|---------|----------------|--------|-------------------------------|
| **TUN on Linux, Windows, macOS** | ✅ | Inherited from `tun-rs` | ✅ |
| **TAP on Linux, Windows, macOS** | ✅ macOS via `feth` pairs | Inherited; Tunnel Lattice adds the bounded wait that ends a macOS async TAP `recv` | ✅ |
| **Error type** | ✅ One typed `Error`, same meaning on every OS | Tunnel Lattice | ⚠️ `std::io::Error` with platform codes |
| **Device deleted while `recv` waits** | ✅ Ends with `Disconnected` on every OS and feature set | Tunnel Lattice | ⚠️ Waits forever with Tokio on Linux and async TAP on macOS |
| **Oversize packet** | ✅ `BufferTooSmall`, never truncated | Tunnel Lattice | ⚠️ Behaviour differs per platform |
| **Async I/O** | ✅ `futures::Stream` and `send_async`, Tokio or async-io | Async I/O inherited; the stream is Tunnel Lattice | ✅ async `recv`/`send`, Tokio or async-io |
| **Allocation-free packet stream** | ✅ Built-in `PacketPool` | Tunnel Lattice | ➖ Caller-managed buffers |
| **Runtime capability flags** | ✅ Per host and per device | Tunnel Lattice | ❌ |
| **Backend injection** | ✅ `Tunnel::new(backend)` over provider traits | Tunnel Lattice | ❌ |
| **TAP MAC address** | ✅ Set at open; changed on an open device on Linux and macOS | Inherited | ✅ |
| **Persistent devices (Linux)** | ✅ Persist, re-attach by name, un-persist | Persist inherited; un-persist is Tunnel Lattice | ⚠️ Persist only |
| **Multi-queue (Linux)** | ✅ `additional_queue` | Inherited | ✅ |
| **Batch send** | ✅ `send_batch`, every OS; coalesced only on a Linux TUN offload queue | Tunnel Lattice | ✅ Linux (`send_multiple`) |
| **GSO/GRO offload** | ✅ Linux TUN, opt-in; one packet per `recv` | Tunnel Lattice (its own header parsing, segmentation, and coalescing) | ✅ Linux |
| **Batch receive** | ❌ One packet per `recv` | — | ✅ Linux (`recv_multiple`) |
| **Address and route setup** | ➖ Delegated to net-lattice | — | ✅ Built in |
| **Throughput** | Measured against `tun-rs` in the same run; see [Benchmarks](#-benchmarks) | — | Baseline |
| **Platforms** | Linux, Windows, macOS | — | 11+, including BSD, iOS, Android |

## 🛠️ API Overview

| Item | Purpose |
|------|---------|
| `Tunnel::connect()` | Connect to the default backend |
| `Tunnel::new(backend)` | Use any backend, including your own or a test double |
| `Tunnel::capabilities` | What the host supports before opening a device |
| `Tunnel::open(DeviceConfig)` | Create a TUN/TAP device and return a `Handle` |
| `DeviceConfig::with_offload` | Request Linux TUN segmentation offload (a hint; read `Capability::SEGMENTATION_OFFLOAD` back) |
| `Handle::id` / `kind` | The identity and kind captured at open (no native call) |
| `Handle::recv` / `send` | Blocking packet transfer |
| `Handle::send_batch` | Blocking send of a prefix of a packet list; returns how many were sent |
| `Handle::snapshot` | Current name, MTU, administrative state, and TAP MAC address |
| `Handle::apply(DeviceConfigPatch)` | Change MTU, TAP MAC address (Linux, macOS), or up/down |
| `Handle::capabilities` | What this device supports at runtime |
| `Handle::persist` / `unpersist` / `additional_queue` | Linux persistence and multi-queue |
| `Handle::packet_stream` | Async `Stream` of pooled packets (`tokio` / `async-io`) |
| `Handle::send_async` | Non-blocking send for async code (`tokio` / `async-io`) |
| `Handle::send_batch_async` | Non-blocking batch send for async code (`tokio` / `async-io`) |

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

With `tokio`, opening a device needs a Tokio runtime entered on the
calling thread, and the blocking `recv`/`send` need that runtime to be
**multi-threaded** (`#[tokio::main]`'s default flavor). A `current_thread`
runtime never drives the device's I/O for the blocking `recv`/`send`.
Inside async code use `packet_stream` and `send_async` instead;
`send_async` also works on a `current_thread` runtime. Use
`async-io` if you need blocking calls on a single-threaded runtime.
</details>

<details>
<summary><b><code>send</code> panics inside a Tokio task</b></summary>

"Cannot start a runtime from within a runtime": the blocking `send`/`recv`
block on the runtime and Tokio forbids that inside async code. Use
`send_async(&packet).await` and `packet_stream` there.
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
sudo -E cargo test -p tunnel-lattice --lib --features tokio -- --ignored   # facade, real devices
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
