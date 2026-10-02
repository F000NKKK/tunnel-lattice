//! Shared code for the forwarder benchmark binaries.
//!
//! - [`cli`]: the one command line every forwarder binary accepts, and the
//!   small process helpers they share (readiness line, fatal exit).
//! - `forward` (Linux, `tokio` or `async-io` builds): the async forwarding
//!   loops, raw tun-rs and the tunnel-lattice facade, plain and `--offload`.
//! - `tunrs_offload` (Linux, any build set): the preallocated buffers of the
//!   raw tun-rs `--offload` loops.
//! - [`report`]: turns a run directory written by
//!   `scripts/bench-forward.sh run` into `results.json` and a Markdown
//!   table.
//!
//! Exactly one build set (`sync`, `tokio` or `async-io`) may be enabled per
//! build: the facade's blocking path differs once an async feature is on,
//! and tun-rs refuses two async runtimes in one build.

#[cfg(any(
    all(feature = "sync", feature = "tokio"),
    all(feature = "sync", feature = "async-io"),
    all(feature = "tokio", feature = "async-io"),
))]
compile_error!(
    "enable exactly one of the `sync`, `tokio` and `async-io` features per build \
     (scripts/bench-forward.sh builds each set separately)"
);

pub mod cli;
#[cfg(all(target_os = "linux", any(feature = "tokio", feature = "async-io")))]
pub mod forward;
pub mod report;
#[cfg(all(
    target_os = "linux",
    any(feature = "sync", feature = "tokio", feature = "async-io")
))]
pub mod tunrs_offload;
