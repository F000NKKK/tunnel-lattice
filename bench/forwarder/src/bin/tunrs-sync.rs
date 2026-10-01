//! Baseline: raw `tun_rs::SyncDevice`, blocking copy threads.
//!
//! `--threads N` copy threads per direction (default 1), each with its own
//! 64 KiB stack buffer, as tun-benchmark2's sync forwarders do.

#[cfg(target_os = "linux")]
fn main() {
    use std::sync::Arc;

    use tun_rs::{DeviceBuilder, SyncDevice};
    use tunnel_lattice_bench_forwarder::cli::{Args, fatal, ready};

    fn open(name: &str, ip: std::net::Ipv4Addr, mtu: u16) -> Arc<SyncDevice> {
        let device = DeviceBuilder::new()
            .name(name)
            .ipv4(ip, 24, None)
            .mtu(mtu)
            .build_sync()
            .unwrap_or_else(|error| fatal(&format!("open {name}"), error));
        Arc::new(device)
    }

    fn copy(src: &SyncDevice, dst: &SyncDevice, direction: &str) -> ! {
        let mut buf = [0u8; 65536];
        loop {
            let len = match src.recv(&mut buf) {
                Ok(len) => len,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => fatal(&format!("{direction} recv"), error),
            };
            if let Err(error) = dst.send(&buf[..len])
                && error.kind() != std::io::ErrorKind::Interrupted
            {
                fatal(&format!("{direction} send"), error);
            }
        }
    }

    let args = Args::from_env_or_exit();
    let dev1 = open(&args.iface1, args.ip1, args.mtu);
    let dev2 = open(&args.iface2, args.ip2, args.mtu);
    ready(&args);

    let mut threads = Vec::new();
    for _ in 0..args.threads.unwrap_or(1) {
        for (src, dst, direction) in [
            (Arc::clone(&dev1), Arc::clone(&dev2), "1->2"),
            (Arc::clone(&dev2), Arc::clone(&dev1), "2->1"),
        ] {
            threads.push(std::thread::spawn(move || copy(&src, &dst, direction)));
        }
    }
    for thread in threads {
        // `copy` never returns; a panic ends the process instead of
        // leaving one direction dead.
        if thread.join().is_err() {
            fatal("copy thread", "panicked");
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    tunnel_lattice_bench_forwarder::cli::linux_only("tunrs-sync")
}
