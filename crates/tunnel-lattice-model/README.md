<div align="center">

# 🧩 tunnel-lattice-model

### OS-Independent TUN/TAP Domain Types for Tunnel Lattice

[![crates.io](https://img.shields.io/crates/v/tunnel-lattice-model.svg?cacheSeconds=86400)](https://crates.io/crates/tunnel-lattice-model)
[![docs.rs](https://img.shields.io/docsrs/tunnel-lattice-model?cacheSeconds=86400)](https://docs.rs/tunnel-lattice-model)
[![License: MPL 2.0](https://img.shields.io/badge/license-MPL--2.0-blue.svg)](https://github.com/F000NKKK/tunnel-lattice/blob/main/LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.99-lightgrey.svg)](https://github.com/F000NKKK/tunnel-lattice)

[Overview](#-overview) • [Features](#-key-features) • [Installation](#-installation) • [Quick Start](#-quick-start)

</div>

---

## 📖 Overview

The data model of [Tunnel Lattice](https://github.com/F000NKKK/tunnel-lattice):
what a TUN/TAP device looks like, what you ask for when you create one, and
what you ask for when you change one. This crate models data and
contracts; it never inspects or changes the host system.

> Use this crate directly for building configurations offline or for
> backend development. Use the
> [`tunnel-lattice`](https://crates.io/crates/tunnel-lattice) facade to
> connect these types to an operating system.

## 🌟 Key Features

- ✅ **`DeviceKind`**: TUN (raw IP) or TAP (Ethernet-framed).
- ✅ **`DeviceConfig`**: desired intent for creating a device (kind, an
  optional name, an optional MTU, an optional TAP MAC address, a
  multi-queue request, a segmentation-offload request), built with
  `DeviceConfig::new`/`with_name`/`with_mtu`/`with_mac`/`with_multi_queue`/
  `with_offload`.
  The `name` field's docs list the per-OS name formats a backend accepts
  (anything else is rejected with `Error::InvalidState` before any native
  call) and
  what happens when an interface with that name already exists.
- ✅ **`DeviceConfig::offload`**: off by default. Like `multi_queue`, it is
  a request, not a guarantee: a backend ignores it without an error where
  it has no such concept (the `tun-rs` backend honours it for Linux TUN
  devices only), and the opened handle's
  `Capability::SEGMENTATION_OFFLOAD` flag says whether offload is in use.
  It never changes what a caller sees (each receive still returns one
  packet and each send takes one, with no offload header), and it is fixed
  at open: `DeviceConfigPatch` cannot change it.
- ✅ **`Device`**: an observed, already-open device (id, actual name, kind,
  MTU, administrative state, and a TAP device's MAC address; `None` for
  TUN). `Device::recv_buffer_len` returns a receive
  buffer size that fits one packet at the snapshot's MTU: the MTU for TUN,
  MTU + 18 for TAP (the 14-byte Ethernet header plus one 4-byte 802.1Q tag,
  so a double-tagged frame does not fit).
- ✅ **`DeviceConfigPatch`**: desired intent for changing an open device's
  MTU, administrative state, or TAP MAC address, separate from creation.
  Built with `DeviceConfigPatch::new`, which rejects an empty or zero-MTU
  patch the same way `net-lattice-model::InterfaceConfig::new` does, or
  with `DeviceConfigPatch::new_mac` for a MAC-only patch; `with_mac` adds a
  MAC address to either.
- ✅ **`MacAddress`**: six octets in transmission order, displayed as
  lowercase colon hex. Shaped like `net-lattice-model`'s own `MacAddress`,
  so the two convert through `[u8; 6]`.
- ✅ **`AdminState` / `DesiredAdminState`**: observed and requested states
  are split the same way `net-lattice-model::interface`'s pair is, so the
  observed `Unknown` is never requested back. Both enums are
  `#[non_exhaustive]`, so a `match` on either needs a wildcard arm.

### The `backend` Feature (for Backend Authors)

The non-default `backend` Cargo feature adds `tunnel_lattice_model::backend`,
a support API for crates that implement Tunnel Lattice backends. It is not
covered by any stability promise and may change in any minor release;
applications should not enable it, and the `tunnel-lattice` facade neither
enables nor re-exports it. It holds the OS-independent rules every backend
shares, with no I/O and no `unsafe`:

- `HostOs` (and `HostOs::CURRENT`) with the per-OS interface-name rules
  (`precheck_name`) and their constants;
- `precheck_open`, the checks `open` runs before any native call (kind,
  name, MTU, then MAC address), returning an `OpenRequest`;
- `offload_requested`, `admin_from_if_flags` and `admin_from_oper_status`;
- `apply_patch`, the `apply` contract (precondition order, MTU then MAC
  address then administrative state, reverse best-effort compensation),
  driven through a backend's `ApplySteps`.

```toml
[dependencies]
tunnel-lattice-model = { version = "0.6", features = ["backend"] }
```

### Out of Scope

- **Packet I/O**: a runtime concern of `tunnel-lattice-platform`'s provider
  traits.
- **IP address assignment**: belongs to
  [`net-lattice`](https://github.com/F000NKKK/net-lattice). Tunnel Lattice
  creates and configures the virtual interface only.

## 📦 Installation

```toml
[dependencies]
tunnel-lattice-model = "0.6"
# For `tunnel_lattice_core::Error`, used in the example below
tunnel-lattice-core = "0.6"
```

## 🎓 Quick Start

```rust
use tunnel_lattice_model::{
    DesiredAdminState, DeviceConfig, DeviceConfigPatch, DeviceId, DeviceKind,
};

fn main() -> Result<(), tunnel_lattice_core::Error> {
    let config = DeviceConfig::new(DeviceKind::Tun)
        .with_name("tun0")
        .with_mtu(1500)
        .with_multi_queue(true)
        .with_offload(true); // a request; the open handle's capability answers
    assert_eq!(config.kind, DeviceKind::Tun);
    assert!(config.multi_queue);
    assert!(config.offload);

    let patch = DeviceConfigPatch::new(DeviceId::new(1), Some(DesiredAdminState::Up), None)?;
    assert_eq!(patch.admin_state(), Some(DesiredAdminState::Up));
    Ok(())
}
```

## 📖 Documentation

- **API reference**: [docs.rs/tunnel-lattice-model](https://docs.rs/tunnel-lattice-model)
- **Project**: [github.com/F000NKKK/tunnel-lattice](https://github.com/F000NKKK/tunnel-lattice)

## 📄 License

Licensed under the
[Mozilla Public License 2.0](https://github.com/F000NKKK/tunnel-lattice/blob/main/LICENSE).
