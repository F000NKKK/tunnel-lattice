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

[Overview](#-overview) • [Features](#-key-features) • [Platforms](#-supported-platforms) • [Performance](#-performance) • [Installation](#-installation) • [Quick Start](#-quick-start) • [Feature Flags](#-feature-flags) • [Batch Send](#-batch-send-and-segmentation-offload) • [Ownership](#-ownership-handle-is-clone) • [Privileges](#-platform-and-privilege-notes) • [Comparison](#-comparison)

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
- ✅ `Handle::send_batch`, sending a prefix of a list of packets in one call
  (see [Batch send and offload](#-batch-send-and-segmentation-offload));
- 🐧 `DeviceConfig::with_offload(true)`, an opt-in request for kernel
  segmentation offload on a Linux TUN device, read back through the
  handle's `Capability::SEGMENTATION_OFFLOAD`;
- ✅ `Handle::snapshot`, re-reading a device's current name/MTU/administrative
  state and, for TAP, its MAC address;
- ✅ `Handle::apply(DeviceConfigPatch)`, changing an open device's MTU,
  administrative state, or TAP MAC address (the last where the handle
  reports `Capability::MAC_MUTATION`: Linux and macOS; on Windows a TAP
  MAC address can only be requested at open with `DeviceConfig::with_mac`);
- 🐧 `Handle::persist`/`Handle::unpersist`/`Handle::additional_queue`, on
  backends that implement `PersistentDevice`/`MultiQueueProvider` (Linux
  only, via `tunnel-lattice-backend-tunrs` — see
  [Persistent devices and multi-queue](#-persistent-devices-and-multi-queue));
- ⚡ with the `async-io` or `tokio` feature (mutually exclusive):
  `Handle::packet_stream` and `Handle::packet_stream_with_pool`, a
  `futures::Stream` of received packets, each a `PacketBuf` view into a
  `PacketPool` slot (both re-exported here), with no allocation or copy per
  packet; and `Handle::send_async` and `Handle::send_batch_async`, the
  non-blocking sends to use inside async code.

## 💻 Supported Platforms

| Platform    | TUN | TAP | Sync | Tokio | async-io | Privilege | Driver |
|-------------|:---:|:---:|:----:|:-----:|:--------:|-----------|--------|
| **Linux**   | ✅  | ✅  | ✅   | ✅    | ✅       | `CAP_NET_ADMIN` (root or `setcap`) | `tun` kernel module (`/dev/net/tun`) for TUN and TAP |
| **Windows** | ✅  | ✅  | ✅   | ✅    | ✅       | Administrator | TUN: Wintun, `wintun.dll` next to the executable or on `PATH`; TAP: the tap-windows6 (`tap0901`) driver staged in the driver store |
| **macOS**   | ✅  | ✅  | ✅   | ✅    | ✅       | root | None: TUN is `utun`, TAP is a `feth` pair, both built in |

✅ tested in CI on real devices: the privileged jobs on `ubuntu-latest`,
`windows-latest`, and `macos-latest` open, use, and delete TUN and TAP
devices in every feature set (on Windows after downloading Wintun 0.14.1
and staging the tap-windows6 9.27.0 driver package), and a separate Linux
job checks persistence across two processes. Only platforms CI tests are
listed: `tun-rs` runs on more, but this crate does not claim them. See
[`tunnel-lattice-backend-tunrs`](https://crates.io/crates/tunnel-lattice-backend-tunrs)
for the per-OS mechanisms and error mapping.

## 🚀 Performance

Forwarding throughput, measured with the project's
[`bench/forwarder`](https://github.com/F000NKKK/tunnel-lattice/tree/main/bench/forwarder)
harness. It uses the method of tun-rs's
[tun-benchmark2](https://github.com/tun-rs/tun-benchmark2): a forwarder
copies packets between two TUN devices while `iperf3` measures TCP
throughput across them, and raw `tun-rs` is measured next to this crate in
the same run, on the same machine. Neither side uses offload. The table and
its notes are copied unchanged from the recorded
[workflow run](https://github.com/F000NKKK/tunnel-lattice/actions/runs/36855227427):

| Configuration | Throughput (median Gbps) | vs tun-rs, same run | CPU avg | RSS max | Retransmissions |
|---|---:|---:|---:|---:|---:|
| tun-rs sync | 2.87 | — | 173 % | 2.7 MB | 79 |
| tunnel-lattice sync (Handle::recv/send) | 2.73 | 94.9 % (94.4–96.6) | 173 % | 2.5 MB | 74 |
| tun-rs async (Tokio) | 2.35 | — | 179 % | 3.6 MB | 64 |
| tunnel-lattice async (Tokio, packet_stream + send_async) | 2.20 | 93.6 % (90.2–96.1) | 181 % | 3.5 MB | 57 |
| tunnel-lattice async (Tokio, one shared PacketPool) | 2.18 | 92.9 % (88.3–97.2) | 180 % | 3.5 MB | 61 |
| tun-rs async (async-io) | 3.04 | — | 187 % | 2.8 MB | 90 |
| tunnel-lattice async (async-io, packet_stream + send_async) | 2.58 | 86.2 % (80.3–92.3) | 183 % | 2.8 MB | 55 |

Recorded 2026-10-01T11:26:41Z on GitHub Actions ubuntu24 20260927.320.1 runner, AMD EPYC 9V74 80-Core Processor, 4 CPUs, Linux 6.17.0-1022-azure.
Code 9f5d286; tun-rs 2.8.11, tunnel-lattice 0.4.0, iperf3 3.16; rustc 1.98.1 (48a229cea 2026-09-01), RUSTFLAGS `-C target-cpu=native`.
Method: two TUN devices (one moved into a network namespace) joined by the forwarder; `iperf3 -t 10` TCP from the host to a server in the namespace. Each row is the median of 5 runs (order rotated every repetition, 1 warm-up run(s) discarded). Throughput is iperf3's receiver rate. "vs tun-rs, same run" is the median (min–max) of the per-repetition ratio to the tun-rs baseline above it, measured in the same repetition. CPU is the forwarder process's user+system time per second sampled at 1 Hz (100 % = one core); RSS is the largest resident set sampled.
Reproduce: `scripts/bench-forward.sh build && sudo scripts/bench-forward.sh run && scripts/bench-forward.sh report <dir>`.
Absolute Gbps from shared or virtual runners are not comparable with numbers published on other hardware; compare the same-run ratio.

The reproduce command runs from a checkout of the
[repository](https://github.com/F000NKKK/tunnel-lattice) on Linux, with
root and `iperf3`. On GitHub, the manual **Forwarder benchmark** workflow
(`workflow_dispatch`) runs the same script and uploads `results.md`,
`results.json`, and every raw run as an artifact.

While the only backend wraps `tun-rs`, this crate can at best match
`tun-rs` minus its own overhead; the ratio column measures that overhead.
The range in brackets is the spread over the repetitions of this one
recorded run only; it says nothing about how much the ratio varies between
runs or machines. Going beyond `tun-rs` needs batched I/O and GSO/GRO
offload, planned for 0.6, and native per-OS backends after that. `tun-rs`'s
own published
numbers come from different hardware, and its headline figures use
offload, so they are not comparable with this table.

## 📦 Installation

```toml
[dependencies]
# Synchronous API, no async runtime
tunnel-lattice = "0.5"

# Async packet stream on Tokio (multi-threaded runtime)
tunnel-lattice = { version = "0.5", features = ["tokio"] }

# Async packet stream on async-io (smol, async-std, ...)
tunnel-lattice = { version = "0.5", features = ["async-io"] }
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

### Async Send (Forwarding)

Inside async code, send with `send_async`, never the blocking `send`. A
`PacketBuf` derefs to `[u8]`, so a received packet is forwarded without a
copy:

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
        outside.send_async(&packet?).await?;
    }
    Ok(())
}
```

`send_async` returns the backend's own async send future unchanged, with
the same results and errors as `send`. Dropping it before it completes
never sends part of a packet: on Linux and macOS nothing was sent; on
Windows the packet may or may not have been sent. Portably, a packet is
sent whole at most once, and whether a dropped send went out is unknown.

## 🚩 Feature Flags

- **`tun-rs`** (default) — selects `tunnel-lattice-backend-tunrs`. A Cargo
  feature, not a `target_os` cfg gate, so a future non-`tun-rs` backend can
  be added alongside it rather than replacing it (see the project's
  `ARCHITECTURE.md`, "Backend replacement plan").
- **`async-io`** / **`tokio`** (mutually exclusive; enabling both is a
  compile error from `tun-rs`) — either adds `Handle::packet_stream`,
  `Handle::packet_stream_with_pool`, `Handle::send_async`, and
  `Handle::send_batch_async`. No async
  runtime dependency is imposed when neither feature is enabled.
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

**With `tokio`, `Tunnel::open` needs a Tokio runtime entered on the
calling thread, and the blocking `Handle::recv`/`send` need that runtime
to be multi-threaded** (`#[tokio::main]`'s default flavor, or
`Builder::new_multi_thread()`, entered with `Runtime::enter` on a thread
that is not running async code; see below). Opening registers the device
with the runtime. `tunnel-lattice-backend-tunrs`'s `PacketIo` then drives
tun-rs's Tokio-backed handle through
`tokio::runtime::Handle::current().block_on`, which only polls that
runtime's I/O driver on the `multi_thread` flavor; on `current_thread` the
first blocking `recv`/`send` call hangs forever. See that crate's README,
"`tokio` requires a multi-threaded runtime," for why. `snapshot` and
`apply` are plain OS queries and changes that never wait on the runtime.

The blocking `recv`/`send` also panic when called from inside async code
(a task, `#[tokio::main]`, or `block_on`): Tokio refuses to block inside
its own runtime. With `async-io` they do not panic but park the executor
thread. Inside async code use `send_async`, `send_batch_async`, and
`packet_stream`; their futures are polled by the runtime, so `send_async`
and `send_batch_async` also work on a `current_thread` runtime.

## 📨 Batch Send and Segmentation Offload

`Handle::send_batch(&[&[u8]])` (and `send_batch_async` with an async
feature) sends a prefix of a list of packets, in order, and returns how
many it sent:

- `Ok(n)`: `packets[..n]` were each sent whole and in order, and the rest
  were not touched. `n` may be less than the list's length (a short
  batch), so loop until everything is sent. An empty list gives `Ok(0)`.
- `Err(e)`: nothing was sent; `e` is the first packet's error, the same
  one `send` would return. A failure after at least one packet was sent is
  reported as `Ok(k)`, and the next call, starting at `packets[k]`,
  returns the error.
- Dropping a `send_batch_async` future before it completes leaves an
  unknown prefix sent: each packet whole and at most once, never a packet
  after one that was not sent. Await it to completion to know the count.

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
        let sent = device.send_batch(rest)?;
        rest = &rest[sent..];
    }
    Ok(())
}
```

`send_batch` works on every backend and OS. By default it sends one
packet at a time, exactly like a loop over `send`. With the `tun-rs`
backend, segmentation offload changes that on **Linux TUN devices only**:

- **Opt-in, and a request rather than a guarantee.** Offload is off by
  default. `DeviceConfig::with_offload(true)` asks for it; it is ignored
  without an error on Windows, macOS, and for TAP, and the handle then
  works exactly as without it. `Capability::SEGMENTATION_OFFLOAD` on the
  opened handle (never on `Tunnel::capabilities()`) says whether this
  queue really uses offload. On Linux the device decides: a queue attached
  to an existing multi-queue device takes that device's framing, whatever
  this open asked for.
- **Nothing changes for the caller.** `recv` still returns one IP packet
  per call. The kernel may hand over one TCP or UDP super-packet of up to
  64 KiB; the backend reads it once into a per-queue buffer (about 64 KiB
  per offload queue) and returns its segments one per `recv`, with
  lengths and checksums completed. An async `recv` dropped mid-way loses
  no segment, and `packet_stream` is unchanged.
- **`send_batch` coalesces.** On an offload queue a call sends at most 128
  packets (a short `Ok` beyond that), and adjacent packets of one TCP flow,
  or of one UDP flow with equal-sized datagrams when the kernel supports
  UDP segmentation offload, go out as one super-packet in one write that
  copies no payload. If the kernel refuses a super-packet, its packets are
  sent again one by one. Elsewhere `send_batch` falls back to sending
  packet by packet, with no 128-packet limit.
- **The kernel's offload setting is device-wide.** An offload open
  switches it on for the whole device. Any later open of the same device
  without offload, including a plain queue attached to a shared
  multi-queue offload device, switches it off again for every queue (the
  `tun-rs` library clears it on every such open). Those queues stay
  correct, but they then receive single packets instead of super-packets.
  Nothing restores the setting when a handle is dropped.

## 🤝 Ownership: `Handle` is `Clone`

`Handle<D>` wraps its device in an `Arc` and can be cloned cheaply to share
one open device across threads — `recv`/`send` take `&self`, so concurrent
calls through separate clones are always safe. Privileged CI checks this
on a real TUN device on Linux, macOS, and Windows in every feature set:
one clone blocks in `recv` while another sends, and the reply reaches the
waiting receiver. The device stays open until
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

A persistent device stays after its last handle closes. A later process
re-attaches by opening the same name, kind, and multi-queue setting (there
is no separate attach call, and `open` does not report whether it attached
or created a new device; a kind or multi-queue mismatch fails with
`Error::AlreadyExists`). `Handle::unpersist` clears persistence from any
handle or queue, so the device is removed once every handle to it, in any
process, is closed. Like `persist`, it exists only on Linux, so this
example does not compile on Windows or macOS either:

```rust,no_run
use tunnel_lattice::{DeviceConfig, DeviceKind, Tunnel};

fn main() -> tunnel_lattice::Result<()> {
    let config = DeviceConfig::new(DeviceKind::Tun).with_name("tl0");
    let device = Tunnel::connect().open(config)?; // re-attaches if tl0 persists
    device.unpersist()?;                          // idempotent
    drop(device);                                 // removed if this was the last handle
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

## 🤝 Comparison

This crate currently runs on `tun-rs`, so this compares what the Tunnel
Lattice layer adds, what it inherits from `tun-rs`, and what it does not
have yet.

| Feature | tunnel-lattice | Source | tun-rs (the backend it wraps) |
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
| **Address and route setup** | ➖ Delegated to [`net-lattice`](https://crates.io/crates/net-lattice) | — | ✅ Built in |
| **Throughput** | Measured against `tun-rs` in the same run; see [Performance](#-performance) | — | Baseline |
| **Platforms** | Linux, Windows, macOS | — | 11+, including BSD, iOS, Android |

## 📖 Documentation

- **API reference**: [docs.rs/tunnel-lattice](https://docs.rs/tunnel-lattice)
- **Async adapter**: [`tunnel-lattice-async`](https://crates.io/crates/tunnel-lattice-async)
- **Default backend**: [`tunnel-lattice-backend-tunrs`](https://crates.io/crates/tunnel-lattice-backend-tunrs)
- **Project**: [github.com/F000NKKK/tunnel-lattice](https://github.com/F000NKKK/tunnel-lattice)

## 📄 License

Licensed under the [Mozilla Public License 2.0](https://github.com/F000NKKK/tunnel-lattice/blob/main/LICENSE).
