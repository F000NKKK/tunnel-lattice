//! Baseline: raw `tun_rs::AsyncDevice` on async-io, one thread per
//! direction, each running `async_io::block_on`.
//!
//! tun-benchmark2 has no async-io forwarder; this gives the async-io
//! tunnel-lattice row a baseline on the same runtime. `--threads` is
//! ignored.
//!
//! With `--offload`, both devices are built with `offload(true)` and each
//! thread uses `recv_multiple`/`send_multiple` with its own preallocated
//! batch buffers and `GROTable`.

#[cfg(target_os = "linux")]
fn main() {
    use tunnel_lattice_bench_forwarder::cli::{Args, fatal, ready};
    use tunnel_lattice_bench_forwarder::forward::{open_tunrs, tunrs};

    let args = Args::from_env_or_exit();
    let (dev1, dev2) = open_tunrs(&args);
    ready(&args);
    let threads = [(dev1.clone(), dev2.clone(), "1->2"), (dev2, dev1, "2->1")].map(
        |(src, dst, direction)| {
            let offload = args.offload;
            std::thread::spawn(move || async_io::block_on(tunrs(src, dst, direction, offload)))
        },
    );
    for thread in threads {
        if thread.join().is_err() {
            fatal("copy thread", "panicked");
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    tunnel_lattice_bench_forwarder::cli::linux_only("tunrs-async-asyncio")
}
