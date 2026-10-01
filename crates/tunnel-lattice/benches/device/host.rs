//! Host configuration for the benchmark's own devices: an IPv4 address on
//! the device, and the teardown guards for anything that would outlive the
//! process. Everything here acts only on state this process created:
//!
//! - addresses and routes live on the device and go away with it;
//! - a macOS TAP is a `feth` pair, a cloned interface that survives the
//!   process, so both ends get a guard that destroys them;
//! - on Windows an inbound firewall rule scoped to the benchmark subnet is
//!   added once per run and removed by its guard.
//!
//! Guards run on every exit path that unwinds (success, error, panic). A
//! killed process skips them; the CI workflow's leak checks catch that.

use std::net::Ipv4Addr;
use std::process::Command;
#[cfg(target_os = "windows")]
use std::time::Duration;

use tunnel_lattice::DeviceKind;

/// Runs a command, returning its combined output as the error on failure.
pub fn run(program: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|err| format!("run {program}: {err}"))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if output.status.success() {
        Ok(text)
    } else {
        Err(format!("{program} {args:?} failed: {}", text.trim()))
    }
}

/// Runs a command on drop, ignoring the result.
pub struct RunOnDrop {
    program: &'static str,
    args: Vec<String>,
}

impl Drop for RunOnDrop {
    fn drop(&mut self) {
        let _ = Command::new(self.program).args(&self.args).output();
    }
}

/// What [`configure`] set up; dropping it undoes what does not go away
/// with the device. Drop it after the device.
pub struct Configured {
    /// The macOS `feth` peer that the TAP's BPF descriptor reads from.
    pub feth_peer: Option<String>,
    _guards: Vec<RunOnDrop>,
}

