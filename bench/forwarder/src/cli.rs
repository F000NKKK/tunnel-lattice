//! The forwarder command line and process helpers.
//!
//! Every forwarder binary accepts the same arguments, compatible with
//! tun-benchmark2's forwarders:
//!
//! ```text
//! --iface1 <name> --ip1 <ipv4> --iface2 <name> --ip2 <ipv4> [--threads N] [--mtu M] [--offload]
//! ```
//!
//! The raw tun-rs binaries assign `--ip1`/`--ip2` (prefix 24) themselves.
//! The tunnel-lattice binaries accept but ignore them: tunnel-lattice has
//! no address API, so the run script assigns addresses for every variant.
//!
//! `--offload` (no value) opens both devices with Linux TUN segmentation
//! offload and switches every binary to its batch/offload loop: tun-rs's
//! `recv_multiple`/`send_multiple`, or the tunnel-lattice facade's `recv`
//! (which splits the kernel's super-packets) with `send_batch`. A binary
//! exits with an error if the device did not grant offload, so a run can
//! never silently measure the plain path under an offload label.

use std::fmt;
use std::io::Write;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicU64, Ordering};

/// Default MTU of both devices, matching the run script.
pub const DEFAULT_MTU: u16 = 1500;

/// Most packets one `--offload` copy loop hands to a single batch send:
/// tun-rs's `IDEAL_BATCH_SIZE`, and the most packets the tunnel-lattice
/// tun-rs backend's `send_batch` accepts per call.
pub const OFFLOAD_BATCH: usize = 128;

/// Exit code of a forwarder binary built for a platform other than Linux.
pub const EXIT_UNSUPPORTED: i32 = 2;

/// Parsed forwarder arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    /// Name of the first (host-side) device.
    pub iface1: String,
    /// Address of the first device.
    pub ip1: Ipv4Addr,
    /// Name of the second device (moved into the network namespace).
    pub iface2: String,
    /// Address of the second device.
    pub ip2: Ipv4Addr,
    /// Threads per direction (sync), or runtime worker threads (Tokio);
    /// `None` keeps each binary's default.
    pub threads: Option<usize>,
    /// MTU of both devices.
    pub mtu: u16,
    /// Open with segmentation offload and use the batch/offload loop.
    pub offload: bool,
}

/// A rejected command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgsError(String);

impl fmt::Display for ArgsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ArgsError {}

impl Args {
    /// Parses `args` (without the program name).
    ///
    /// # Errors
    ///
    /// A missing or repeated required option, an unknown option, a missing
    /// value, an invalid IPv4 address, a zero `--threads`, a zero MTU, or a
    /// repeated `--offload`.
    pub fn parse<I, S>(args: I) -> Result<Self, ArgsError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut iface1 = None;
        let mut ip1 = None;
        let mut iface2 = None;
        let mut ip2 = None;
        let mut threads = None;
        let mut mtu = None;
        let mut offload = None;

        let mut args = args.into_iter().map(Into::into);
        while let Some(flag) = args.next() {
            // The only option without a value.
            if flag == "--offload" {
                set_once(&mut offload, &flag, true)?;
                continue;
            }
            let value = args
                .next()
                .ok_or_else(|| ArgsError(format!("{flag} needs a value")))?;
            match flag.as_str() {
                "--iface1" => set_once(&mut iface1, &flag, value)?,
                "--iface2" => set_once(&mut iface2, &flag, value)?,
                "--ip1" => set_once(&mut ip1, &flag, parse_ipv4(&flag, &value)?)?,
                "--ip2" => set_once(&mut ip2, &flag, parse_ipv4(&flag, &value)?)?,
                "--threads" => set_once(&mut threads, &flag, parse_nonzero(&flag, &value)?)?,
                "--mtu" => {
                    let parsed = parse_nonzero(&flag, &value)?;
                    let parsed = u16::try_from(parsed)
                        .map_err(|_| ArgsError(format!("{flag} {value}: above 65535")))?;
                    set_once(&mut mtu, &flag, parsed)?;
                }
                _ => return Err(ArgsError(format!("unknown option {flag}"))),
            }
        }

        Ok(Self {
            iface1: iface1.ok_or_else(|| missing("--iface1"))?,
            ip1: ip1.ok_or_else(|| missing("--ip1"))?,
            iface2: iface2.ok_or_else(|| missing("--iface2"))?,
            ip2: ip2.ok_or_else(|| missing("--ip2"))?,
            threads,
            mtu: mtu.unwrap_or(DEFAULT_MTU),
            offload: offload.unwrap_or(false),
        })
    }

    /// Parses the process arguments, or prints the error and usage and
    /// exits with code 64 (`EX_USAGE`).
    pub fn from_env_or_exit() -> Self {
        Self::parse(std::env::args().skip(1)).unwrap_or_else(|error| {
            eprintln!("{error}");
            eprintln!(
                "usage: --iface1 <name> --ip1 <ipv4> --iface2 <name> --ip2 <ipv4> \
                 [--threads N] [--mtu M] [--offload]"
            );
            std::process::exit(64)
        })
    }
}

fn set_once<T>(slot: &mut Option<T>, flag: &str, value: T) -> Result<(), ArgsError> {
    if slot.is_some() {
        return Err(ArgsError(format!("{flag} given more than once")));
    }
    *slot = Some(value);
    Ok(())
}

