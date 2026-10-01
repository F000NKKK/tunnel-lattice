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
- ✅ **`PacketIo`**: synchronous packet transfer on an open device handle.
- ✅ **`DeviceObserver`**: reads an open device's current name, MTU, and
  administrative state back, and returns the device identity captured at
  open without a native call.
- ✅ **`DeviceMutator: DeviceObserver`**: changes an open device's MTU or
  administrative state, gated by `Capability::DEVICE_MUTATION`.
- ✅ **`PersistentDevice`**: marks an open device to survive process exit,
  gated by `Capability::PERSISTENT_DEVICES`.
- ✅ **`MultiQueueProvider`**: duplicates a kernel-scheduled queue on the
  same device for another thread, gated by `Capability::MULTI_QUEUE`. This
  is separate from ordinary multi-threaded use of `PacketIo`, which is
  always safe on every backend since `recv`/`send` take `&self`; see that
  trait's docs.
- ⚡ **`AsyncPacketIo`** (feature `async`): a native non-blocking transfer
  path a backend implements when it has real async I/O rather than a
  blocking syscall. See that trait's docs for why it is preferred over
  `tunnel-lattice-async`'s generic thread-based adapter when available.

### Capabilities
- 🧩 **`Capability`**: a `bitflags` set of runtime-dependent features:
  `DEVICE_MUTATION`, `PERSISTENT_DEVICES`, `TAP_DEVICES`, `MULTI_QUEUE`,
  `NATIVE_ASYNC`.
- 🧩 **`CapabilityProvider`**: reports which of them a connected device has.

## 📦 Installation

```toml
[dependencies]
tunnel-lattice-platform = "0.4"

# With the native async packet I/O trait
tunnel-lattice-platform = { version = "0.4", features = ["async"] }
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

## 📖 Documentation

- **API reference**: [docs.rs/tunnel-lattice-platform](https://docs.rs/tunnel-lattice-platform)
- **Project**: [github.com/F000NKKK/tunnel-lattice](https://github.com/F000NKKK/tunnel-lattice)

## 📄 License

Licensed under the [Mozilla Public License 2.0](https://github.com/F000NKKK/tunnel-lattice/blob/main/LICENSE).
