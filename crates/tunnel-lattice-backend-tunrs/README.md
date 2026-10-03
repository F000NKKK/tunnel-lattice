<div align="center">

# ⚙️ tunnel-lattice-backend-tunrs

### The `tun-rs`-Backed TUN/TAP Backend for Tunnel Lattice

[![crates.io](https://img.shields.io/crates/v/tunnel-lattice-backend-tunrs.svg?cacheSeconds=86400)](https://crates.io/crates/tunnel-lattice-backend-tunrs)
[![docs.rs](https://img.shields.io/docsrs/tunnel-lattice-backend-tunrs?cacheSeconds=86400)](https://docs.rs/tunnel-lattice-backend-tunrs)
[![License: MPL 2.0](https://img.shields.io/badge/license-MPL--2.0-blue.svg)](https://github.com/F000NKKK/tunnel-lattice/blob/main/LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.99-lightgrey.svg)](https://github.com/F000NKKK/tunnel-lattice)

![Linux](https://img.shields.io/badge/Linux-supported-success)
![Windows](https://img.shields.io/badge/Windows-supported-success)
![macOS](https://img.shields.io/badge/macOS-supported-success)

[Overview](#-overview) • [Platforms](#-supported-platforms) • [Installation](#-installation)
• [Errors](#-errors) • [Opening](#-opening-a-device)
• [Privileges](#-privileges-and-runtime-requirements) • [Offload](#-segmentation-offload-linux-tun)

</div>

---

## 📖 Overview

The cross-platform TUN/TAP backend of
[Tunnel Lattice](https://github.com/F000NKKK/tunnel-lattice), built on
[`tun-rs`](https://github.com/tun-rs/tun-rs) (minimum and verified version
2.8.11). It implements `tunnel-lattice-platform`'s provider traits and turns
every `tun-rs`/OS error into a typed `tunnel_lattice_core::Error`.

> Not used directly: the
> [`tunnel-lattice`](https://crates.io/crates/tunnel-lattice) facade selects
> this backend by default through its `tun-rs` feature. Depend on it
> directly only to drive the provider traits without the facade.

What it adds on top of `tun-rs`:

- **Typed errors** with the same meaning on every OS (`Disconnected`,
  `DriverUnavailable`, `AlreadyExists`, `BufferTooSmall`, ...).
- **Device removal always ends `recv`**, including the Linux Tokio and
  async macOS TAP cases where `tun-rs`'s own wait never returns.
- **No truncation**: an oversize packet is reported, not cut.
- **Honest open**: names the OS would not honor exactly are rejected up
  front, and an existing interface is never adopted and then destroyed.
- **Linux TUN segmentation offload** with its own header parsing,
  segmentation, and coalescing, plus `send_batch` on every OS.

The rules every backend shares (name and open checks, the `apply` step
order, the `recv_batch` drain, the Linux `errno` table, and the offload codec
and engine) are not private to this crate: they come from the non-default
`backend` (and, on Linux targets, `offload`) support modules of
`tunnel-lattice-model` and `tunnel-lattice-platform`. This crate keeps only
the `tun-rs` calls, its error classifiers, and the trait implementations.

Main surface:

- `TunRsBackend` (`#[non_exhaustive]`; build with `new()`/`default()`):
  `DeviceProvider::open` and a host-level `CapabilityProvider`.
- `TunRsDevice`: `PacketIo` (`recv`, `recv_batch`, `send`, `send_batch`),
  `DeviceObserver`, `DeviceMutator` (MTU, TAP MAC, admin state),
  `CapabilityProvider`, and with an async feature `AsyncPacketIo`
  (`Capability::NATIVE_ASYNC`). On Linux only, `PersistentDevice` and
  `MultiQueueProvider`; elsewhere they are not implemented at all.

## 💻 Supported Platforms

Tested in CI on real devices (TUN and TAP, sync, Tokio, async-io):

- **Linux** — `/dev/net/tun`; persistence, multi-queue, TUN offload.
- **Windows** — TUN via Wintun (`wintun.dll`), TAP via tap-windows6.
- **macOS** — TUN via `utun`, TAP via `feth` pairs and BPF.

Admin state is read from the host on every `snapshot`; Windows applies a
change asynchronously, so a snapshot right after `apply` may lag briefly.

Host-level capabilities (no device opened, no privilege needed):
`DEVICE_MUTATION` always; `PERSISTENT_DEVICES` and `MULTI_QUEUE` on Linux;
`NATIVE_ASYNC` with an async feature; `TAP_DEVICES` on Linux and macOS, and
on Windows only when the tap-windows6 driver is installed (detected once per
process, advisory only). `MAC_MUTATION` (TAP on Linux and macOS) and
`SEGMENTATION_OFFLOAD` are reported only by an open handle.

```rust
use tunnel_lattice_backend_tunrs::TunRsBackend;
use tunnel_lattice_platform::{Capability, CapabilityProvider};

let host = TunRsBackend::new().capabilities();
if host.contains(Capability::TAP_DEVICES) {
    // a TAP device can be requested on this host
}
```

## 📦 Installation

```toml
[dependencies]
tunnel-lattice-backend-tunrs = "0.6"
# or, with the native async path (mutually exclusive):
tunnel-lattice-backend-tunrs = { version = "0.6", features = ["tokio"] }
tunnel-lattice-backend-tunrs = { version = "0.6", features = ["async-io"] }
```

`async-io` and `tokio` select `tun-rs`'s two async backends; enabling both
is a compile error from `tun-rs` itself, so `--all-features` is not valid.

## 🧭 Errors

- Every `io::Error` is mapped by `io::ErrorKind` first: `PermissionDenied`,
  `NotFound`, `AlreadyExists`, `Unsupported`, and
  `BrokenPipe`/`UnexpectedEof`/`NotConnected` → `Disconnected`. Anything
  else becomes `Platform(PlatformErrorCode)`, tagged by OS, or `Unknown`
  when there is no OS code (never a fabricated `0`).
- `recv` never truncates: an oversize packet is discarded as
  `BufferTooSmall` and the device stays usable. `EINTR` and a transient
  empty macOS TAP read are retried internally.
- `recv_batch` on Linux waits for the first packet like `recv`, then
  drains what is already queued without waiting again and without
  toggling `O_NONBLOCK` (`preadv2` with `RWF_NOWAIT` in the blocking
  build). An empty queue ends the batch quietly, `EINTR` repeats the
  read, and an offload `EINVAL` or invalid frame is dropped; any other
  error after the first packet ends the batch and is left for the next
  call. A packet after the first that is too long for its buffer makes
  the next `recv`/`recv_batch` on that handle return `BufferTooSmall`
  first.
  A dropped async `recv_batch` has received nothing. Windows and macOS
  return one packet per call.
- Deleting a Linux device, or the peer `feth` of a macOS TAP device, ends a
  waiting `recv` with `Disconnected` in every feature set. A device that is
  down or disabled reports `InvalidState`, with two exceptions: a down
  Linux device makes `recv` wait instead, and a down macOS device still
  accepts `send`. Applying `DesiredAdminState::Up` on the same handle
  recovers it, except for a Windows adapter disabled outside this crate.
- `DriverUnavailable` comes only from `open`: a missing `wintun.dll` or
  tap-windows6 driver on Windows, or a missing `tun` module on Linux.

Per-OS rules, native codes, and the retry details are in
[ARCHITECTURE.md, "Error model"](https://github.com/F000NKKK/tunnel-lattice/blob/main/ARCHITECTURE.md#error-model)
and the [API reference](https://docs.rs/tunnel-lattice-backend-tunrs).

## 🚪 Opening a Device

- A name or MTU the OS cannot honor exactly is rejected with `InvalidState`
  before any native call: Linux names are at most 15 bytes with no `%`;
  macOS TAP names are `feth<N>` (N ≤ 32767) and TUN names `utun<N>`;
  Windows names are at most 255 UTF-16 units; MTU is at most `u16::MAX`.
  Leave the name unset to let the OS choose.
- `open` never adopts an existing interface and then deletes it. An
  existing name gives `AlreadyExists`, except two documented attach cases
  that are never deleted on drop: a Linux persistent or multi-queue device
  of the same kind and setting, and an existing Wintun adapter on Windows.
  A Windows `Tun` open fails with `Platform(Windows(1247))` while another
  handle holds that Wintun adapter's session (the other handle keeps
  working), and with `Platform(Windows(code))` when the existing adapter of
  that name is not a Wintun adapter.
- `open` does not report whether it attached or created. Joining another
  user's Linux multi-queue device requires `CAP_NET_ADMIN`.
- `DeviceConfig::with_mac` sets a TAP MAC address at open on every OS; if
  it was not applied, `open` returns `Unsupported` and removes the device.

## 🔐 Privileges and Runtime Requirements

- Opening needs `CAP_NET_ADMIN` (Linux), Administrator (Windows), or root
  (macOS); without it `open` reports `Error::PermissionDenied`.
- **Windows TUN** loads `wintun.dll` at runtime (not linked or vendored):
  ship it from [wintun.net](https://www.wintun.net/) next to the executable
  or on `PATH`. **Windows TAP** needs the
  [tap-windows6](https://github.com/OpenVPN/tap-windows6/releases) driver
  (`tap0901`) staged, e.g. `pnputil /add-driver OemVista.inf`; adapters are
  created and removed by `open` and drop.
- **With `tokio`**, `open` must run inside an entered Tokio runtime, and the
  blocking `PacketIo::recv`/`send` need a **multi-threaded** runtime: on
  `current_thread` the first blocking call hangs forever. Never call them
  from async code (Tokio panics; `async-io` parks the executor); use
  `AsyncPacketIo` there, which works on any runtime flavor.
- To detect device removal, a Linux handle with `tokio`, and a macOS TAP
  handle with either async feature, holds one extra file descriptor.

## 🔀 Persistent Devices and Multi-Queue (Linux)

- `PersistentDevice::persist`/`unpersist` set and clear `IFF_PERSIST`
  (idempotent, from any queue). A later process re-attaches by opening the
  same name, kind, and multi-queue setting.
- `DeviceConfig::with_multi_queue(true)` enables `additional_queue`, an
  independent kernel-scheduled queue; without it the call returns
  `Error::Unsupported`. Sharing a handle across threads needs no extra
  queue: `recv`/`send` take `&self`.

## 📦 Segmentation Offload (Linux TUN)

- `DeviceConfig::with_offload(true)` is a request, off by default, ignored
  without error on Windows, macOS, and TAP. `Capability::SEGMENTATION_OFFLOAD`
  on the open handle says whether this queue really uses offload; the
  backend reads the queue's real framing back after every open.
- `recv` returns one IP packet per call: super-packets (up to 64 KiB) are
  staged in a per-queue buffer and split with lengths and checksums
  completed. A dropped async `recv` loses no segment. `recv_batch`
  returns the segments of one or more super-packets in one call.
- `send_batch` sends at most 128 packets per call and coalesces adjacent
  packets of one TCP flow (or UDP flow, when the kernel supports it) into
  one write without copying payload; a refused super-packet is resent
  packet by packet. The prefix contract (`Ok(n)` / `Err` = nothing sent)
  holds.
- **Device-wide side effect**: the kernel's offload setting covers every
  queue and process; any later open without offload turns it off again,
  and nothing restores it on drop.
- Offload needs no privilege beyond opening the device. A big-endian host
  whose device was switched to little-endian headers by another program is
  not supported (and not detected).

Details:
[ARCHITECTURE.md, "Segmentation offload"](https://github.com/F000NKKK/tunnel-lattice/blob/main/ARCHITECTURE.md#segmentation-offload-linux-tun).

## 📖 Documentation

- **API reference**: [docs.rs/tunnel-lattice-backend-tunrs](https://docs.rs/tunnel-lattice-backend-tunrs)
- **Design and backend replacement plan**:
  [ARCHITECTURE.md](https://github.com/F000NKKK/tunnel-lattice/blob/main/ARCHITECTURE.md)
- **Facade**: [`tunnel-lattice`](https://crates.io/crates/tunnel-lattice)

## 📄 License

Licensed under the
[Mozilla Public License 2.0](https://github.com/F000NKKK/tunnel-lattice/blob/main/LICENSE).

Built on [`tun-rs`](https://github.com/tun-rs/tun-rs) and, on Windows,
[Wintun](https://www.wintun.net/).
