//! Unprivileged tests of the real-device benchmark's pure parts: packet
//! reflection (`benches/device/packet.rs`) and its statistics and report
//! output (`benches/device/report.rs`). The benchmark itself needs a real
//! device and privilege; these run in every `cargo test`.

#[allow(dead_code)]
#[path = "../benches/device/packet.rs"]
mod packet;
#[allow(dead_code)]
#[path = "../benches/device/report.rs"]
mod report;

use std::net::Ipv4Addr;

use packet::{Action, Flow, Framing, MIN_ETHERNET_FRAME, PEER_MAC, STOP_MAGIC, reflect};
use report::{Meta, Outcome, Scenario};

const FLOW: Flow = Flow {
    local: Ipv4Addr::new(198, 18, 11, 1),
    peer: Ipv4Addr::new(198, 18, 11, 2),
};

const HOST_MAC: [u8; 6] = [0x02, 0, 0, 0, 0, 0x01];

/// The 16-bit one's-complement sum of `bytes` (RFC 1071), odd length
/// padded with a zero byte.
fn sum16(bytes: &[u8]) -> u32 {
    let mut sum = 0u32;
    for chunk in bytes.chunks(2) {
        let word = u16::from_be_bytes([chunk[0], *chunk.get(1).unwrap_or(&0)]);
        sum += u32::from(word);
    }
    sum
}

fn fold(mut sum: u32) -> u16 {
    while sum > 0xffff {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    u16::try_from(sum).unwrap()
}

/// An IPv4/UDP datagram `src:sport -> dst:dport` with valid IPv4 and UDP
/// checksums.
fn udp(src: Ipv4Addr, sport: u16, dst: Ipv4Addr, dport: u16, payload: &[u8]) -> Vec<u8> {
    let total = 28 + payload.len();
    let mut p = vec![0u8; total];
    p[0] = 0x45;
    p[2..4].copy_from_slice(&u16::try_from(total).unwrap().to_be_bytes());
    p[8] = 64;
    p[9] = 17;
    p[12..16].copy_from_slice(&src.octets());
    p[16..20].copy_from_slice(&dst.octets());
    let header = !fold(sum16(&p[..20]));
    p[10..12].copy_from_slice(&header.to_be_bytes());
    p[20..22].copy_from_slice(&sport.to_be_bytes());
    p[22..24].copy_from_slice(&dport.to_be_bytes());
    let udp_len = u16::try_from(8 + payload.len()).unwrap();
    p[24..26].copy_from_slice(&udp_len.to_be_bytes());
    p[28..].copy_from_slice(payload);
    let checksum = !fold(udp_pseudo_sum(&p));
    p[26..28].copy_from_slice(&checksum.to_be_bytes());
    p
}

/// The UDP checksum sum: pseudo-header plus the UDP header and payload.
fn udp_pseudo_sum(p: &[u8]) -> u32 {
    let udp_len = u16::from_be_bytes([p[24], p[25]]);
    sum16(&p[12..20]) + 17 + u32::from(udp_len) + sum16(&p[20..])
}

fn checksums_valid(p: &[u8]) -> bool {
    fold(sum16(&p[..20])) == 0xffff && fold(udp_pseudo_sum(p)) == 0xffff
}

fn ethernet(dst: [u8; 6], src: [u8; 6], ethertype: u16, body: &[u8]) -> Vec<u8> {
    let mut frame = Vec::new();
    frame.extend_from_slice(&dst);
    frame.extend_from_slice(&src);
    frame.extend_from_slice(&ethertype.to_be_bytes());
    frame.extend_from_slice(body);
    frame
}

fn arp_request(sender_mac: [u8; 6], sender: Ipv4Addr, target: Ipv4Addr) -> Vec<u8> {
    let mut arp = vec![0x00, 0x01, 0x08, 0x00, 6, 4, 0x00, 0x01];
    arp.extend_from_slice(&sender_mac);
    arp.extend_from_slice(&sender.octets());
    arp.extend_from_slice(&[0; 6]);
    arp.extend_from_slice(&target.octets());
    ethernet([0xff; 6], sender_mac, 0x0806, &arp)
}

fn reflect_ip(packet: &mut [u8]) -> Action {
    reflect(Framing::Ip, FLOW, packet, &mut [0u8; MIN_ETHERNET_FRAME])
}

#[test]
fn a_tun_datagram_of_the_flow_is_reflected_with_valid_checksums() {
    let original = udp(FLOW.local, 40000, FLOW.peer, 7, b"0123456789abcdef");
    assert!(checksums_valid(&original));
    let mut packet = original.clone();
    assert_eq!(reflect_ip(&mut packet), Action::Reflect);
    assert_eq!(packet[12..16], FLOW.peer.octets());
    assert_eq!(packet[16..20], FLOW.local.octets());
    assert_eq!(packet[20..22], 7u16.to_be_bytes());
    assert_eq!(packet[22..24], 40000u16.to_be_bytes());
    assert_eq!(
        packet[24..],
        original[24..],
        "length, checksum and payload unchanged"
    );
    assert!(
        checksums_valid(&packet),
        "swapping keeps both checksums valid"
    );
}

#[test]
fn a_stop_datagram_stops_and_is_not_modified() {
    let mut payload = STOP_MAGIC.to_vec();
    payload.extend_from_slice(&[0; 8]);
    let original = udp(FLOW.local, 40000, FLOW.peer, 7, &payload);
    let mut packet = original.clone();
    assert_eq!(reflect_ip(&mut packet), Action::Stop);
    assert_eq!(packet, original);
}

#[test]
fn packets_outside_the_flow_are_ignored_untouched() {
    let other = Ipv4Addr::new(198, 18, 12, 2);
    let mut tcp = udp(FLOW.local, 40000, FLOW.peer, 7, b"x");
    tcp[9] = 6;
    let mut fragment = udp(FLOW.local, 40000, FLOW.peer, 7, b"x");
    fragment[6] = 0x20; // more fragments
    let mut later_fragment = udp(FLOW.local, 40000, FLOW.peer, 7, b"x");
    later_fragment[7] = 1; // offset 8
    let mut ipv6 = vec![0u8; 48];
    ipv6[0] = 0x60;
    let mut long_total = udp(FLOW.local, 40000, FLOW.peer, 7, b"x");
    long_total[3] += 1; // claims one byte more than was received
    let mut bad_ihl = udp(FLOW.local, 40000, FLOW.peer, 7, b"x");
    bad_ihl[0] = 0x44;
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("wrong destination", udp(FLOW.local, 40000, other, 7, b"x")),
        ("wrong source", udp(other, 40000, FLOW.peer, 7, b"x")),
        ("reversed flow", udp(FLOW.peer, 7, FLOW.local, 40000, b"x")),
        ("tcp", tcp),
        ("fragment", fragment),
        ("later fragment", later_fragment),
        ("ipv6", ipv6),
        ("total length past the buffer", long_total),
        ("header shorter than 20 bytes", bad_ihl),
        (
            "truncated",
            udp(FLOW.local, 40000, FLOW.peer, 7, b"x")[..19].to_vec(),
        ),
        ("empty", Vec::new()),
    ];
    for (name, original) in cases {
        let mut packet = original.clone();
        assert_eq!(reflect_ip(&mut packet), Action::Ignore, "{name}");
        assert_eq!(packet, original, "{name}: left untouched");
    }
}

