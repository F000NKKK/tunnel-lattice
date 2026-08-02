# Tunnel Lattice

Cross-platform Rust library for TUN/TAP tunnel interfaces, designed to compose with the rest of the Lattice networking stack.

## Status

**Bootstrap stage.** This repository currently contains repository workflow,
policies, and packaging scaffolding ported from
[net-lattice](https://github.com/F000NKKK/net-lattice), the first crate in the
Lattice networking ecosystem. No implementation code has landed yet and no
version has been published.

## The Lattice ecosystem

| Crate | Purpose |
| --- | --- |
| [net-lattice](https://github.com/F000NKKK/net-lattice) | OS networking inspection and configuration (routes, DNS, interfaces) |
| [tunnel-lattice](https://github.com/F000NKKK/tunnel-lattice) | TUN/TAP tunnel interfaces |
| [dns-lattice](https://github.com/F000NKKK/dns-lattice) | Programmable DNS control plane |
| [flow-lattice](https://github.com/F000NKKK/flow-lattice) | Policy compiler: rules -> platform-neutral network plans |
| [sdk-lattice](https://github.com/F000NKKK/sdk-lattice) | Application-facing SDK composing the crates above |

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Feedback on scope and API direction is
the most valuable contribution at this stage.

## License

Licensed under the [Mozilla Public License 2.0](LICENSE).
