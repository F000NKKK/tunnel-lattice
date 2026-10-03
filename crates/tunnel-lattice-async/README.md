<div align="center">

# ⚡ tunnel-lattice-async

### Runtime-Agnostic, Zero-Copy Packet Streams for Tunnel Lattice

[![crates.io](https://img.shields.io/crates/v/tunnel-lattice-async.svg?cacheSeconds=86400)](https://crates.io/crates/tunnel-lattice-async)
[![docs.rs](https://img.shields.io/docsrs/tunnel-lattice-async?cacheSeconds=86400)](https://docs.rs/tunnel-lattice-async)
[![License: MPL 2.0](https://img.shields.io/badge/license-MPL--2.0-blue.svg)](https://github.com/F000NKKK/tunnel-lattice/blob/main/LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.99-lightgrey.svg)](https://github.com/F000NKKK/tunnel-lattice)

[Overview](#-overview) • [Installation](#-installation) • [Quick Start](#-quick-start)
• [Errors](#-stream-behaviour) • [Pool](#-packet-buffer-pool)

</div>

---

## 📖 Overview

`futures::Stream`s of received packets for
[Tunnel Lattice](https://github.com/F000NKKK/tunnel-lattice). Each packet
is received straight into a slot of a fixed-size buffer pool and handed out
as a small `PacketBuf` view, so the steady state allocates and copies
nothing at this layer. This crate imposes no Tokio, async-std, or smol
dependency.

> Most applications get this stream through the
> [`tunnel-lattice`](https://crates.io/crates/tunnel-lattice) facade's
> `Handle::packet_stream` (with its `tokio` or `async-io` feature). Depend on
> this crate directly when writing a backend or driving a
> `tunnel-lattice-platform` device without the facade.

Main surface, all producing the same `PacketStream`:

- `from_async_device(Arc<D>, buf_len)` — wraps a backend's native
  `AsyncPacketIo`: no worker thread; dropping the stream cancels the pending
  `recv`. **Prefer it whenever the backend reports
  `Capability::NATIVE_ASYNC`** (the tun-rs backend does with either async
  feature).
- `from_device(Arc<D>, buf_len)` — bridges a blocking `PacketIo::recv` on
  one dedicated worker thread; the fallback for a backend with no async
  path.
- `from_async_device_with_pool` / `from_device_with_pool` — the same over a
  `PacketPool` you built (pick the slot count, or share one pool). They
  cannot fail.

The `buf_len` constructors return `Err(Error::InvalidState)` only for a
rejected `buf_len` (zero, or too large for a slot), before touching the
device. `from_device*` panic if the OS cannot spawn the worker thread.

No Cargo features and no OS-specific code.

## 📦 Installation

```toml
[dependencies]
tunnel-lattice-async = "0.6"
# Used by the examples below
futures = "0.3"
tunnel-lattice-platform = { version = "0.6", features = ["async"] }
```

## 🎓 Quick Start

```rust
use std::sync::Arc;

use futures::StreamExt;
use tunnel_lattice_async::{Result, from_async_device};
use tunnel_lattice_platform::AsyncPacketIo;

async fn print_packets<D: AsyncPacketIo + Send + Sync + 'static>(
    device: Arc<D>,
    buf_len: usize, // e.g. `Device::recv_buffer_len()`
) -> Result<()> {
    let mut packets = from_async_device(device, buf_len)?;
    while let Some(packet) = packets.next().await {
        let packet = packet?; // a PacketBuf: derefs to [u8]
        println!("{} bytes", packet.len());
        // Dropping `packet` here returns its slot to the pool.
    }
    Ok(())
}
```

Each item is exactly one packet as `recv` frames it (one IP packet for TUN,
one Ethernet frame for TAP), never fragmented or coalesced. Pass
`recv_buffer_len()` as `buf_len`: the MTU for TUN, MTU + 18 for TAP.

## 🧭 Stream Behaviour

- **Oversize packets** are never truncated: the stream yields
  `Err(Error::BufferTooSmall)` and keeps receiving.
- **Every other error** is yielded once, then the stream ends. That includes
  `Disconnected` and recoverable errors such as `InvalidState` for a
  disabled interface; create a new stream after recovering. This keeps a
  failing device from becoming a busy loop.
- **Back-pressure**: a stream receives only while its pool has a free slot.
  Holding every slot pauses it (never an error) until an item is dropped.
  Streams sharing a pool share its slots, with no fairness guarantee.
- **`from_device` shutdown is best-effort**: its worker cannot be forcibly
  cancelled, so a worker blocked in `recv` exits only once the device
  unblocks it. `from_async_device` has no such limitation.
- `from_device`'s worker enters no async runtime. Do not bridge a device
  whose blocking `recv` needs one (the tun-rs backend built with `tokio`
  panics there); use `from_async_device` for it.

## 🧱 Packet Buffer Pool

`PacketPool` is a fixed-capacity pool of equal slots carved from one slab,
allocated once. `PacketBuf` (16 bytes on 64-bit targets) is an exclusive
view of one packet; it derefs to `[u8]`, supports `advance`, `truncate`, and
`to_vec`, and returns its slot when dropped.

- `PacketPool::new(slots, buf_len)` accepts non-zero sizes whose slab fits
  in `u32::MAX` bytes; `with_buf_len` picks up to `DEFAULT_SLOTS` (128)
  within `DEFAULT_MAX_BYTES` (4 MiB). Invalid sizes return `InvalidState`.
- Slots are `buf_len + 1` bytes rounded up to 64 (no shared cache lines).
- **Safety note**: slots are reused without re-zeroing, so this relies on
  backends writing every byte `recv` reports. A non-conforming backend
  could expose an earlier packet's bytes (not undefined behaviour); the
  tun-rs backend conforms. The pool's `unsafe` code is confined to one
  module, tested in CI under Miri.
- Claiming and releasing a slot is lock-free; the lock is taken only when
  a stream must wait for a slot.

```rust
use std::sync::Arc;
use tunnel_lattice_async::{PacketPool, PacketStream, Result, from_device_with_pool};
use tunnel_lattice_platform::PacketIo;

fn two_streams<D: PacketIo + Send + Sync + 'static>(
    a: Arc<D>,
    b: Arc<D>,
) -> Result<(PacketStream, PacketStream)> {
    let pool = PacketPool::new(32, 1500)?; // 32 slots of 1500 bytes, shared
    assert!(PacketPool::new(0, 1500).is_err());
    Ok((from_device_with_pool(a, pool.clone()), from_device_with_pool(b, pool)))
}
```

Allocation tests pin zero allocations per packet on both paths;
`cargo bench -p tunnel-lattice-async` (and `--bench pool`) measure them on
in-memory mock devices, no privileges needed.

## 📖 Documentation

- **API reference**: [docs.rs/tunnel-lattice-async](https://docs.rs/tunnel-lattice-async)
- **Async design and packet buffers**:
  [ARCHITECTURE.md](https://github.com/F000NKKK/tunnel-lattice/blob/main/ARCHITECTURE.md#async-design)
- **Facade**: [`tunnel-lattice`](https://crates.io/crates/tunnel-lattice)
- **Project**: [github.com/F000NKKK/tunnel-lattice](https://github.com/F000NKKK/tunnel-lattice)

## 📄 License

Licensed under the
[Mozilla Public License 2.0](https://github.com/F000NKKK/tunnel-lattice/blob/main/LICENSE).