#[test]
fn a_tap_frame_of_the_flow_is_reflected_with_swapped_macs() {
    let datagram = udp(FLOW.local, 40000, FLOW.peer, 7, b"payload!");
    // Ethernet padding after the IP packet is carried along unchanged.
    let mut body = datagram.clone();
    body.extend_from_slice(&[0; 6]);
    let original = ethernet(PEER_MAC, HOST_MAC, 0x0800, &body);
    let mut frame = original.clone();
    let mut reply = [0u8; MIN_ETHERNET_FRAME];
    assert_eq!(
        reflect(Framing::Ethernet, FLOW, &mut frame, &mut reply),
        Action::Reflect
    );
    assert_eq!(frame[..6], HOST_MAC);
    assert_eq!(frame[6..12], PEER_MAC);
    assert_eq!(frame.len(), original.len());
    assert!(checksums_valid(&frame[14..14 + datagram.len()]));
    assert_eq!(frame[14 + datagram.len()..], [0; 6]);
}

#[test]
fn an_arp_request_for_the_peer_gets_a_padded_reply() {
    let mut request = arp_request(HOST_MAC, FLOW.local, FLOW.peer);
    let mut reply = [0xeeu8; MIN_ETHERNET_FRAME];
    assert_eq!(
        reflect(Framing::Ethernet, FLOW, &mut request, &mut reply),
        Action::Reply(MIN_ETHERNET_FRAME)
    );
    assert_eq!(reply[..6], HOST_MAC, "unicast back to the requester");
    assert_eq!(reply[6..12], PEER_MAC);
    assert_eq!(reply[12..14], [0x08, 0x06]);
    assert_eq!(reply[14..22], [0x00, 0x01, 0x08, 0x00, 6, 4, 0x00, 0x02]);
    assert_eq!(reply[22..28], PEER_MAC, "sender hardware address");
    assert_eq!(reply[28..32], FLOW.peer.octets(), "sender protocol address");
    assert_eq!(reply[32..38], HOST_MAC, "target hardware address");
    assert_eq!(
        reply[38..42],
        FLOW.local.octets(),
        "target protocol address"
    );
    assert_eq!(reply[42..], [0; 18], "zero padding to the minimum frame");
}

