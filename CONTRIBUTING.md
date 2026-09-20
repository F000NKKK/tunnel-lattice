# Contributing to Tunnel Lattice

Thank you for your interest in contributing to Tunnel Lattice.

## Project Status

Tunnel Lattice has a working crate architecture (see
[ARCHITECTURE.md](ARCHITECTURE.md)) but no published release yet — nothing in
the public API is frozen. The most valuable contributions right now are:

- Feedback on the crate architecture and API shape (see
  [ARCHITECTURE.md](ARCHITECTURE.md))
- Verifying `tunnel-lattice-backend-tunrs`'s `tun-rs` usage on Windows and
  macOS (developed and CI-checked so far without a native build on every
  platform — see that crate's README)
- Documentation and tooling improvements

Please check open issues and discussions before starting significant work, to
avoid duplicated effort.

## Getting Started

1. Fork the repository and clone your fork.
2. Create a topic branch for your change.
3. Make your changes, following the conventions described below.
4. Open a pull request against `main` using the provided pull request template.

## Development Conventions

Tunnel Lattice follows standard Rust ecosystem conventions:

- Code must be formatted with `rustfmt`.
- Code must be free of `clippy` warnings.
- Public APIs must be documented.
- Changes must include appropriate tests.
- Every affected crate must retain a standalone crate-local README, and
  English/Russian project documentation must remain synchronized.
- Privileged network tests must be isolated, opt-in, and restore changed state.
- Commit messages should be clear and descriptive.

## Reporting Issues

Please use the issue templates under `.github/ISSUE_TEMPLATE/` when filing
bug reports or feature requests. Include as much context as possible.

## Security Issues

Do not report security vulnerabilities through public GitHub issues. See
[SECURITY.md](SECURITY.md) for the responsible disclosure process.

## Code of Conduct

By participating in this project, you agree to abide by the
[Code of Conduct](CODE_OF_CONDUCT.md).

## License

By contributing to Tunnel Lattice, you agree that your contributions will be licensed
under the [Mozilla Public License 2.0](LICENSE).
