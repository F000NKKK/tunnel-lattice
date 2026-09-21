# tunnel-lattice

Cross-platform Rust library for TUN/TAP tunnel interfaces, designed to
compose with the rest of the Lattice networking stack. This is the
application-facing Tunnel Lattice crate.

## What it provides

- `Tunnel::connect()`, a stateless connection to the default `tun-rs`-backed
  backend (the `tun-rs` feature, enabled by default);
- `Tunnel::open(DeviceConfig)`, creating a new TUN or TAP device and
  returning a `Handle` to it;
- `Handle::recv`/`send`, blocking packet transfer on an open device;
- `Handle::snapshot`, re-reading a device's current name/MTU/administrative
  state;
- `Handle::apply(DeviceConfigPatch)`, changing an open device's MTU or
  administrative state;
- `Handle::persist`/`Handle::additional_queue`, on backends that implement
  `PersistentDevice`/`MultiQueueProvider` (Linux only, via
  `tunnel-lattice-backend-tunrs` — see "Persistent devices and multi-queue"
  below);
- with the `async-io` or `tokio` feature (mutually exclusive):
  `Handle::packet_stream`, a `futures::Stream` of received packets.

This crate does not assign IP addresses to the interfaces it creates — see
`net-lattice` in the sibling Lattice ecosystem for OS network configuration
once a device exists.

## Ownership: `Handle` is `Clone`

`Handle<D>` wraps its device in an `Arc` and can be cloned cheaply to share
one open device across threads — `recv`/`send` take `&self`, so concurrent
calls through separate clones are always safe. The device stays open until
every `Handle` clone (and any `PacketStream` derived from one) has been
dropped; there is no explicit close method. See `ARCHITECTURE.md`'s
"Ownership and concurrency contract" for the full write-up, including how
this differs from `additional_queue`'s independent hardware queue.

## Quick start

```rust,no_run
use tunnel_lattice::{DeviceConfig, DeviceKind, Result, Tunnel};

fn main() -> Result<()> {
    let tunnel = Tunnel::connect();
    let device = tunnel.open(DeviceConfig::new(DeviceKind::Tun).with_mtu(1500))?;
    let mut buf = vec![0u8; 1500];
    let len = device.recv(&mut buf)?;
    println!("{len} bytes: {:?}", &buf[..len]);
    Ok(())
}
```

## Feature flags

- `tun-rs` (default) — selects `tunnel-lattice-backend-tunrs`. A Cargo
  feature, not a `target_os` cfg gate, so a future non-`tun-rs` backend can
  be added alongside it rather than replacing it — see the workspace
  `ARCHITECTURE.md`, "Backend replacement plan."
- `async-io` / `tokio` (mutually exclusive; enabling both is a compile
  error from `tun-rs`) — either adds `Handle::packet_stream`, a
  `futures::Stream` of received packets. `async-io` selects `tun-rs`'s
  `async-io`/`blocking`-based backend (no tokio dependency); `tokio` selects
  its tokio-based one. Uses a backend's native async path when it reports
  `Capability::NATIVE_ASYNC` (as `tunnel-lattice-backend-tunrs` does with
  either feature); otherwise falls back to `tunnel-lattice-async`'s
  thread-based adapter. No async runtime dependency is imposed when neither
  feature is enabled.

  **With `tokio`, `Handle::recv`/`send`/`snapshot`/`apply` all require a
  multi-threaded Tokio runtime entered on the calling thread**
  (`#[tokio::main]`'s default flavor, or `Builder::new_multi_thread()`) —
  not only `packet_stream`. `tunnel-lattice-backend-tunrs`'s `PacketIo`
  drives tun-rs's Tokio-backed handle through
  `tokio::runtime::Handle::current().block_on`, which only polls that
  runtime's I/O driver on the `multi_thread` flavor; on `current_thread` the
  first `recv`/`send` call hangs forever. See that crate's README, "`tokio`
  requires a multi-threaded runtime," for why. Prefer `async-io` if a
  single-threaded runtime is a hard requirement.

## Persistent devices and multi-queue

Both Linux-only, gated by `Capability::PERSISTENT_DEVICES`/
`Capability::MULTI_QUEUE`:

```rust,no_run
# #[cfg(target_os = "linux")]
# fn main() -> tunnel_lattice::Result<()> {
use tunnel_lattice::{DeviceConfig, DeviceKind, Tunnel};

let tunnel = Tunnel::connect();
let device = tunnel.open(DeviceConfig::new(DeviceKind::Tun).with_multi_queue(true))?;
device.persist()?; // survives this process exiting
let second_queue = device.additional_queue()?; // independent hardware queue
# Ok(())
# }
# #[cfg(not(target_os = "linux"))]
# fn main() {}
```

`additional_queue` on a device not opened with `with_multi_queue(true)`
returns `Error::Unsupported`. Multi-queue is a throughput optimization, not
a requirement for sharing a device across threads — see "Ownership" above.

## Platform and privilege notes

Creating a TUN/TAP device generally requires `CAP_NET_ADMIN` on Linux,
Administrator on Windows, or root on macOS/BSD. Runtime `Capability` flags
describe implemented surfaces, not a guarantee the current process is
authorized.

On Windows, `TunRsBackend::open` for a TUN device also requires
`wintun.dll` to be present next to your application's executable or on
`PATH` — `tun-rs` loads it at runtime rather than linking it at build time,
and does not vendor it. Without it, `open` fails with a generic
`Error::Platform` carrying no OS error code, not an obviously-named error.
Download it from [wintun.net](https://www.wintun.net/) and ship it with your
application; see `tunnel-lattice-backend-tunrs`'s README for details.
