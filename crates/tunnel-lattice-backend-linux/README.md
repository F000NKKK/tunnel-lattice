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

> **Status: in development.** This version contains a working synchronous
> device core, but it is internal: the crate exports no public API yet, so
> it cannot be used to open a device from outside. For a working backend
> today use the
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
- **Opening devices.** TUN and TAP devices are created (or attached to,
  for a persistent or multi-queue device) through `TUNSETIFF`, always
  without the packet-information header. The descriptor is non-blocking
  and close-on-exec from creation. The interface index is captured at open
  and is the device's identity; a requested MTU and MAC address are applied
  right after, and any failure closes the descriptor again. A device that
  uses virtio-net header framing is refused as `Unsupported`, and the
  `offload` request is accepted but ignored.
- **Packet I/O.** `recv` reads with `readv` into the caller's buffer plus
  a one-byte sentinel, so an oversize packet is reported as
  `BufferTooSmall` instead of being silently truncated; `send` writes first.
  Both wait with `poll` only when the queue is empty or full. A deleted
  device ends a waiting receive with `Disconnected`; sending to a device
  that is administratively down is `InvalidState`.
- **Observing and changing a device** over rtnetlink by the captured
  index: `snapshot` (name, kind, MTU, administrative state, and the MAC of
  a TAP device) and `apply` of MTU, then MAC, then administrative state,
  with a best-effort reverse revert of the steps already applied when a
  later one fails.
- **Persistence and multi-queue.** `TUNSETPERSIST` is passed by value;
  additional queues attach to a multi-queue device by its current name and
  share its control socket.
- **Capabilities** match `tunnel-lattice-backend-tunrs` on Linux: device
  mutation, persistent devices, TAP devices, and multi-queue on every host,
  plus MAC mutation on a TAP handle. There is no segmentation offload and
  no native async I/O yet.

Known limitation: Linux reports `IFF_NO_PI` and "no socket filter" with the
same flag bit, so the read-back cannot confirm that the packet-information
header is off; it is always requested.

## 💻 Platforms

Linux only. On every other target the crate compiles to an **empty
crate** and pulls in no dependency beyond `tunnel-lattice-core`,
`tunnel-lattice-model`, and `tunnel-lattice-platform`, so a
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

Creating a TUN/TAP device needs `CAP_NET_ADMIN` (attaching to a
persistent device owned by the calling user or group does not), and
changing a link (MTU, MAC, administrative state) always does; without it
both are refused with `Error::PermissionDenied`. Looking a link up over
rtnetlink, packet I/O on an open queue, and toggling persistence through an
open queue need nothing more. `/dev/net/tun` must exist: a missing node or
`tun` module is reported as `Error::DriverUnavailable`, and the backend
never creates the node (no `mknod`).

## 📖 Documentation

- **API reference**: [docs.rs/tunnel-lattice-backend-linux](https://docs.rs/tunnel-lattice-backend-linux)
- **Design and backend replacement plan**:
  [ARCHITECTURE.md](https://github.com/F000NKKK/tunnel-lattice/blob/main/ARCHITECTURE.md)
- **Facade**: [`tunnel-lattice`](https://crates.io/crates/tunnel-lattice)

## 📄 License

Licensed under the
[Mozilla Public License 2.0](https://github.com/F000NKKK/tunnel-lattice/blob/main/LICENSE).
