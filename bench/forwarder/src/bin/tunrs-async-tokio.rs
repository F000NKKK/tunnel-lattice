//! Baseline: raw `tun_rs::AsyncDevice` on Tokio, one task per direction.
//!
//! `--threads 1` runs a `current_thread` runtime, `--threads N` a
//! multi-thread runtime with `N` workers, and no `--threads` Tokio's
//! default multi-thread runtime, as in tun-benchmark2.

#[cfg(target_os = "linux")]
fn main() {
    use tunnel_lattice_bench_forwarder::cli::{Args, fatal, ready};
    use tunnel_lattice_bench_forwarder::forward::{open_tunrs, tokio_runtime, tunrs};

    let args = Args::from_env_or_exit();
    tokio_runtime(args.threads).block_on(async {
        let (dev1, dev2) = open_tunrs(&args);
        ready(&args);
        let one = tokio::spawn(tunrs(dev1.clone(), dev2.clone(), "1->2"));
        let two = tokio::spawn(tunrs(dev2, dev1, "2->1"));
        // Both loops only end by exiting the process; a join error means
        // a task panicked.
        let (one, two) = (one.await, two.await);
        if let Err(error) = one.and(two) {
            fatal("copy task", error);
        }
    });
}

#[cfg(not(target_os = "linux"))]
fn main() {
    tunnel_lattice_bench_forwarder::cli::linux_only("tunrs-async-tokio")
}
