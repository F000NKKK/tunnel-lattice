<div align="center">

# ⚡ tunnel-lattice-async

### Runtime-Agnostic, Zero-Copy Packet Streams for Tunnel Lattice

[![crates.io](https://img.shields.io/crates/v/tunnel-lattice-async.svg?cacheSeconds=86400)](https://crates.io/crates/tunnel-lattice-async)
[![docs.rs](https://img.shields.io/docsrs/tunnel-lattice-async?cacheSeconds=86400)](https://docs.rs/tunnel-lattice-async)
[![License: MPL 2.0](https://img.shields.io/badge/license-MPL--2.0-blue.svg)](https://github.com/F000NKKK/tunnel-lattice/blob/main/LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.93-lightgrey.svg)](https://github.com/F000NKKK/tunnel-lattice)

[Overview](#-overview) • [Features](#-key-features) • [Installation](#-installation) • [Quick Start](#-quick-start) • [Errors](#-stream-errors) • [Pool](#-packet-buffer-pool) • [Performance](#-per-packet-cost-and-benchmarks)

</div>

---

## 📖 Overview

Two ways to get a `futures::Stream` of received packets for
[Tunnel Lattice](https://github.com/F000NKKK/tunnel-lattice), both producing
the same `PacketStream` type. Each packet is received straight into a slot
of a fixed-size buffer pool and handed out as a small `PacketBuf` view, so
the steady state allocates and copies nothing at this layer. No Tokio,
async-std, or smol dependency is imposed by this crate itself.

> Most applications get this stream through the
> [`tunnel-lattice`](https://crates.io/crates/tunnel-lattice) facade's
> `Handle::packet_stream` (with its `tokio` or `async-io` feature), which
> picks the right constructor automatically. Depend on this crate directly
> when writing a backend or driving a `tunnel-lattice-platform` device
> without the facade.

## 🌟 Key Features

- ✅ `from_async_device(Arc<D>, buf_len) -> Result<PacketStream>`, wrapping a
  backend's own `tunnel_lattice_platform::AsyncPacketIo` directly — no
  worker thread, no polling loop. Dropping the stream drops the in-flight
  `recv` future, which is genuine, immediate cancellation, the same way
  dropping any other future is. **Prefer this whenever the backend reports
  `Capability::NATIVE_ASYNC`** — `tunnel_lattice::Handle::packet_stream`
  already does this automatically, so most callers never call this
  directly.
- ✅ `from_device(Arc<D>, buf_len) -> Result<PacketStream>`, bridging a
  blocking `tunnel_lattice_platform::PacketIo::recv` loop (run on one
  dedicated worker thread) onto the stream instead — the fallback for a
  backend with no native async path at all. See
  [Known limitation](#-known-limitation-only-from_device) for what
  this variant cannot guarantee that `from_async_device` can.
- ✅ `from_async_device_with_pool(Arc<D>, PacketPool)` and
  `from_device_with_pool(Arc<D>, PacketPool)`, the same two streams over a
  `PacketPool` you built, for example to pick the slot count or to share
  one pool between several streams. They cannot fail.
- ✅ `PacketPool` and `PacketBuf`, described under
  [Packet buffer pool](#-packet-buffer-pool).

The two `buf_len` constructors build a pool with
`PacketPool::with_buf_len(buf_len)`. They return `Err(Error::InvalidState)`
if that rejects `buf_len` (zero, or too large for a pool slot), before
allocating anything and without touching the device; that is their only
error. `from_device` and `from_device_with_pool` panic if the OS cannot
spawn the worker thread, as `std::thread::spawn` does.

No Cargo features. The pool is plain `std` code with no OS-specific paths;
the crate depends only on `tunnel-lattice-core`, `tunnel-lattice-platform`
(with its `async` feature), and `futures`.

## 📦 Installation

```toml
[dependencies]
tunnel-lattice-async = "0.4"
# Used by the example below
futures = "0.3"
tunnel-lattice-platform = { version = "0.4", features = ["async"] }
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

## 📥 Items and Buffer Size

Each `Ok` item is a `PacketBuf` holding exactly one packet as the device's
`recv` frames it: one raw IP packet for a TUN device, or one full Ethernet
frame for a TAP device, never a fragment or several packets coalesced into
one. Its length is at most the pool's `buf_len`. A `PacketBuf` dereferences
to `[u8]`; call `to_vec()` when you need an owned copy. Dropping it returns
its slot to the pool.

`buf_len` is the per-packet receive buffer size. For a device opened
through the `tunnel-lattice` facade, pass
`handle.snapshot()?.recv_buffer_len()`: the MTU for TUN, MTU + 18 for TAP
(the Ethernet header and one 802.1Q tag).

## 🚦 Back-Pressure

A stream receives only while its pool has a free slot. Items you still hold
keep their slots, and so do items the `from_device` worker has received but
you have not polled yet. Once every slot is taken, the stream waits
(`Poll::Pending`) without calling `recv` until you drop an item; running
out of slots is never an error. A stream that is the only user of its pool
(the default) always makes progress once you drop an item. Streams sharing
one pool share its slots, so a consumer that holds many items can starve
the others; no fairness between them is guaranteed.

## 🧭 Stream Errors

Both variants handle errors the same way:

- A packet larger than `buf_len` is never truncated. It is discarded, the
  stream yields `Err(Error::BufferTooSmall)`, and it keeps receiving. A
  device that reports more bytes than `buf_len` (which a conforming backend
  never does) is treated the same way instead of being sliced out of
  bounds.
- Every other error is yielded once, and then the stream ends (polling it
  again keeps returning `None`). That includes `Err(Error::Disconnected)`
  (the device is gone for good) and errors the device can recover from,
  such as `Err(Error::InvalidState)` for an administratively disabled
  interface. After recovering, create a new stream (with the facade, call
  `Handle::packet_stream` again).

The stream never yields `InvalidState` for its own reasons: a rejected
`buf_len` is reported only by the constructor. With the tun-rs backend, an
`InvalidState` item means the device exists but is not passing packets
because it is down or disabled (a disabled Wintun interface, or a Windows
TAP adapter whose media applying `DesiredAdminState::Down` disconnected);
applying `DesiredAdminState::Up` recovers it unless the adapter was
disabled outside this API. Other backends define their own meaning.

Ending on the first such error keeps a device whose `recv` fails
immediately and repeatedly (for example after the interface was deleted or
disabled) from turning the stream into a busy loop of error items. The
`from_device` worker thread stops calling `recv` and exits as soon as it
has forwarded the error. A worker thread that panics also ends the stream.
Transient native conditions, such as a signal interrupting the read, are
retried inside a conforming backend's `recv` and never reach the stream.

## 🔀 When to Use Which

Every backend `tunnel-lattice` ships today (`tunnel-lattice-backend-tunrs`)
implements `AsyncPacketIo` whenever it's built with an async feature at
all, so `from_async_device` is the path actually taken in practice.
`from_device` exists for a hypothetical future backend that only ever
implements `PacketIo` — it needs no async story of its own to be usable
this way; implementing `PacketIo` alone is enough.

## 🚧 Known Limitation (only `from_device`)

`from_device`'s worker thread cannot be forcibly cancelled: dropping the
stream can only signal the thread to stop, so a worker parked inside a
blocking `recv` call with no further packets cannot be woken by this
adapter alone — it exits only once the underlying device itself unblocks
it. A worker that is waiting for a free slot or for room to queue an item
exits promptly. `from_async_device` has no such limitation: there is no
worker thread to leak in the first place. See `PacketStream`'s rustdoc for
the exact behavior this implies before relying on prompt shutdown from
`from_device`.

## 🧱 Packet Buffer Pool

`PacketPool` is a fixed-capacity pool of equally sized receive slots,
carved from one zeroed slab that is allocated once. `PacketBuf` is an
exclusive view of one packet inside a slot: the pool handle plus a 32-bit
offset and length, 16 bytes on 64-bit targets. It dereferences to `[u8]`,
supports `advance` (drop a prefix) and `truncate`, and returns its slot to
the pool when dropped. Only the streams hand out `PacketBuf`s; the pool has
no public acquire method.

- `PacketPool::new(slots, buf_len)` accepts any non-zero sizes whose slab
  fits in `u32::MAX` bytes (and in `isize::MAX` on 32-bit targets).
  `PacketPool::with_buf_len(buf_len)` picks up to `DEFAULT_SLOTS` (128)
  slots within `DEFAULT_MAX_BYTES` (4 MiB). Invalid sizes return
  `Error::InvalidState` before anything is allocated. A failed slab
  allocation aborts the process, as it does for a `Vec`.
- Each slot is `buf_len + 1` bytes rounded up to 64, so slots never share
  a cache line. For example, a 1500-byte `buf_len` gives 1536-byte slots.
- Slots are reused without being re-zeroed. `PacketBuf` only exposes the
  bytes the device's `recv` reported, so this relies on backends honoring
  `recv`'s contract: every reported byte was written. A backend that
  reports more bytes than it wrote could expose bytes of an earlier
  packet, possibly one received by another stream sharing the pool. That
  is not undefined behaviour, and the tun-rs backend conforms.
- The pool is plain `std` code with no feature flags or OS-specific paths.
  Its `unsafe` code is confined to one module, whose tests CI also runs
  under Miri.

```rust
use std::sync::Arc;
use tunnel_lattice_async::{PacketPool, PacketStream, Result, from_device_with_pool};
use tunnel_lattice_platform::PacketIo;

fn two_streams<D: PacketIo + Send + Sync + 'static>(
    a: Arc<D>,
    b: Arc<D>,
) -> Result<(PacketStream, PacketStream)> {
    // 32 slots of 1500 bytes, shared by both streams.
    let pool = PacketPool::new(32, 1500)?;
    assert_eq!(pool.available(), 32);
    assert!(PacketPool::new(0, 1500).is_err());
    Ok((
        from_device_with_pool(a, pool.clone()),
        from_device_with_pool(b, pool),
    ))
}
```

## 📊 Per-Packet Cost and Benchmarks

On both paths a steady stream makes no heap allocation and no copy per
packet at this layer: the device's `recv` writes straight into a pool slot.

- **Lock-free fast path.** Claiming and releasing a slot takes no lock:
  each slot has an atomic ownership flag, and the pool's reference count is
  paid in batches. A stream that owns its pool's only handle (the default
  for `from_async_device` and `from_device`) claims with a plain load and
  store; streams sharing a pool claim with one compare-and-swap. The pool
  lock is taken only when a stream has to wait for a free slot.
- **Thread bridge.** It adds a bounded `std::sync::mpsc::sync_channel` with
  one entry per slot, created once, and a wake of the consumer per packet.
- **Allocation tests.** The repository's `tests/alloc_count.rs` pins zero
  allocations per packet on both paths, including a native stream whose
  every receive has to wait for a slot, and bounds the memory a stream
  owns.
- **Benchmarks.** `cargo bench -p tunnel-lattice-async` measures both paths
  against a synchronous caller-buffer loop and a frozen copy of the earlier
  `Vec`-per-packet streams, using in-memory mock devices (no privileges or
  real device needed). `cargo bench -p tunnel-lattice-async --bench pool`
  measures `PacketPool` construction and first use, the pool's own
  per-packet overhead, and contention on a shared pool.

## 📖 Documentation

- **API reference**: [docs.rs/tunnel-lattice-async](https://docs.rs/tunnel-lattice-async)
- **Facade**: [`tunnel-lattice`](https://crates.io/crates/tunnel-lattice), whose `Handle::packet_stream` builds these streams
- **Project**: [github.com/F000NKKK/tunnel-lattice](https://github.com/F000NKKK/tunnel-lattice)

## 📄 License

Licensed under the [Mozilla Public License 2.0](https://github.com/F000NKKK/tunnel-lattice/blob/main/LICENSE).
