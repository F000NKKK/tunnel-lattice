# Research and tool-use rules

- Prefer `Grep`/`Glob` for repository search and targeted reads for context.
- Use `cargo metadata --no-deps --format-version 1` to inspect package
  relationships and `cargo package -p <crate> --allow-dirty --list` to verify
  published file contents.
- Use `git diff`, `git log`, and `git status` as evidence, not as a
  substitute for reading source and tests.
- Use `mcp__youtrack__search_issues`/`mcp__youtrack__get_issue` to check
  prior evidence recorded on the active Epic/Story/Task before re-deriving
  it; see `@.claude/rules/youtrack.md`.
- Test/inspect Linux, Windows, and macOS behavior separately before claiming
  platform parity, even while there is only one backend crate
  (`tunnel-lattice-backend-tunrs`) — `tun-rs` still behaves differently per
  OS internally, and a future native per-OS backend will split this crate
  the way `net-lattice-backend-linux`/`-windows`/`-darwin` are split today
  (see `ARCHITECTURE.md`, "Backend replacement plan").
- Separate compile-time provider contracts, runtime capabilities, native
  privilege requirements, and async I/O availability in findings.
- Cite exact paths and symbols in audit reports; avoid unsupported
  assumptions.
- Prefer repository and primary-source evidence. If a current external fact
  is material, use the appropriate authoritative source and record its URL
  and access date in the audit rather than guessing.
