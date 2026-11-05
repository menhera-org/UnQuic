use std::net::Ipv6Addr;
use unquic::{
    UnquicUuid,
    config::{Config, Daemon, Endpoint, Peer, Session},
};

fn config() -> Config {
    Config {
        daemon: Daemon::default(),
        endpoints: vec![Endpoint {
            name: "default".into(),
            listen_address: Ipv6Addr::UNSPECIFIED,
            listen_port: 40000,
            scope_id: 0,
            bind_device: None,
        }],
        peers: (0..2)
            .map(|i| Peer {
                name: format!("p{i}"),
                endpoint: "default".into(),
                address: Some(Ipv6Addr::LOCALHOST),
                peer_port: 40001 + i,
                scope_id: 0,
                tx_lanes: 4,
                outer_payload_limit: unquic::BASE_UDP_PAYLOAD,
                path_group: None,
            })
            .collect(),
        sessions: (0..3)
            .map(|i| Session {
                name: format!("s{i}"),
                peer: format!("p{}", i / 2),
                cookie: UnquicUuid::from_bytes([i + 1; 16]),
                tap_name: None,
                inner_mtu: 2400,
                mac: None,
                queue_count: 2,
            })
            .collect(),
    }
}

#[test]
fn one_endpoint_many_peers_and_many_sessions_resolve_to_numeric_indices() {
    let config = config().validate().unwrap();
    assert_eq!(config.config().daemon.memory_limit, 256 * 1024 * 1024);
    for peer in 0..2 {
        assert_eq!(config.endpoint_for_peer(peer), Some(0));
    }
    for session in 0..3 {
        assert_eq!(config.peer_for_session(session), Some(session / 2));
        assert_eq!(
            config.session_for_cookie(UnquicUuid::from_bytes([session as u8 + 1; 16])),
            Some(session)
        );
    }
    assert_eq!(config.endpoint_for_peer(usize::MAX), None);
    assert_eq!(config.peer_for_session(usize::MAX), None);
    assert_eq!(
        config.session_for_cookie(UnquicUuid::from_bytes([255; 16])),
        None
    );
    assert_eq!(config.config().sessions[1].tap_name(), "s1");
    assert_eq!(config.config().endpoints[0].socket_addr().port(), 40000);
    assert_eq!(
        config.config().peers[1].socket_addr().unwrap().port(),
        40002
    );
}

#[test]
fn duplicate_names_cookies_and_effective_tap_names_are_rejected() {
    let mutations: &[fn(&mut Config)] = &[
        |c| c.endpoints.push(c.endpoints[0].clone()),
        |c| c.peers[1].name = c.peers[0].name.clone(),
        |c| c.sessions[1].name = c.sessions[0].name.clone(),
        |c| c.sessions[1].cookie = c.sessions[0].cookie,
        |c| c.sessions[1].tap_name = Some(c.sessions[0].name.clone()),
    ];
    for (mutate, field) in mutations.iter().zip([
        "endpoints.name",
        "peers.name",
        "sessions.name",
        "sessions.cookie",
        "sessions.tap_name",
    ]) {
        let mut c = config();
        mutate(&mut c);
        assert_eq!(c.validate().err().unwrap().field, field);
    }
}

