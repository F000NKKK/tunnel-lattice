<div align="center">

# 🧩 tunnel-lattice-model

### OS-Independent TUN/TAP Domain Types for Tunnel Lattice

[![crates.io](https://img.shields.io/crates/v/tunnel-lattice-model.svg?cacheSeconds=86400)](https://crates.io/crates/tunnel-lattice-model)
[![docs.rs](https://img.shields.io/docsrs/tunnel-lattice-model?cacheSeconds=86400)](https://docs.rs/tunnel-lattice-model)
[![License: MPL 2.0](https://img.shields.io/badge/license-MPL--2.0-blue.svg)](https://github.com/F000NKKK/tunnel-lattice/blob/main/LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.93-lightgrey.svg)](https://github.com/F000NKKK/tunnel-lattice)

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
  optional name, an optional MTU, a multi-queue request), built with
  `DeviceConfig::new`/`with_name`/`with_mtu`/`with_multi_queue`. The `name`
  field's docs list the per-OS name formats a backend accepts (anything
  else is rejected with `Error::InvalidState` before any native call) and
  what happens when an interface with that name already exists.
- ✅ **`Device`**: an observed, already-open device (id, actual name, kind,
  MTU, administrative state). `Device::recv_buffer_len` returns a receive
  buffer size that fits one packet at the snapshot's MTU: the MTU for TUN,
  MTU + 18 for TAP (the 14-byte Ethernet header plus one 4-byte 802.1Q tag,
  so a double-tagged frame does not fit).
- ✅ **`DeviceConfigPatch`**: desired intent for changing an open device's
  MTU or administrative state, separate from creation. Built with
  `DeviceConfigPatch::new`, which rejects an empty or zero-MTU patch the
  same way `net-lattice-model::InterfaceConfig::new` does.
- ✅ **`AdminState` / `DesiredAdminState`**: observed and requested states
  are split the same way `net-lattice-model::interface`'s pair is, so the
  observed `Unknown` is never requested back. Both enums are
  `#[non_exhaustive]`, so a `match` on either needs a wildcard arm.

### Out of Scope

- **Packet I/O**: a runtime concern of `tunnel-lattice-platform`'s provider
  traits.
- **IP address assignment**: belongs to
  [`net-lattice`](https://github.com/F000NKKK/net-lattice). Tunnel Lattice
  creates and configures the virtual interface only.

## 📦 Installation

```toml
[dependencies]
tunnel-lattice-model = "0.4"
```

## 🎓 Quick Start

```rust
use tunnel_lattice_model::{DeviceConfig, DeviceConfigPatch, DeviceId, DeviceKind, DesiredAdminState};

fn main() -> Result<(), tunnel_lattice_core::Error> {
    let config = DeviceConfig::new(DeviceKind::Tun)
        .with_name("tun0")
        .with_mtu(1500)
        .with_multi_queue(true);
    assert_eq!(config.kind, DeviceKind::Tun);
    assert!(config.multi_queue);

    let patch = DeviceConfigPatch::new(DeviceId::new(1), Some(DesiredAdminState::Up), None)?;
    assert_eq!(patch.admin_state(), Some(DesiredAdminState::Up));
    Ok(())
}
```

## 📖 Documentation

- **API reference**: [docs.rs/tunnel-lattice-model](https://docs.rs/tunnel-lattice-model)
- **Project**: [github.com/F000NKKK/tunnel-lattice](https://github.com/F000NKKK/tunnel-lattice)

## 📄 License

Licensed under the [Mozilla Public License 2.0](https://github.com/F000NKKK/tunnel-lattice/blob/main/LICENSE).
