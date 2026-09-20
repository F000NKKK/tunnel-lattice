# tunnel-lattice-core

Foundational error, ID, and result types shared across the Tunnel Lattice
workspace. No OS dependency, no TUN/TAP-specific types.

## What it provides

- `Error`, the single `#[non_exhaustive]` error type every provider trait
  method returns instead of a raw OS error (`std::io::Error`, a bare
  `errno`, a Windows `DWORD`);
- `PlatformErrorCode`, a platform-tagged raw error code preserved as a
  diagnostic escape hatch (`Error::Platform`);
- `Id<T>`, a phantom-typed identifier generic over the domain object it
  names, so `Id<Device>` and `Id<Queue>` are distinct types at compile time.

## Usage

```rust
use tunnel_lattice_core::{Error, Id};

struct Device;
let id = Id::<Device>::new(7);
assert_eq!(id.value(), 7);

let err = Error::NotFound;
assert!(err.is_not_found());
```

Use this crate directly only when building a new backend or an independent
tool against Tunnel Lattice's error/ID shape. Application code normally
depends on the `tunnel-lattice` facade instead.
