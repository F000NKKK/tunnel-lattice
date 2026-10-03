<div align="center">

# 🔌 tunnel-lattice-platform

### Provider Traits and the Capability Contract for Tunnel Lattice Backends

[![crates.io](https://img.shields.io/crates/v/tunnel-lattice-platform.svg?cacheSeconds=86400)](https://crates.io/crates/tunnel-lattice-platform)
[![docs.rs](https://img.shields.io/docsrs/tunnel-lattice-platform?cacheSeconds=86400)](https://docs.rs/tunnel-lattice-platform)
[![License: MPL 2.0](https://img.shields.io/badge/license-MPL--2.0-blue.svg)](https://github.com/F000NKKK/tunnel-lattice/blob/main/LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.93-lightgrey.svg)](https://github.com/F000NKKK/tunnel-lattice)

[Overview](#-overview) • [Traits](#-key-features) • [Installation](#-installation) • [Backend Contract](#-backend-contract)

</div>

---

## 📖 Overview

The boundary between [Tunnel Lattice](https://github.com/F000NKKK/tunnel-lattice)'s
domain model and its platform backends: generic provider traits and the
runtime `Capability` flags. It depends only on `tunnel-lattice-core`, never
on `tunnel-lattice-model`.

### 🎯 Why This Crate Exists

This boundary is what lets the current `tun-rs`-backed implementation
(`tunnel-lattice-backend-tunrs`) be replaced later by hand-written per-OS
TUN/TAP code without moving this crate or the `tunnel-lattice` facade's
public API. The workspace
[ARCHITECTURE.md](https://github.com/F000NKKK/tunnel-lattice/blob/main/ARCHITECTURE.md),
"Backend replacement plan", describes how.

> Application code does not use this crate directly; see the
> [`tunnel-lattice`](https://crates.io/crates/tunnel-lattice) facade.
> Implement these traits when writing a new backend.

## 🌟 Key Features

### Provider Traits
- ✅ **`DeviceProvider`**: opens a TUN/TAP device, with generic
  `DeviceConfig`/`Device` associated types (this crate never names
  `tunnel_lattice_model` types directly).
- ✅ **`PacketIo`**: synchronous packet transfer on an open device handle:
  `recv`, `send`, a provided `send_batch` that sends a prefix of a packet
  list, and a provided `recv_batch` that receives packets into
  caller-provided buffers (see "Backend Contract" below). The default
  `send_batch` sends one packet at a time and the default `recv_batch`
  receives exactly one packet; a backend overrides them when it can move
  several packets more cheaply. Both keep the trait dyn-compatible.
- ✅ **`DeviceObserver`**: reads an open device's current name, MTU, and
  administrative state back, and returns the device identity captured at
  open without a native call.
- ✅ **`DeviceMutator: DeviceObserver`**: changes an open device's MTU or
  administrative state, gated by `Capability::DEVICE_MUTATION`.
- ✅ **`PersistentDevice`**: `persist` marks an open device to survive
  process exit, and `unpersist` clears that again so the device is
  destroyed when its last handle closes; gated by
  `Capability::PERSISTENT_DEVICES`. A later process re-attaches by opening
  the same name, kind, and multi-queue setting.
- ✅ **`MultiQueueProvider`**: duplicates a kernel-scheduled queue on the
  same device for another thread, gated by `Capability::MULTI_QUEUE`. This
  is separate from ordinary multi-threaded use of `PacketIo`, which is
  always safe on every backend since `recv`/`send` take `&self`; see that
  trait's docs.
- ⚡ **`AsyncPacketIo`** (feature `async`): a native non-blocking transfer
  path a backend implements when it has real async I/O rather than a
  blocking syscall. See that trait's docs for why it is preferred over
  `tunnel-lattice-async`'s generic thread-based adapter when available.
  It has the same provided `send_batch`, awaiting `send` once per packet by
  default, and the same provided `recv_batch`, awaiting `recv` once.

### Capabilities
- 🧩 **`Capability`**: a `bitflags` set of runtime-dependent features:
  `DEVICE_MUTATION`, `PERSISTENT_DEVICES`, `TAP_DEVICES`, `MULTI_QUEUE`,
  `NATIVE_ASYNC`, `MAC_MUTATION`, `SEGMENTATION_OFFLOAD`.
  `SEGMENTATION_OFFLOAD` is reported on an open handle only, never for the
  host: it means the handle uses kernel segmentation offload internally
  (`recv` splits each offloaded super-packet back into single packets, and
  `send_batch` may coalesce adjacent packets of one flow into one native
  write), while every receive still returns exactly one packet with no
  offload header.
- 🧩 **`CapabilityProvider`**: reports which of them a connected device has.
  A backend may implement it too, answering for the host before any device
  is opened (for example whether a TAP driver is installed); an open
  device then adds the flags that exist only per handle. The answer is
  infallible and advisory: a flag the backend cannot confirm is absent, and
  `open` stays the authoritative check.

## 📦 Installation

```toml
[dependencies]
tunnel-lattice-platform = "0.6"

# With the native async packet I/O trait
tunnel-lattice-platform = { version = "0.6", features = ["async"] }
```

### The `backend` Feature (for Backend Authors)

The non-default `backend` Cargo feature adds
`tunnel_lattice_platform::backend`, a support API for crates that implement
Tunnel Lattice backends. It is not covered by any stability promise and may
change in any minor release; applications should not enable it, and the
`tunnel-lattice` facade neither enables nor re-exports it. It holds the
packet-path rules every backend shares, as safe Rust with no I/O, no
`unsafe` and no `libc` dependency:

- `Step`, `recv_step`, `send_step` and `tap_abort_retries`: what a `recv` or
  `send` loop does after one native attempt (retry, or finish with a
  result). What is transient or fatal is the caller's rule, passed in as a
  closure;
- the `recv_batch` drain: `DrainStep`, `batch_capacity`, `DeferredTooSmall`,
  `drain_plain` and `recv_batch_blocking`, generic over the error type of
  the read (`std::io::Error` for one backend, a raw `errno` for another)
  with the classifier passed as a closure;
- `backend::errno`: the raw error codes as plain integers, and for Linux the
  table (`linux::classify`) that maps an `errno` and the operation that
  produced it (`linux::Op`) to an outcome (`linux::Class`), plus the subset
  of it (`linux::packet_rule`) that a `std::io::Error`-based backend can
  apply to the raw code of a packet-path failure. The crate's own tests
  check every Linux constant against `libc`.

```toml
[dependencies]
tunnel-lattice-platform = { version = "0.6", features = ["backend"] }
```

## 📐 Backend Contract

A backend's `PacketIo::recv` and `AsyncPacketIo::recv` must return `Ok(n)`
only after writing all of `buf[..n]`, with `n <= buf.len()`. A packet that
does not fit in `buf` must never be truncated: the backend discards it,
returns `Error::BufferTooSmall`, and keeps the device usable for the next
`recv`. `Device::recv_buffer_len` gives a buffer size that fits at the
device's current MTU.

Callers such as `tunnel-lattice-async`'s buffer pool reuse receive buffers
without clearing them, so reporting bytes that were not written would
expose data from an earlier packet.

Dropping an `AsyncPacketIo::send` future before it completes must leave
either the whole packet sent or nothing, never part of it and never twice,
and must not use `buf` afterwards (copy it first if the write can outlive
the future). Callers may rely only on "the whole packet at most once;
unknown after drop".

Dropping an `AsyncPacketIo::recv` future while it waits must lose no
packet: the implementation must not hold a packet it took from the device
across an `.await`. A backend that cannot guarantee this on some OS
documents what a dropped receive may lose there.

`recv_batch(bufs, lens)` (sync and async) receives up to
`min(bufs.len(), lens.len())` packets, and an override must keep this
contract:

- `Ok(n)`: packets were written to `bufs[..n]` in device order, `lens[i]`
  is the length of packet `i` (at most `bufs[i].len()`), and each
  `bufs[i][..lens[i]]` is exactly one packet framed as `recv` frames it.
  Only those bytes are meaningful: the backend may have written anywhere
  in every buffer.
- `Ok(0)` only for zero capacity, without a native call.
- Wait for the first packet only: once one packet was taken from the
  device, never wait again and return a short batch instead.
- `Err(e)`: no packet was received, and `e` is classified as `recv` would
  classify it. A failure after `k >= 1` packets is reported as `Ok(k)`, and
  the next call on the same queue must see it; no error is swallowed.
- A packet too large for `bufs[0]` is discarded as `BufferTooSmall`. One
  too large for `bufs[k]`, `k >= 1`, ends the batch: either it stays
  queued, or, if it was already read, the next `recv` or `recv_batch` on
  that queue returns `BufferTooSmall` first.
- Dropping an `AsyncPacketIo::recv_batch` future loses no packet beyond
  what dropping a `recv` future in the same state would: no `.await` after
  the first packet is taken, and any packet taken but not returned (such as
  the remaining segments of an offloaded super-packet) stays in state owned
  by the queue.

`send_batch` (sync and async) follows a prefix contract with no deferred
errors, and an override must keep it:

- `Ok(n)`: `packets[..n]` were each sent whole and in order, and
  `packets[n..]` were not touched. `n` may be less than `packets.len()` (a
  short batch), so a caller that must send everything loops from
  `packets[n]`. An empty slice gives `Ok(0)`.
- `Err(e)`: nothing was sent, and `e` belongs to `packets[0]`, classified
  as `send` would classify it. A failure after at least one packet was sent
  is reported as `Ok(k)`; the next call, starting at `packets[k]`, sees the
  error.
- Dropping an `AsyncPacketIo::send_batch` future before it completes leaves
  an unknown prefix of `packets` sent: each packet whole and at most once,
  and never a packet after one that was not sent. A caller that must know
  how many went out awaits the future to completion.

## 📖 Documentation

- **API reference**: [docs.rs/tunnel-lattice-platform](https://docs.rs/tunnel-lattice-platform)
- **Project**: [github.com/F000NKKK/tunnel-lattice](https://github.com/F000NKKK/tunnel-lattice)

## 📄 License

Licensed under the
[Mozilla Public License 2.0](https://github.com/F000NKKK/tunnel-lattice/blob/main/LICENSE).