#[test]
fn scalar_and_reference_validation_reports_the_offending_field() {
    type Case = (&'static str, fn(&mut Config));
    let cases: &[Case] = &[
        ("daemon.memory_limit", |c| c.daemon.memory_limit = 0),
        ("endpoints.name", |c| c.endpoints[0].name.clear()),
        ("endpoints.listen_port", |c| c.endpoints[0].listen_port = 0),
        ("endpoints.bind_device", |c| {
            c.endpoints[0].bind_device = Some(c"".into())
        }),
        ("peers.endpoint", |c| c.peers[0].endpoint = "missing".into()),
        ("peers.peer_port", |c| c.peers[0].peer_port = 0),
        ("peers.tx_lanes", |c| c.peers[0].tx_lanes = 0),
        ("peers.tx_lanes", |c| c.peers[0].tx_lanes = 65),
        ("peers.path_group", |c| {
            c.peers[0].path_group = Some(String::new())
        }),
        ("sessions.peer", |c| c.sessions[0].peer = "missing".into()),
        ("sessions.inner_mtu", |c| c.sessions[0].inner_mtu = 1499),
        ("sessions.inner_mtu", |c| c.sessions[0].inner_mtu = 9001),
        ("sessions.queue_count", |c| c.sessions[0].queue_count = 0),
        ("sessions.queue_count", |c| c.sessions[0].queue_count = 65),
    ];
    for (field, mutate) in cases {
        let mut c = config();
        mutate(&mut c);
        let error = c.validate().err().unwrap();
        assert_eq!(error.field, *field);
        assert!(!error.to_string().contains("01010101-0101"));
    }
    for name in [
        "",
        ".",
        "..",
        "a/b",
        "a:b",
        "a b",
        "a\tb",
        "a\nb",
        "a\0b",
        "tap%d",
        "0123456789abcdef",
    ] {
        let mut c = config();
        c.sessions[0].tap_name = Some(name.into());
        assert_eq!(c.validate().err().unwrap().field, "sessions.tap_name");
    }
}

#[test]
fn address_scope_and_receive_endpoint_overlap_are_checked_without_io() {
    for ip in ["ff02::1", "::ffff:192.0.2.1", "fe80::1"] {
        let mut c = config();
        c.endpoints[0].listen_address = ip.parse().unwrap();
        assert_eq!(
            c.validate().err().unwrap().field,
            "endpoints.listen_address"
        );
    }
    for ip in ["::", "ff02::1", "::ffff:192.0.2.1", "fe80::1"] {
        let mut c = config();
        c.peers[0].address = Some(ip.parse().unwrap());
        assert_eq!(c.validate().err().unwrap().field, "peers.address");
    }
    let mut c = config();
    c.endpoints[0].listen_address = "fe80::1".parse().unwrap();
    c.endpoints[0].scope_id = 3;
    c.peers[0].address = Some("fe80::2".parse().unwrap());
    c.peers[0].scope_id = 3;
    assert!(c.validate().is_ok());

    let mut c = config();
    let mut duplicate = c.endpoints[0].clone();
    duplicate.name = "second".into();
    duplicate.listen_address = Ipv6Addr::LOCALHOST;
    c.endpoints.push(duplicate);
    assert_eq!(
        c.clone().validate().err().unwrap().field,
        "endpoints.listen_address"
    );
    c.endpoints[1].bind_device = Some(c"vrf-test".into());
    assert!(c.validate().is_ok());
}

#[test]
fn unresolved_peers_and_configuration_boundaries_are_allowed() {
    let mut c = config();
    c.peers[0].address = None;
    assert_eq!(c.peers[0].socket_addr(), None);
    c.sessions[0].inner_mtu = 1500;
    c.sessions[1].inner_mtu = 9000;
    c.sessions[0].queue_count = 1;
    c.sessions[1].queue_count = 64;
    c.peers[0].tx_lanes = 1;
    c.peers[1].tx_lanes = 64;
    c.sessions[0].tap_name = Some("0123456789abcde".into());
    let validated = c.validate().unwrap();
    assert_eq!(validated.config().peers[0].address, None);
    assert!(Config::default().validate().is_ok());
}

#[test]
fn outer_payload_limits_are_bounded_per_peer() {
    for limit in [1232, 1300, 1452] {
        let mut c = config();
        c.peers[1].outer_payload_limit = limit;
        let valid = c.validate().unwrap();
        assert_eq!(valid.config().peers[0].outer_payload_limit, 1232);
        assert_eq!(valid.config().peers[1].outer_payload_limit, limit);
    }
    for limit in [0, 1231, 1453, usize::MAX] {
        let mut c = config();
        c.peers[1].outer_payload_limit = limit;
        let error = c.validate().err().unwrap();
        assert_eq!(error.field, "peers.outer_payload_limit");
    }
}
