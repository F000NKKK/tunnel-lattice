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

**Pre-release, active design/implementation.** No version has been
published yet (see `SUPPORT.md`). The workspace now has a real crate
architecture — see [ARCHITECTURE.md](ARCHITECTURE.md) — but nothing in it is
API-frozen; every type, trait, and feature flag may still change before
`0.1.0` ships.

## What it does

- Creates and configures TUN (raw IP) and TAP (Ethernet-framed) devices on
  Linux, Windows, and macOS through the `tun-rs` crate;
- Transfers packets on an open device, synchronously by default and, with
  the optional `async` feature, through a `futures::Stream` — no async
  runtime is pulled in unless that feature is enabled;
- Re-reads and patches an open device's MTU and administrative state.

Tunnel Lattice does not assign IP addresses to the interfaces it creates —
see `net-lattice` below for OS network configuration once a device exists.

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