#[test]
fn other_ethernet_frames_are_ignored() {
    let mut reply = [0u8; MIN_ETHERNET_FRAME];
    let mut reply_op = arp_request(HOST_MAC, FLOW.local, FLOW.peer);
    reply_op[21] = 2;
    let cases: Vec<(&str, Vec<u8>)> = vec![
        (
            "ARP for another address",
            arp_request(HOST_MAC, FLOW.local, Ipv4Addr::new(198, 18, 11, 3)),
        ),
        ("ARP reply", reply_op),
        (
            "truncated ARP",
            arp_request(HOST_MAC, FLOW.local, FLOW.peer)[..41].to_vec(),
        ),
        (
            "IPv6 ethertype",
            ethernet(PEER_MAC, HOST_MAC, 0x86dd, &[0x60; 40]),
        ),
        (
            "IPv4 outside the flow",
            ethernet(
                PEER_MAC,
                HOST_MAC,
                0x0800,
                &udp(FLOW.peer, 7, FLOW.local, 40000, b"x"),
            ),
        ),
        ("shorter than a header", vec![0; 13]),
    ];
    for (name, original) in cases {
        let mut frame = original.clone();
        assert_eq!(
            reflect(Framing::Ethernet, FLOW, &mut frame, &mut reply),
            Action::Ignore,
            "{name}"
        );
        assert_eq!(frame, original, "{name}: left untouched");
    }
    let mut request = arp_request(HOST_MAC, FLOW.local, FLOW.peer);
    assert_eq!(
        reflect(Framing::Ethernet, FLOW, &mut request, &mut [0u8; 59]),
        Action::Ignore,
        "a reply buffer shorter than a minimum frame is never written past"
    );
}

#[test]
fn percentiles_use_the_nearest_rank() {
    let sorted: Vec<u64> = (1..=100).collect();
    assert_eq!(report::percentile(&sorted, 50.0), 50);
    assert_eq!(report::percentile(&sorted, 99.0), 99);
    assert_eq!(report::percentile(&sorted, 100.0), 100);
    assert_eq!(report::percentile(&[7], 50.0), 7);
    assert_eq!(
        report::percentile(&[1, 2, 3], 0.0),
        1,
        "rank clamps to the first sample"
    );
    assert_eq!(report::percentile(&[1, 2, 3], 50.0), 2);
}

#[test]
fn latency_summarizes_unsorted_samples_and_needs_at_least_one() {
    let stats = report::latency(vec![30, 10, 20, 40], 2).unwrap();
    assert_eq!(stats.samples, 4);
    assert_eq!(stats.lost, 2);
    assert_eq!((stats.min_ns, stats.max_ns), (10, 40));
    assert_eq!(stats.p50_ns, 20);
    assert_eq!(stats.p99_ns, 40);
    assert_eq!(stats.mean_ns, 25);
    assert!(report::latency(Vec::new(), 5).is_none());
}

#[test]
fn throughput_rates_and_loss() {
    let t = report::throughput(2.0, 1000, 1100, 1045, 1428);
    assert!((t.pps - 500.0).abs() < 1e-9);
    assert!((t.mbps - 500.0 * 1428.0 * 8.0 / 1e6).abs() < 1e-9);
    assert!((t.loss_percent() - 5.0).abs() < 1e-9);
    let empty = report::throughput(0.0, 0, 0, 0, 92);
    assert_eq!(empty.pps, 0.0);
    assert_eq!(empty.loss_percent(), 0.0);
    // More echoes than datagrams counted (a late echo) is no loss, not an
    // underflow.
    assert_eq!(report::throughput(1.0, 1, 1, 2, 92).loss_percent(), 0.0);
}

