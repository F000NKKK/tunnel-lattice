<div align="center">

# 🧱 tunnel-lattice-core

### Shared Errors, Results, and IDs for Tunnel Lattice

[![crates.io](https://img.shields.io/crates/v/tunnel-lattice-core.svg?cacheSeconds=86400)](https://crates.io/crates/tunnel-lattice-core)
[![docs.rs](https://img.shields.io/docsrs/tunnel-lattice-core?cacheSeconds=86400)](https://docs.rs/tunnel-lattice-core)
[![License: MPL 2.0](https://img.shields.io/badge/license-MPL--2.0-blue.svg)](https://github.com/F000NKKK/tunnel-lattice/blob/main/LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.93-lightgrey.svg)](https://github.com/F000NKKK/tunnel-lattice)

[Overview](#-overview) • [Features](#-key-features) • [Installation](#-installation) • [Quick Start](#-quick-start)

</div>

---

## 📖 Overview

The foundation crate of [Tunnel Lattice](https://github.com/F000NKKK/tunnel-lattice):
the error, ID, and result types every other crate in the workspace shares.
It has no OS dependency and no TUN/TAP-specific types.

> Application code normally depends on the
> [`tunnel-lattice`](https://crates.io/crates/tunnel-lattice) facade, which
> re-exports these types. Use this crate directly only when building a new
> backend or a tool against Tunnel Lattice's error and ID shape.

## 🌟 Key Features

- ✅ **`Error`**: the single `#[non_exhaustive]` error type every provider
  trait method returns instead of a raw OS error (`std::io::Error`, a bare
  `errno`, a Windows `DWORD`). Besides the usual typed variants it has:
  - `DriverUnavailable`, returned only when opening a device fails because
    the OS driver or user-mode runtime (for example `wintun.dll` or the
    Linux `tun` module) is missing;
  - `BufferTooSmall`, returned by a receive whose buffer was too small for
    the packet. The packet is discarded, never truncated, and the device
    stays usable.
- ✅ **`PlatformErrorCode`**: a platform-tagged raw error code kept as a
  diagnostic escape hatch (`Error::Platform`): `Linux(i32)`,
  `Windows(u32)`, `Darwin(i32)`, or `Unknown` when the native failure
  carried no OS code or the target has no tag. It is `#[non_exhaustive]`,
  so a `match` on it needs a wildcard arm.
- ✅ **`Id<T>`**: a phantom-typed identifier generic over the domain object
  it names, so `Id<Device>` and `Id<Queue>` are distinct types at compile
  time.

## 📦 Installation

```toml
[dependencies]
tunnel-lattice-core = "0.4"
```

## 🎓 Quick Start

```rust
use tunnel_lattice_core::{Error, Id};

struct Device;
let id = Id::<Device>::new(7);
assert_eq!(id.value(), 7);

let err = Error::NotFound;
assert!(err.is_not_found());
```

## 📖 Documentation

- **API reference**: [docs.rs/tunnel-lattice-core](https://docs.rs/tunnel-lattice-core)
- **Project**: [github.com/F000NKKK/tunnel-lattice](https://github.com/F000NKKK/tunnel-lattice)

## 📄 License

Licensed under the [Mozilla Public License 2.0](https://github.com/F000NKKK/tunnel-lattice/blob/main/LICENSE).