fn parse_ipv4(flag: &str, value: &str) -> Result<Ipv4Addr, ArgsError> {
    value
        .parse()
        .map_err(|_| ArgsError(format!("{flag} {value}: not an IPv4 address")))
}

fn parse_nonzero(flag: &str, value: &str) -> Result<usize, ArgsError> {
    match value.parse::<usize>() {
        Ok(0) | Err(_) => Err(ArgsError(format!("{flag} {value}: not a positive integer"))),
        Ok(parsed) => Ok(parsed),
    }
}

fn missing(flag: &str) -> ArgsError {
    ArgsError(format!("missing {flag}"))
}

/// Prints the `ready <iface1> <iface2>` line the run script waits for.
pub fn ready(args: &Args) {
    let mut stdout = std::io::stdout().lock();
    // A closed stdout must not stop the forwarder; the script also polls
    // for the interfaces themselves.
    let _ = writeln!(stdout, "ready {} {}", args.iface1, args.iface2);
    let _ = stdout.flush();
}

/// Prints `context: error` and exits the whole process with code 1.
///
/// Used for every unexpected receive/send error, so a failing copy thread
/// or task can never leave a half-working forwarder that the benchmark
/// would record as a low number.
pub fn fatal(context: &str, error: impl fmt::Display) -> ! {
    eprintln!("{context}: {error}");
    std::process::exit(1)
}

/// The `main` of a forwarder built for a platform other than Linux.
pub fn linux_only(binary: &str) -> ! {
    eprintln!("{binary}: the forwarder benchmark runs on Linux only");
    std::process::exit(EXIT_UNSUPPORTED)
}

/// Counts packets skipped because they did not fit the receive buffer.
///
/// Logs the first one and every power of two after it, so a misconfigured
/// buffer is visible in the forwarder log without flooding it.
#[derive(Debug, Default)]
pub struct SkipCounter(AtomicU64);

impl SkipCounter {
    /// A counter at zero.
    pub const fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    /// Records one skipped packet and returns the new total.
    pub fn record(&self, what: &str) -> u64 {
        let total = self.0.fetch_add(1, Ordering::Relaxed) + 1;
        if total.is_power_of_two() {
            eprintln!("{what}: {total} packet(s) skipped (larger than the receive buffer)");
        }
        total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Vec<&'static str> {
        vec![
            "--iface1", "tun11", "--ip1", "10.0.1.1", "--iface2", "tun22", "--ip2", "10.0.2.1",
        ]
    }

    #[test]
    fn parses_the_required_options_with_defaults() {
        let args = Args::parse(base()).expect("valid");
        assert_eq!(args.iface1, "tun11");
        assert_eq!(args.ip1, Ipv4Addr::new(10, 0, 1, 1));
        assert_eq!(args.iface2, "tun22");
        assert_eq!(args.ip2, Ipv4Addr::new(10, 0, 2, 1));
        assert_eq!(args.threads, None);
        assert_eq!(args.mtu, DEFAULT_MTU);
        assert!(!args.offload);
    }

    #[test]
    fn parses_threads_and_mtu_in_any_order() {
        let mut argv = vec!["--mtu", "9000", "--threads", "2"];
        argv.extend(base());
        let args = Args::parse(argv).expect("valid");
        assert_eq!(args.threads, Some(2));
        assert_eq!(args.mtu, 9000);
    }

    #[test]
    fn offload_is_a_flag_without_a_value() {
        for argv in [
            [vec!["--offload"], base()].concat(),
            [base(), vec!["--offload"]].concat(),
            [
                vec!["--iface1", "tun11", "--offload", "--ip1", "10.0.1.1"],
                base()[4..].to_vec(),
            ]
            .concat(),
        ] {
            let args = Args::parse(argv.clone()).expect("valid");
            assert!(args.offload, "{argv:?}");
            assert_eq!(args.ip1, Ipv4Addr::new(10, 0, 1, 1), "{argv:?}");
        }
    }

    #[test]
    fn rejects_bad_command_lines() {
        let cases: Vec<(Vec<&str>, &str)> = vec![
            (base()[2..].to_vec(), "missing --iface1"),
            (
                [base(), vec!["--threads", "0"]].concat(),
                "not a positive integer",
            ),
            ([base(), vec!["--mtu", "70000"]].concat(), "above 65535"),
            (
                [base(), vec!["--mtu", "0"]].concat(),
                "not a positive integer",
            ),
            ([base(), vec!["--iface1", "x"]].concat(), "more than once"),
            ([base(), vec!["--threads"]].concat(), "needs a value"),
            ([base(), vec!["--gso", "1"]].concat(), "unknown option"),
            (
                [base(), vec!["--offload", "--offload"]].concat(),
                "--offload given more than once",
            ),
            // `--offload` takes no value, so a following word is a flag.
            ([base(), vec!["--offload", "1"]].concat(), "1 needs a value"),
        ];
        for (argv, expected) in cases {
            let error = Args::parse(argv.clone()).expect_err("invalid");
            assert!(error.to_string().contains(expected), "{argv:?}: {error}");
        }

        let mut bad_ip = base();
        bad_ip[3] = "10.0.1";
        let error = Args::parse(bad_ip).expect_err("invalid");
        assert!(error.to_string().contains("not an IPv4 address"), "{error}");
    }

    #[test]
    fn skip_counter_counts() {
        let counter = SkipCounter::new();
        assert_eq!(counter.record("test"), 1);
        assert_eq!(counter.record("test"), 2);
    }
}