#[test]
fn json_strings_are_escaped() {
    assert_eq!(report::json_escape("plain"), "plain");
    assert_eq!(
        report::json_escape("a\"b\\c\nd\te\u{1}"),
        "a\\\"b\\\\c\\nd\\te\\u0001"
    );
}

#[test]
fn utc_timestamps() {
    assert_eq!(report::utc_rfc3339(0), "1970-01-01T00:00:00Z");
    assert_eq!(report::utc_rfc3339(951_782_400), "2000-02-29T00:00:00Z");
    assert_eq!(report::utc_rfc3339(1_700_000_000), "2023-11-14T22:13:20Z");
}

fn sample_run() -> (Meta, Vec<Scenario>) {
    let meta = Meta {
        created_utc: "2026-10-01T00:00:00Z".into(),
        os: "linux".into(),
        arch: "x86_64".into(),
        build: "default".into(),
        cpus: 4,
        git_sha: "0123456789abcdef".into(),
        crate_version: "0.4.0".into(),
        rtts: 2000,
        secs: 3.0,
        window: 64,
        ..Meta::default()
    };
    let ok = Scenario {
        path: "sync".into(),
        label: "sync Handle::recv/send".into(),
        kind: "tun".into(),
        payload: 64,
        wire_len: 92,
        latency: report::latency(vec![10_000, 20_000, 30_000], 0),
        throughput: Some(report::throughput(1.0, 1000, 1000, 1000, 92)),
        outcome: Outcome::Ok,
    };
    let skipped = Scenario {
        path: "thread-bridge".into(),
        label: "thread bridge".into(),
        kind: "tap".into(),
        payload: 0,
        wire_len: 0,
        latency: None,
        throughput: None,
        outcome: Outcome::Skipped("no driver".into()),
    };
    let failed = Scenario {
        outcome: Outcome::Failed("no echo | \"quoted\"\nnext".into()),
        latency: None,
        throughput: None,
        ..ok.clone()
    };
    (meta, vec![ok, skipped, failed])
}

#[test]
fn markdown_has_one_row_per_scenario_and_a_footer() {
    let (meta, scenarios) = sample_run();
    let md = report::to_markdown(&meta, &scenarios);
    assert!(md.contains("linux (x86_64), `default` build"));
    assert!(
        md.contains("| sync Handle::recv/send | TUN | 64 B | 20.0 | 30.0 | 1000 | 0.7 | 0.00 % |")
    );
    assert!(md.contains("| thread bridge | TAP | — | skipped: no driver |"));
    assert!(
        md.contains("**failed**: no echo \\| \"quoted\" next"),
        "a reason cannot break the table: {md}"
    );
    assert!(md.contains("tunnel-lattice 0.4.0 at 0123456"));
    assert!(md.contains("a local host"));
    let rows = md.lines().filter(|line| line.starts_with("| ")).count();
    assert_eq!(
        rows,
        1 + scenarios.len(),
        "header plus one row per scenario"
    );

    let not_started = Scenario {
        payload: 0,
        ..scenarios[2].clone()
    };
    let md = report::to_markdown(&meta, &[not_started]);
    assert!(md.contains("| TUN | — | **failed**: "), "{md}");
}

#[test]
fn json_records_every_scenario_and_its_outcome() {
    let (meta, scenarios) = sample_run();
    let json = report::to_json(&meta, &scenarios);
    assert!(json.contains(&format!("\"schema\": \"{}\"", report::SCHEMA)));
    assert_eq!(json.matches("\"path\": ").count(), 3);
    assert!(json.contains("\"outcome\": \"ok\", \"reason\": null"));
    assert!(json.contains("\"outcome\": \"skipped\", \"reason\": \"no driver\""));
    assert!(json.contains("\"reason\": \"no echo | \\\"quoted\\\"\\nnext\""));
    assert!(json.contains("\"p50_ns\": 20000"));
    assert!(json.contains("\"latency\": null, \"throughput\": null"));
    // Balanced braces and brackets: the hand-written JSON stays well formed.
    let depth = json.chars().try_fold(0i32, |depth, c| {
        let depth = match c {
            '{' | '[' => depth + 1,
            '}' | ']' => depth - 1,
            _ => depth,
        };
        (depth >= 0).then_some(depth)
    });
    assert_eq!(depth, Some(0));
}
