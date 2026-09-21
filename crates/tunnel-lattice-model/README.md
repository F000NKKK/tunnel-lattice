# tunnel-lattice-model

Operating-system-independent TUN/TAP domain types for Tunnel Lattice. This
crate models data and contracts; it never inspects or mutates the host
system.

## What it provides

- `DeviceKind`, distinguishing TUN (raw IP) from TAP (Ethernet-framed)
  devices;
- `DeviceConfig`, desired intent for creating a new device (kind, an
  advisory name, an optional MTU, a multi-queue request) via
  `DeviceConfig::new`/`with_name`/`with_mtu`/`with_multi_queue`;
- `Device`, an observed, already-open device (id, actual name, kind, MTU,
  administrative state);
- `DeviceConfigPatch`, desired intent for changing an open device's MTU or
  administrative state, distinct from creation — built with
  `DeviceConfigPatch::new`, which rejects an empty or zero-MTU patch the
  same way `net-lattice-model::InterfaceConfig::new` does;
- `AdminState`/`DesiredAdminState`, split the same way
  `net-lattice-model::interface`'s pair is: observed `Unknown` is never
  requested back.

This crate deliberately does not model packet I/O (that is a runtime
concern of `tunnel-lattice-platform`'s provider traits) or IP address
assignment on the resulting device (that belongs to `net-lattice`, not this
crate — Tunnel Lattice creates and configures the virtual interface only).

## Usage

```rust
use tunnel_lattice_model::{DeviceConfig, DeviceConfigPatch, DeviceId, DeviceKind, DesiredAdminState};

let config = DeviceConfig::new(DeviceKind::Tun)
    .with_name("tun0")
    .with_mtu(1500)
    .with_multi_queue(true);
assert_eq!(config.kind, DeviceKind::Tun);
assert!(config.multi_queue);

let patch = DeviceConfigPatch::new(DeviceId::new(1), Some(DesiredAdminState::Up), None)?;
assert_eq!(patch.admin_state(), Some(DesiredAdminState::Up));
# Ok::<(), tunnel_lattice_core::Error>(())
```

Use this crate directly for offline configuration construction or backend
development. Use the `tunnel-lattice` facade to connect these types to an
operating system.