/// Gives the device `name` the address `local` (a /24; on a macOS `utun`
/// a point-to-point link to `peer`) and brings it up.
#[cfg_attr(
    not(any(target_os = "linux", target_os = "macos", target_os = "windows")),
    allow(unused_variables)
)]
pub fn configure(
    kind: DeviceKind,
    name: &str,
    local: Ipv4Addr,
    peer: Ipv4Addr,
) -> Result<Configured, String> {
    let local = local.to_string();
    #[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
    let mut configured = Configured {
        feth_peer: None,
        _guards: Vec::new(),
    };
    #[cfg(target_os = "linux")]
    {
        let _ = (kind, peer);
        run("ip", &["addr", "add", &format!("{local}/24"), "dev", name])?;
        run("ip", &["link", "set", "dev", name, "up"])?;
    }
    #[cfg(target_os = "macos")]
    match kind {
        DeviceKind::Tap => {
            // The `dev` guard is armed before the peer lookup, so a failed
            // lookup still removes `dev`. Guards drop in reverse push
            // order: the peer first.
            configured._guards.push(RunOnDrop {
                program: "ifconfig",
                args: vec![name.to_owned(), "destroy".into()],
            });
            let peer_if = feth_peer_of(name)?;
            configured._guards.push(RunOnDrop {
                program: "ifconfig",
                args: vec![peer_if.clone(), "destroy".into()],
            });
            configured._guards.reverse();
            run("ifconfig", &[&peer_if, "up"])?;
            run(
                "ifconfig",
                &[name, "inet", &local, "netmask", "255.255.255.0", "up"],
            )?;
            configured.feth_peer = Some(peer_if);
        }
        _ => {
            run("ifconfig", &[name, "inet", &local, &peer.to_string(), "up"])?;
        }
    }
    // A fresh adapter is not always registered with the IP helper yet;
    // netsh then fails ("Failed to configure the DHCP service"). Retry for
    // a bounded time.
    #[cfg(target_os = "windows")]
    {
        let _ = (kind, peer);
        let name_arg = format!("name={name}");
        let args = [
            "interface",
            "ipv4",
            "set",
            "address",
            name_arg.as_str(),
            "static",
            &local,
            "255.255.255.0",
        ];
        let mut attempts = 0;
        loop {
            match run("netsh", &args) {
                Ok(_) => break,
                Err(err) if attempts >= 30 => return Err(err),
                Err(_) => {
                    attempts += 1;
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        }
    }
    Ok(configured)
}

/// The peer of the macOS `feth` interface `dev`, from the `peer: fethN`
/// line of `ifconfig <dev>` (tun-rs does not expose it).
#[cfg(target_os = "macos")]
fn feth_peer_of(dev: &str) -> Result<String, String> {
    let output = run("ifconfig", &[dev])?;
    output
        .lines()
        .find_map(|line| line.trim().strip_prefix("peer: "))
        .and_then(|rest| rest.split_whitespace().next())
        .map(str::to_owned)
        .ok_or_else(|| format!("no `peer:` line in `ifconfig {dev}`:\n{output}"))
}

/// Allows inbound UDP to the benchmark subnet through the Windows firewall
/// for this run; elsewhere nothing to do. The rule name carries the process
/// id, so the guard removes exactly the rule this process added.
pub fn allow_inbound(subnet: &str) -> Result<Option<RunOnDrop>, String> {
    #[cfg(target_os = "windows")]
    {
        let rule = format!("name=tunnel-lattice-device-bench-{}", std::process::id());
        run(
            "netsh",
            &[
                "advfirewall",
                "firewall",
                "add",
                "rule",
                &rule,
                "dir=in",
                "action=allow",
                "protocol=UDP",
                &format!("localip={subnet}"),
                "profile=any",
            ],
        )?;
        Ok(Some(RunOnDrop {
            program: "netsh",
            args: vec![
                "advfirewall".into(),
                "firewall".into(),
                "delete".into(),
                "rule".into(),
                rule,
            ],
        }))
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = subnet;
        Ok(None)
    }
}

/// Makes a `recv` stuck on the device return, best effort, when the
/// reflector does not stop on its own: Linux deletes the device, macOS TAP
/// destroys the `feth` peer, Windows TAP disables the adapter. A macOS
/// `utun` and a Wintun adapter go away when the process exits.
#[cfg_attr(
    not(any(target_os = "linux", target_os = "macos", target_os = "windows")),
    allow(unused_variables)
)]
pub fn release(kind: DeviceKind, name: &str, feth_peer: Option<&str>) {
    #[cfg(target_os = "linux")]
    {
        let _ = (kind, feth_peer);
        let _ = run("ip", &["link", "del", name]);
    }
    #[cfg(target_os = "macos")]
    {
        let _ = (kind, name);
        if let Some(peer) = feth_peer {
            let _ = run("ifconfig", &[peer, "destroy"]);
        }
    }
    #[cfg(target_os = "windows")]
    {
        let _ = feth_peer;
        if kind == DeviceKind::Tap {
            let _ = run(
                "powershell",
                &[
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    &format!("Disable-NetAdapter -Name '{name}' -Confirm:$false"),
                ],
            );
        }
    }
}

/// The CPU model, best effort.
pub fn cpu_model() -> String {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/cpuinfo")
            .ok()
            .and_then(|info| {
                info.lines()
                    .find_map(|line| line.strip_prefix("model name"))
                    .and_then(|rest| rest.split_once(':'))
                    .map(|(_, model)| model.trim().to_owned())
            })
            .unwrap_or_default()
    }
    #[cfg(target_os = "macos")]
    {
        run("sysctl", &["-n", "machdep.cpu.brand_string"])
            .map(|model| model.trim().to_owned())
            .unwrap_or_default()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        std::env::var("PROCESSOR_IDENTIFIER").unwrap_or_default()
    }
}

/// The OS kernel or release, best effort.
pub fn kernel() -> String {
    #[cfg(unix)]
    let version = run("uname", &["-sr"]);
    #[cfg(not(unix))]
    let version = run("cmd", &["/C", "ver"]);
    version
        .map(|text| text.trim().to_owned())
        .unwrap_or_default()
}
