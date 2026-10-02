<div align="center">

# 🐧 tunnel-lattice-backend-linux

### The Native Linux TUN/TAP Backend for Tunnel Lattice

[![crates.io](https://img.shields.io/crates/v/tunnel-lattice-backend-linux.svg?cacheSeconds=86400)](https://crates.io/crates/tunnel-lattice-backend-linux)
[![docs.rs](https://img.shields.io/docsrs/tunnel-lattice-backend-linux?cacheSeconds=86400)](https://docs.rs/tunnel-lattice-backend-linux)
[![License: MPL 2.0](https://img.shields.io/badge/license-MPL--2.0-blue.svg)](https://github.com/F000NKKK/tunnel-lattice/blob/main/LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.93-lightgrey.svg)](https://github.com/F000NKKK/tunnel-lattice)

![Linux](https://img.shields.io/badge/Linux-in%20development-yellow)

</div>

---

## 📖 Overview

A native Linux backend for
[Tunnel Lattice](https://github.com/F000NKKK/tunnel-lattice), built
directly on `/dev/net/tun` and a synchronous rtnetlink socket, with no
`tun-rs` underneath and no async runtime on the control path. It is meant
to implement the same `tunnel-lattice-platform` provider contracts as
[`tunnel-lattice-backend-tunrs`](https://crates.io/crates/tunnel-lattice-backend-tunrs)
and to be selectable alongside it.

> **Status: in development.** This version contains the internal
> foundations only and exports no public API: it cannot open a device yet.
> For a working backend today use the
> [`tunnel-lattice`](https://crates.io/crates/tunnel-lattice) facade, whose
> default backend is `tunnel-lattice-backend-tunrs`.

What is implemented so far (all internal):

- **Error mapping** from the raw `errno` and the operation that produced
  it, never from `std::io::ErrorKind`. A missing `/dev/net/tun` node or
  `tun` module is reported as `DriverUnavailable`; the backend never
  creates the device node itself.
- **A synchronous rtnetlink control plane** for link lookups and changes
  (MTU, MAC, administrative state): one blocking socket, an acknowledged
  request per change, sequence numbers checked, and kernel errors decoded
  to typed `tunnel_lattice_core::Error` values. It uses no async runtime,
  so it is safe to call from any thread, inside or outside one.

## 💻 Platforms

Linux only. On every other target the crate compiles to an **empty
crate** and pulls in no dependency beyond `tunnel-lattice-core`, so a
cross-platform workspace can depend on it unconditionally.

## 📦 Installation

```toml
[dependencies]
tunnel-lattice-backend-linux = "0.1"
```

There are no Cargo features yet. On Linux it depends on `libc` and the
`netlink-packet-route`/`netlink-packet-core`/`netlink-sys` crates with
default features off (no tokio, mio, or smol).

## 🔐 Privileges

Nothing in this version needs privilege by itself: looking a link up over
rtnetlink is unprivileged. Changing a link (MTU, MAC, administrative state)
requires `CAP_NET_ADMIN` and is otherwise refused with
`Error::PermissionDenied`.

## 📖 Documentation

- **API reference**: [docs.rs/tunnel-lattice-backend-linux](https://docs.rs/tunnel-lattice-backend-linux)
- **Design and backend replacement plan**:
  [ARCHITECTURE.md](https://github.com/F000NKKK/tunnel-lattice/blob/main/ARCHITECTURE.md)
- **Facade**: [`tunnel-lattice`](https://crates.io/crates/tunnel-lattice)

## 📄 License

Licensed under the
[Mozilla Public License 2.0](https://github.com/F000NKKK/tunnel-lattice/blob/main/LICENSE).
