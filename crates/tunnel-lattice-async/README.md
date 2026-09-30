# tunnel-lattice-async

Two ways to get a `futures::Stream` of received packets, both producing the
same `PacketStream` type. No Tokio, async-std, or smol dependency is
imposed by this crate itself.

## What it provides

- `from_async_device(Arc<D>, mtu) -> PacketStream`, wrapping a backend's own
  `tunnel_lattice_platform::AsyncPacketIo` directly — no worker thread, no
  polling loop. Dropping the stream drops the in-flight `recv` future,
  which is genuine, immediate cancellation, the same way dropping any other
  future is. **Prefer this whenever the backend reports
  `Capability::NATIVE_ASYNC`** — `tunnel_lattice::Handle::packet_stream`
  already does this automatically, so most callers never call this
  directly.
- `from_device(Arc<D>, mtu) -> PacketStream`, bridging a blocking
  `tunnel_lattice_platform::PacketIo::recv` loop (run on one dedicated
  worker thread) onto the stream instead — the fallback for a backend with
  no native async path at all. See "Known limitation" below for what this
  variant cannot guarantee that `from_async_device` can.

## When to use which

Every backend `tunnel-lattice` ships today (`tunnel-lattice-backend-tunrs`)
implements `AsyncPacketIo` whenever it's built with an async feature at
all, so `from_async_device` is the path actually taken in practice.
`from_device` exists for a hypothetical future backend that only ever
implements `PacketIo` — it needs no async story of its own to be usable
this way; implementing `PacketIo` alone is enough.

## Known limitation (only `from_device`)

`from_device`'s worker thread cannot be forcibly cancelled: dropping the
stream can only set a flag the thread checks *between* `recv` calls, so a
worker parked inside a blocking `recv` call with no further packets cannot
be woken by this adapter alone — it exits only once the underlying device
itself unblocks it. `from_async_device` has no such limitation: there is no
worker thread to leak in the first place. See `PacketStream`'s rustdoc for
the exact behavior this implies before relying on prompt shutdown from
`from_device`.

## Usage

```rust,ignore
use std::sync::Arc;
use futures::StreamExt;
use tunnel_lattice_async::from_async_device;

let device = Arc::new(open_some_async_device()?);
let mut packets = from_async_device(device, 1500);
while let Some(packet) = packets.next().await {
    let packet = packet?;
    println!("{} bytes", packet.len());
}
```

## Packet buffer pool

`PacketPool` is a fixed-capacity pool of equally sized receive slots,
carved from one zeroed slab that is allocated once. `PacketBuf` is an
exclusive view of one packet inside a slot: the pool handle plus a 32-bit
offset and length, 16 bytes on 64-bit targets. It dereferences to `[u8]`,
supports `advance` (drop a prefix) and `truncate`, and returns its slot to
the pool when dropped.

- `PacketPool::new(slots, buf_len)` accepts any non-zero sizes whose slab
  fits in `u32::MAX` bytes (and in `isize::MAX` on 32-bit targets).
  `PacketPool::with_buf_len(buf_len)` picks up to `DEFAULT_SLOTS` (128)
  slots within `DEFAULT_MAX_BYTES` (4 MiB).
  Invalid sizes return `Error::InvalidState` before anything is allocated.
  A failed slab allocation aborts the process, as it does for a `Vec`.
- Each slot is `buf_len + 1` bytes rounded up to 64, so slots never share
  a cache line. For example, a 1500-byte `buf_len` gives 1536-byte slots.
- Slots are reused without being re-zeroed. `PacketBuf` only exposes the
  bytes the device's `recv` reported, so this relies on backends honoring
  `recv`'s contract: every reported byte was written.
- The pool is plain `std` code with no feature flags or OS-specific paths.
  Its `unsafe` code is confined to one module, whose tests CI also runs
  under Miri.

The stream adapters do not use the pool yet: `PacketStream` still yields
`Vec<u8>` items, and the pool has no public acquire method.

```rust
use tunnel_lattice_async::{PacketPool, Result};

fn make_pool() -> Result<PacketPool> {
    let pool = PacketPool::with_buf_len(1500)?;
    assert_eq!(pool.slots(), 128);
    assert_eq!(pool.available(), 128);
    assert!(PacketPool::new(0, 1500).is_err());
    Ok(pool)
}
```

## Per-packet cost and benchmarks

Today both paths yield an owned `Vec<u8>` per packet: the native path
allocates one zeroed `mtu`-byte buffer per `recv`, and the thread bridge
copies each packet into a new `Vec` and an unbounded-channel node (two
allocations per packet). The repository's `tests/alloc_count.rs` pins these
counts, and `cargo bench -p tunnel-lattice-async` measures both paths
against a synchronous caller-buffer loop, using in-memory mock devices (no
privileges or real device needed). `cargo bench -p tunnel-lattice-async
--bench pool` measures `PacketPool` construction for full-MTU, jumbo and
maximum-size buffers.
