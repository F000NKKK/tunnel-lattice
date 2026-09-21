# Tunnel Lattice

**Languages**

🇺🇸 **English** | 🇷🇺 [Русский](README.ru.md)

[![License: MPL 2.0](https://img.shields.io/badge/license-MPL--2.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/language-Rust-orange.svg)](https://www.rust-lang.org)
[![crates.io](https://img.shields.io/crates/v/tunnel-lattice.svg)](https://crates.io/crates/tunnel-lattice)
[![docs.rs](https://img.shields.io/docsrs/tunnel-lattice)](https://docs.rs/tunnel-lattice)
[![Downloads](https://img.shields.io/crates/d/tunnel-lattice.svg)](https://crates.io/crates/tunnel-lattice)
[![MSRV](https://img.shields.io/badge/MSRV-1.93-lightgrey.svg)](Cargo.toml)
[![CI](https://github.com/F000NKKK/tunnel-lattice/actions/workflows/ci.yml/badge.svg)](https://github.com/F000NKKK/tunnel-lattice/actions/workflows/ci.yml)

![Linux](https://img.shields.io/badge/Linux-in%20progress-yellow)
![Windows](https://img.shields.io/badge/Windows-in%20progress-yellow)
![macOS](https://img.shields.io/badge/macOS-in%20progress-yellow)

Cross-platform Rust library for TUN/TAP tunnel interfaces, designed to
compose with the rest of the Lattice networking stack.

## Status

**`0.2.0` published, active design/implementation.** See
[ARCHITECTURE.md](ARCHITECTURE.md) for the crate architecture — nothing in
it is API-frozen yet; every type, trait, and feature flag may still change
in a future `0.x` release (see `versioning.md`'s pre-1.0 policy).

## What it does

- Creates and configures TUN (raw IP) and TAP (Ethernet-framed) devices on
  Linux, Windows, and macOS through the `tun-rs` crate;
- Transfers packets on an open device, synchronously by default and, with
  the optional `async-io` or `tokio` feature (mutually exclusive), through a
  `futures::Stream` — no async runtime is pulled in unless one of them is
  enabled;
- Re-reads and patches an open device's MTU and administrative state.

Tunnel Lattice does not assign IP addresses to the interfaces it creates —
see "Interop with net-lattice" below.

## Interop with net-lattice

Tunnel Lattice and `net-lattice` never share a process-independent object
identity: a `tunnel_lattice::DeviceId` and a `net_lattice::InterfaceId` are
distinct phantom-typed wrappers even when their underlying native index
happens to coincide, so the two crates never let you pass one where the
other is expected by accident. Bridge them through the OS-assigned
**interface name** instead — the one field both sides expose in the same
shape:

```rust,no_run
use net_lattice::Lattice;
use tunnel_lattice::{DeviceConfig, DeviceKind, Tunnel};

let tunnel = Tunnel::connect();
let device = tunnel.open(DeviceConfig::new(DeviceKind::Tun))?;
let snapshot = device.snapshot()?; // has `snapshot.name`, e.g. "tun0"

let lattice = Lattice::connect()?;
let interface = lattice
    .interfaces()?
    .into_iter()
    .find(|i| i.name == snapshot.name)
    .ok_or(net_lattice::Error::NotFound)?;
// assign an address, bring it up, etc. through `net-lattice` from here.
# Ok::<(), Box<dyn std::error::Error>>(())
```

`name` is advisory on `DeviceConfig` (see its docs) — a backend may assign a
different name than requested, especially on Windows — so always read the
name back from `snapshot()`/the returned `Device`, never from the
`DeviceConfig` you passed in.

## Quick start

```rust,no_run
use tunnel_lattice::{DeviceConfig, DeviceKind, Result, Tunnel};

fn main() -> Result<()> {
    let tunnel = Tunnel::connect();
    let device = tunnel.open(DeviceConfig::new(DeviceKind::Tun).with_mtu(1500))?;
    let mut buf = vec![0u8; 1500];
    let len = device.recv(&mut buf)?;
    println!("{len} bytes");
    Ok(())
}
```

See `crates/tunnel-lattice/README.md` for feature flags and a fuller usage
walkthrough.

## Workspace crates

The workspace is split into focused crates. Each crate has its own
crate-level README with its scope and a usage example:

| Crate | Purpose |
| --- | --- |
| [`tunnel-lattice`](crates/tunnel-lattice/README.md) | Public facade: `Tunnel`/`Handle`, feature-gated backend selection |
| [`tunnel-lattice-model`](crates/tunnel-lattice-model/README.md) | Observed/desired device types (`Device`, `DeviceConfig`, `DeviceConfigPatch`) |
| [`tunnel-lattice-platform`](crates/tunnel-lattice-platform/README.md) | Provider traits and `Capability` contract |
| [`tunnel-lattice-core`](crates/tunnel-lattice-core/README.md) | Shared errors, results, and IDs |
| [`tunnel-lattice-async`](crates/tunnel-lattice-async/README.md) | Runtime-independent `futures::Stream` adapter for backends without native async |
| [`tunnel-lattice-backend-tunrs`](crates/tunnel-lattice-backend-tunrs/README.md) | `tun-rs`-backed cross-platform TUN/TAP implementation |

## The Lattice ecosystem

| Crate | Purpose |
| --- | --- |
| [net-lattice](https://github.com/F000NKKK/net-lattice) | OS networking inspection and configuration (routes, DNS, interfaces) |
| [tunnel-lattice](https://github.com/F000NKKK/tunnel-lattice) | TUN/TAP tunnel interfaces |
| [dns-lattice](https://github.com/F000NKKK/dns-lattice) | Programmable DNS control plane |
| [flow-lattice](https://github.com/F000NKKK/flow-lattice) | Policy compiler: rules -> platform-neutral network plans |
| [sdk-lattice](https://github.com/F000NKKK/sdk-lattice) | Application-facing SDK composing the crates above |

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Feedback on the crate architecture
and API shape in [ARCHITECTURE.md](ARCHITECTURE.md) is the most valuable
contribution at this stage.

## License

Licensed under the [Mozilla Public License 2.0](LICENSE).
