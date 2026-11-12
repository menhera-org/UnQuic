use serde::Deserialize;
use std::{
    io,
    net::{Ipv6Addr, SocketAddr},
    time::Duration,
};
use unquic::{
    MacAddress,
    config::{Config, Daemon, Endpoint, Peer, Session},
    dns::{
        BindDevice, ClientId, Dhcpv6ResolverConfig, IpHost, Ipv6ResolverConfig, RaResolverConfig,
        Resolver, StaticResolverConfig,
    },
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    daemon: Daemon,
    #[serde(default)]
    endpoints: Vec<Endpoint>,
    #[serde(default)]
    peers: Vec<FilePeer>,
    #[serde(default)]
    sessions: Vec<Session>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FilePeer {
    name: String,
    endpoint: String,
    host: IpHost,
    peer_port: u16,
    #[serde(default)]
    scope_id: u32,
    #[serde(default = "one")]
    tx_lanes: usize,
    #[serde(default = "outer_payload_limit")]
    outer_payload_limit: usize,
    path_group: Option<String>,
    #[serde(default)]
    resolver: ResolverConfig,
    #[serde(default = "refresh")]
    refresh_seconds: u64,
}

const fn one() -> usize {
    1
}
const fn outer_payload_limit() -> usize {
    unquic::socket::MAX_UDP_PAYLOAD
}
const fn refresh() -> u64 {
    60
}

#[derive(Clone, Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase", deny_unknown_fields)]
pub enum ResolverConfig {
    System {},
    Static {
        nameservers: Vec<SocketAddr>,
        bind_device: Option<String>,
    },
    Ra {
        interface: String,
    },
    Dhcpv6 {
        interface: String,
        client_mac: Option<MacAddress>,
    },
    Ipv6 {
        interface: String,
        client_mac: Option<MacAddress>,
    },
}

impl Default for ResolverConfig {
    fn default() -> Self {
        Self::System {}
    }
}

impl ResolverConfig {
    fn validate(&self) -> io::Result<()> {
        let interface = match self {
            Self::System {} => None,
            Self::Static {
                nameservers,
                bind_device,
            } => {
                if nameservers.is_empty()
                    || nameservers
                        .iter()
                        .any(|s| s.port() == 0 || s.ip().is_unspecified() || s.ip().is_multicast())
                {
                    return Err(invalid(
                        "resolver requires unicast nameservers with nonzero ports",
                    ));
                }
                bind_device.as_deref()
            }
            Self::Ra { interface }
            | Self::Dhcpv6 { interface, .. }
            | Self::Ipv6 { interface, .. } => Some(interface.as_str()),
        };
        if let Some(name) = interface
            && (name.is_empty()
                || name.len() >= libc::IFNAMSIZ
                || name == "."
                || name == ".."
                || name
                    .bytes()
                    .any(|b| b == 0 || b == b'/' || b == b':' || b.is_ascii_whitespace()))
        {
            return Err(invalid("invalid resolver interface name"));
        }
        Ok(())
    }

    pub fn build(&self) -> io::Result<Resolver> {
        let client = |mac: &Option<MacAddress>| {
            mac.map_or(ClientId::None, |m| ClientId::DuidLl(*m.as_bytes()))
        };
        Ok(match self {
            Self::System {} => Resolver::System,
            Self::Static {
                nameservers,
                bind_device,
            } => {
                let mut config = StaticResolverConfig::new(nameservers.iter().copied());
                if let Some(device) = bind_device {
                    config = config.with_bind_device(BindDevice::Interface(device.clone()));
                }
                Resolver::Static(config)
            }
            Self::Ra { interface } => Resolver::Ra(RaResolverConfig::new(interface.clone())?),
            Self::Dhcpv6 {
                interface,
                client_mac,
            } => Resolver::Dhcpv6(Dhcpv6ResolverConfig::new(
                interface.clone(),
                client(client_mac),
            )?),
            Self::Ipv6 {
                interface,
                client_mac,
            } => Resolver::Ipv6(Ipv6ResolverConfig::new(
                interface.clone(),
                client(client_mac),
            )?),
        })
    }
}

#[derive(Clone)]
pub struct Lookup {
    pub peer: usize,
    pub host: IpHost,
    pub resolver: ResolverConfig,
    pub scope_id: u32,
    pub refresh: Duration,
}

pub struct Loaded {
    pub config: Config,
    pub lookups: Vec<Lookup>,
}

impl Loaded {
    pub fn parse(text: &str) -> io::Result<Self> {
        // TOML diagnostics may include source text and cookies; report only its position.
        let raw: File = toml::from_str(text).map_err(|e: toml::de::Error| {
            invalid(format!(
                "invalid TOML configuration near byte {}",
                e.span().map_or(0, |s| s.start)
            ))
        })?;
        let mut peers = Vec::with_capacity(raw.peers.len());
        let mut lookups = Vec::new();
        for (index, peer) in raw.peers.into_iter().enumerate() {
            peer.resolver.validate()?;
            if !(1..=86400).contains(&peer.refresh_seconds) {
                return Err(invalid("refresh_seconds must be from 1 through 86400"));
            }
            let address: Option<Ipv6Addr> = match peer.host {
                IpHost::V6Addr(address) => Some(address),
                IpHost::V4Addr(_) => return Err(invalid("peer host requires IPv6 or a hostname")),
                host @ IpHost::Fqdn(_) => {
                    lookups.push(Lookup {
                        peer: index,
                        host,
                        resolver: peer.resolver,
                        scope_id: peer.scope_id,
                        refresh: Duration::from_secs(peer.refresh_seconds),
                    });
                    None
                }
            };
            peers.push(Peer {
                name: peer.name,
                endpoint: peer.endpoint,
                address,
                peer_port: peer.peer_port,
                scope_id: peer.scope_id,
                tx_lanes: peer.tx_lanes,
                outer_payload_limit: peer.outer_payload_limit,
                path_group: peer.path_group,
            });
        }
        Ok(Self {
            config: Config {
                daemon: raw.daemon,
                endpoints: raw.endpoints,
                peers,
                sessions: raw.sessions,
            },
            lookups,
        })
    }
}

fn invalid(message: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    const COOKIE: &str = "00112233-4455-4677-8899-aabbccddeeff";

    fn example(document: &str) -> String {
        document
            .split("```toml\n")
            .nth(1)
            .unwrap()
            .split("```")
            .next()
            .unwrap()
            .replace("<uuid-v4>", COOKIE)
    }

    fn sample() -> String {
        example(include_str!("../../../config-model.md"))
    }

    #[test]
    fn documented_examples_parse_and_validate_without_resolution() {
        for document in [
            include_str!("../../../config-model.md"),
            include_str!("../../../initial-design.md"),
        ] {
            let loaded = Loaded::parse(&example(document)).unwrap();
            assert_eq!(loaded.lookups.len(), 1);
            assert!(matches!(loaded.lookups[0].host, IpHost::Fqdn(_)));
            assert!(matches!(
                loaded.lookups[0].resolver,
                ResolverConfig::System {}
            ));
            assert_eq!(loaded.lookups[0].refresh, Duration::from_secs(60));
            assert_eq!(loaded.config.peers[0].address, None);
            assert_eq!(
                loaded.config.peers[0].outer_payload_limit,
                unquic::socket::MAX_UDP_PAYLOAD
            );
            assert_eq!(loaded.config.sessions[0].queue_count, 1);
            assert_eq!(loaded.config.daemon.memory_limit, 256 * 1024 * 1024);
            assert!(loaded.config.daemon.tolerate_random_loss);
            assert_eq!(loaded.config.daemon.loss_threshold_permille, 20);
            assert!(!loaded.config.daemon.balance_flows);
            assert_eq!(loaded.config.daemon.tx_hold_us, 0);
            assert!(!loaded.config.daemon.probe_rtt_ack_delay);
            loaded.config.validate().unwrap();
        }
        let text = format!("[daemon]\ntx_hold_us = 1000\n{}", sample());
        let loaded = Loaded::parse(&text).unwrap();
        assert_eq!(loaded.config.daemon.tx_hold_us, 1000);
        loaded.config.validate().unwrap();
        let text = format!("[daemon]\nprobe_rtt_ack_delay = true\n{}", sample());
        let loaded = Loaded::parse(&text).unwrap();
        assert!(loaded.config.daemon.probe_rtt_ack_delay);
        loaded.config.validate().unwrap();
        let text = format!("[daemon]\ntolerate_random_loss = false\n{}", sample());
        let loaded = Loaded::parse(&text).unwrap();
        assert!(!loaded.config.daemon.tolerate_random_loss);
        assert_eq!(loaded.config.daemon.memory_limit, 256 * 1024 * 1024);
        let text = format!("[daemon]\nloss_threshold_permille = 100\n{}", sample());
        let loaded = Loaded::parse(&text).unwrap();
        assert_eq!(loaded.config.daemon.loss_threshold_permille, 100);
        assert!(loaded.config.daemon.tolerate_random_loss);
        loaded.config.validate().unwrap();
        let text = sample().replace("test.example.invalid", "2001:db8::1");
        let loaded = Loaded::parse(&text).unwrap();
        assert!(loaded.lookups.is_empty());
        assert_eq!(
            loaded.config.peers[0].address,
            Some("2001:db8::1".parse().unwrap())
        );
        loaded.config.validate().unwrap();
    }

    #[test]
    fn configured_outer_limit_reaches_the_core() {
        for limit in [1232, 1300, 1452, 8952] {
            let text = sample().replace(
                "tx_lanes = 4",
                &format!("tx_lanes = 4\nouter_payload_limit = {limit}"),
            );
            let config = Loaded::parse(&text).unwrap().config.validate().unwrap();
            assert_eq!(config.config().peers[0].outer_payload_limit, limit);
        }
    }

    #[test]
    fn parser_rejects_typos_and_never_echoes_source_or_cookie() {
        let text = sample();
        for bad in [
            text.replace("listen_port", "listen_prt"),
            text.replace("peer_port", "peer_prt"),
            text.replace("inner_mtu", "inner_mt"),
            text.replace(COOKIE, "secret-invalid-cookie"),
            text.replace("02:00:00:00:00:01", "01:00:00:00:00:01"),
            text.replace("test.example.invalid", "127.0.0.1"),
            text.replace("tx_lanes = 4", "tx_lanes = 4\nrefresh_seconds = 0"),
            text.replace("tx_lanes = 4", "tx_lanes = 4\nrefresh_seconds = 86401"),
            format!("{text}\nunknown = '{COOKIE}'"),
        ] {
            let error = Loaded::parse(&bad).err().expect("must reject");
            assert!(!error.to_string().contains(COOKIE));
            assert!(!error.to_string().contains("secret-invalid-cookie"));
        }
    }

    #[test]
    fn parsed_fields_reach_every_core_validation_constraint() {
        let text = sample();
        for (from, to, field) in [
            ("name = \"default\"", "name = \"\"", "endpoints.name"),
            (
                "listen_port = 40000",
                "listen_port = 0",
                "endpoints.listen_port",
            ),
            (
                "listen_address = \"::\"",
                "listen_address = \"ff02::1\"",
                "endpoints.listen_address",
            ),
            (
                "listen_address = \"::\"",
                "listen_address = \"fe80::1\"",
                "endpoints.listen_address",
            ),
            (
                "bind_device = \"vrf-ngn\"",
                "bind_device = \"bad/name\"",
                "endpoints.bind_device",
            ),
            (
                "endpoint = \"default\"",
                "endpoint = \"missing\"",
                "peers.endpoint",
            ),
            ("peer_port = 40000", "peer_port = 0", "peers.peer_port"),
            ("test.example.invalid", "::", "peers.address"),
            ("test.example.invalid", "fe80::1", "peers.address"),
            ("test.example.invalid", "::ffff:192.0.2.1", "peers.address"),
            (
                "tx_lanes = 4",
                "tx_lanes = 4\nouter_payload_limit = 1231",
                "peers.outer_payload_limit",
            ),
            (
                "tx_lanes = 4",
                "tx_lanes = 4\nouter_payload_limit = 8953",
                "peers.outer_payload_limit",
            ),
            ("tx_lanes = 4", "tx_lanes = 0", "peers.tx_lanes"),
            ("tx_lanes = 4", "tx_lanes = 65", "peers.tx_lanes"),
            (
                "tx_lanes = 4",
                "tx_lanes = 4\npath_group = \"\"",
                "peers.path_group",
            ),
            ("peer = \"peer-a\"", "peer = \"missing\"", "sessions.peer"),
            (
                "tap_name = \"unquic0\"",
                "tap_name = \"uq%d\"",
                "sessions.tap_name",
            ),
            ("inner_mtu = 2400", "inner_mtu = 1499", "sessions.inner_mtu"),
            ("inner_mtu = 2400", "inner_mtu = 9001", "sessions.inner_mtu"),
            (
                "inner_mtu = 2400",
                "inner_mtu = 2400\nqueue_count = 0",
                "sessions.queue_count",
            ),
        ] {
            let config = Loaded::parse(&text.replace(from, to)).unwrap().config;
            assert_eq!(config.validate().err().unwrap().field, field, "{field}");
        }
        for (setting, field) in [
            ("memory_limit = 0", "daemon.memory_limit"),
            (
                "loss_threshold_permille = 19",
                "daemon.loss_threshold_permille",
            ),
            (
                "loss_threshold_permille = 201",
                "daemon.loss_threshold_permille",
            ),
            ("tx_hold_us = 1001", "daemon.tx_hold_us"),
        ] {
            let bad = format!("[daemon]\n{setting}\n{text}");
            assert_eq!(
                Loaded::parse(&bad)
                    .unwrap()
                    .config
                    .validate()
                    .err()
                    .unwrap()
                    .field,
                field
            );
        }
        let loaded = Loaded::parse(&text).unwrap();
        let mut config = loaded.config;
        config.sessions.push(config.sessions[0].clone());
        config.sessions[1].name = "second".into();
        config.sessions[1].tap_name = Some("uq-second".into());
        assert_eq!(config.validate().err().unwrap().field, "sessions.cookie");
    }

    #[test]
    fn resolver_selection_is_explicit_and_validated_without_io() {
        for (mode, fields) in [
            ("system", ""),
            ("static", "nameservers = ['[::1]:5353']\nbind_device = 'lo'"),
            ("ra", "interface = 'eth0'"),
            (
                "dhcpv6",
                "interface = 'eth0'\nclient_mac = '02:00:00:00:00:01'",
            ),
            (
                "ipv6",
                "interface = 'eth0'\nclient_mac = '02:00:00:00:00:01'",
            ),
        ] {
            let text = sample().replace(
                "[[sessions]]",
                &format!("[peers.resolver]\nmode = '{mode}'\n{fields}\n\n[[sessions]]"),
            );
            let loaded = Loaded::parse(&text).unwrap();
            let selected = &loaded.lookups[0].resolver;
            match mode {
                "system" => assert_eq!(selected.build().unwrap(), Resolver::System),
                "static" => {
                    let Resolver::Static(config) = selected.build().unwrap() else {
                        panic!()
                    };
                    assert_eq!(config.nameservers(), &["[::1]:5353".parse().unwrap()]);
                    assert_eq!(
                        config.bind_device(),
                        Some(&BindDevice::Interface("lo".into()))
                    );
                }
                "ra" => assert!(matches!(selected, ResolverConfig::Ra { .. })),
                "dhcpv6" => assert!(matches!(
                    selected,
                    ResolverConfig::Dhcpv6 {
                        client_mac: Some(_),
                        ..
                    }
                )),
                "ipv6" => assert!(matches!(
                    selected,
                    ResolverConfig::Ipv6 {
                        client_mac: Some(_),
                        ..
                    }
                )),
                _ => unreachable!(),
            }
        }
        for fields in [
            "mode = 'unknown'",
            "mode = 'static'\nnameservers = []",
            "mode = 'static'\nnameservers = ['[::]:53']",
            "mode = 'static'\nnameservers = ['[::1]:0']",
            "mode = 'ra'\ninterface = 'bad/name'",
            "mode = 'dhcpv6'",
            "mode = 'system'\ninterface = 'eth0'",
        ] {
            let text = sample().replace(
                "[[sessions]]",
                &format!("[peers.resolver]\n{fields}\n[[sessions]]"),
            );
            assert!(Loaded::parse(&text).is_err(), "{fields}");
        }
    }
}
