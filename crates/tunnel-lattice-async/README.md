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
