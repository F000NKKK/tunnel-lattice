//! Baseline: raw `tun_rs::SyncDevice`, blocking copy threads.
//!
//! `--threads N` copy threads per direction (default 1), each with its own
//! 64 KiB stack buffer, as tun-benchmark2's sync forwarders do.
//!
//! With `--offload`, both devices are built with `offload(true)` and each
//! copy thread uses `recv_multiple`/`send_multiple` with its own
//! preallocated batch buffers and `GROTable`.

#[cfg(target_os = "linux")]
fn main() {
    use std::sync::Arc;

    use tun_rs::{DeviceBuilder, SyncDevice};
    use tunnel_lattice_bench_forwarder::cli::{Args, fatal, ready};
    use tunnel_lattice_bench_forwarder::tunrs_offload::TunrsBatch;

    fn open(name: &str, ip: std::net::Ipv4Addr, mtu: u16, offload: bool) -> Arc<SyncDevice> {
        let device = DeviceBuilder::new()
            .name(name)
            .ipv4(ip, 24, None)
            .mtu(mtu)
            .offload(offload)
            .build_sync()
            .unwrap_or_else(|error| fatal(&format!("open {name}"), error));
        if offload {
            if !device.tcp_gso() {
                fatal(&format!("open {name}"), "offload requested but not enabled");
            }
            eprintln!("{name}: offload on, UDP GSO {}", device.udp_gso());
        }
        Arc::new(device)
    }

    fn copy_offload(src: &SyncDevice, dst: &SyncDevice, mtu: u16, direction: &str) -> ! {
        let mut batch = TunrsBatch::new(mtu);
        loop {
            let count = match src.recv_multiple(
                &mut batch.original,
                &mut batch.bufs,
                &mut batch.sizes,
                TunrsBatch::OFFSET,
            ) {
                Ok(count) => count,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => fatal(&format!("{direction} recv_multiple"), error),
            };
            batch.trim(count);
            if let Err(error) =
                dst.send_multiple(&mut batch.gro, &mut batch.bufs[..count], TunrsBatch::OFFSET)
                && error.kind() != std::io::ErrorKind::Interrupted
            {
                fatal(&format!("{direction} send_multiple"), error);
            }
            batch.restore(count);
        }
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
    let dev1 = open(&args.iface1, args.ip1, args.mtu, args.offload);
    let dev2 = open(&args.iface2, args.ip2, args.mtu, args.offload);
    let (mtu, offload) = (args.mtu, args.offload);
    ready(&args);

    let mut threads = Vec::new();
    for _ in 0..args.threads.unwrap_or(1) {
        for (src, dst, direction) in [
            (Arc::clone(&dev1), Arc::clone(&dev2), "1->2"),
            (Arc::clone(&dev2), Arc::clone(&dev1), "2->1"),
        ] {
            threads.push(std::thread::spawn(move || {
                if offload {
                    copy_offload(&src, &dst, mtu, direction)
                } else {
                    copy(&src, &dst, direction)
                }
            }));
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
