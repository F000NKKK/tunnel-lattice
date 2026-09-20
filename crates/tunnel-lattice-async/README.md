# tunnel-lattice-async

Runtime-agnostic `futures::Stream` adapter over a synchronous
`tunnel_lattice_platform::PacketIo` device, for backends with no native
async path. No Tokio, async-std, or smol dependency is imposed by this
crate itself.

## What it provides

- `from_device(Arc<D>, mtu) -> PacketStream`, bridging a blocking
  `PacketIo::recv` loop (run on one dedicated worker thread) onto a
  `futures::Stream<Item = Result<Vec<u8>>>`.

## When to use this vs. a backend's native async path

Prefer a backend's own `tunnel_lattice_platform::AsyncPacketIo`
implementation (checked via `Capability::NATIVE_ASYNC`) when available —
`tunnel-lattice-backend-tunrs` implements it directly on top of
`tun_rs::AsyncDevice`. Reach for this crate only for a backend that has no
native async I/O path at all.

## Known limitation

`PacketStream`'s worker thread cannot be forcibly cancelled: dropping the
stream cannot interrupt a thread parked inside a blocking `recv` call with
no further packets. See the type's rustdoc for the exact behavior this
implies before relying on prompt shutdown.

## Usage

```rust,ignore
use std::sync::Arc;
use futures::StreamExt;
use tunnel_lattice_async::from_device;

let device = Arc::new(open_some_device()?);
let mut packets = from_device(device, 1500);
while let Some(packet) = packets.next().await {
    let packet = packet?;
    println!("{} bytes", packet.len());
}
```
