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
- with the `async-io` or `tokio` feature (mutually exclusive):
  `Handle::packet_stream`, a `futures::Stream` of received packets.

This crate does not assign IP addresses to the interfaces it creates — see
`net-lattice` in the sibling Lattice ecosystem for OS network configuration
once a device exists.

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

## Platform and privilege notes

Creating a TUN/TAP device generally requires `CAP_NET_ADMIN` on Linux,
Administrator on Windows, or root on macOS/BSD. Runtime `Capability` flags
describe implemented surfaces, not a guarantee the current process is
authorized.
