//! Raw IPv6 transport configuration and startup-only name resolution.
//!
//! Named endpoints may serve many peers; each peer may serve many sessions.
//! Validation consumes the configuration and builds immutable numeric indices.
//! DNS lives in the optional resolution layer, outside this module and workers.

use crate::{MacAddress, UnquicUuid, uuid::CookieIndex};
use std::{
    collections::BTreeMap,
    ffi::CString,
    fmt,
    net::{Ipv6Addr, SocketAddrV6},
};

/// Parsed configuration, supplied explicitly by a caller.
#[derive(Clone, Debug, Default)]
pub struct Config {
    /// Daemon-wide resource settings.
    pub daemon: Daemon,
    /// Named local receive endpoints.
    pub endpoints: Vec<Endpoint>,
    /// Transmit destinations, each referencing one endpoint.
    pub peers: Vec<Peer>,
    /// Ethernet sessions, each referencing one peer.
    pub sessions: Vec<Session>,
}

/// Daemon-wide resource limits.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "bin", derive(serde::Deserialize))]
#[cfg_attr(feature = "bin", serde(deny_unknown_fields))]
#[cfg_attr(feature = "bin", serde(default))]
pub struct Daemon {
    /// Total transport storage ceiling, including metadata and queues.
    pub memory_limit: usize,
    /// Deviation from BBR draft 07, off by default: outside bandwidth probing,
    /// cut the short-term bounds only after a loss round above 2% loss.
    pub tolerate_random_loss: bool,
    /// Off by default: pin each new flow to the next lane round-robin instead
    /// of its hash lane, so a few heavy flows use separate transmit workers.
    pub balance_flows: bool,
}
impl Default for Daemon {
    fn default() -> Self {
        Self {
            memory_limit: 256 * 1024 * 1024,
            tolerate_random_loss: false,
            balance_flows: false,
        }
    }
}

/// A fixed-port receive endpoint; source addresses are not admission filters.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "bin", derive(serde::Deserialize))]
#[cfg_attr(feature = "bin", serde(deny_unknown_fields))]
pub struct Endpoint {
    /// Unique local endpoint name.
    pub name: String,
    /// Local IPv6 address; unspecified binds all addresses in this scope.
    pub listen_address: Ipv6Addr,
    /// Fixed nonzero receive-only port.
    pub listen_port: u16,
    /// Numeric interface index for a link-local listen address; otherwise zero.
    #[cfg_attr(feature = "bin", serde(default))]
    pub scope_id: u32,
    /// Optional interface or VRF binding.
    pub bind_device: Option<CString>,
}

impl Endpoint {
    /// Socket address to bind, without DNS or interface-name lookup.
    pub const fn socket_addr(&self) -> SocketAddrV6 {
        SocketAddrV6::new(self.listen_address, self.listen_port, 0, self.scope_id)
    }
}

/// One transmit destination, shared by any number of sessions.
///
/// No hostname is stored here. A resolution helper can supply an initial IPv6
/// address or later candidate; None leaves transmission waiting without delaying
/// TAP creation. Receive admission never compares a packet's source with this.
#[derive(Clone, Debug)]
pub struct Peer {
    /// Unique local peer name.
    pub name: String,
    /// Name of the local receive endpoint and transmit binding scope.
    pub endpoint: String,
    /// Initial raw IPv6 transmit destination, or None until supplied.
    pub address: Option<Ipv6Addr>,
    /// Remote fixed receive port; never an observed source port.
    pub peer_port: u16,
    /// Numeric interface index for link-local destinations; otherwise zero.
    pub scope_id: u32,
    /// Number of local transmit lanes, from 1 through 64.
    pub tx_lanes: usize,
    /// Discovery ceiling in UDP payload bytes, 1232..=8952; DATA starts at 1232.
    pub outer_payload_limit: usize,
    /// Explicit congestion group, or None to share the endpoint's binding scope.
    pub path_group: Option<String>,
}

impl Peer {
    /// Initial transmit address, without resolving names or opening sockets.
    pub const fn socket_addr(&self) -> Option<SocketAddrV6> {
        match self.address {
            Some(ip) => Some(SocketAddrV6::new(ip, self.peer_port, 0, self.scope_id)),
            None => None,
        }
    }
}

/// One Ethernet session. Handshake roles are elected, never configured.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "bin", derive(serde::Deserialize))]
#[cfg_attr(feature = "bin", serde(deny_unknown_fields))]
pub struct Session {
    /// Unique local session name.
    pub name: String,
    /// Name of the peer carrying this session.
    pub peer: String,
    /// Explicitly provisioned admission cookie, unique across the daemon.
    pub cookie: UnquicUuid,
    /// TAP name, or None to use the session name.
    pub tap_name: Option<String>,
    /// Inner IP MTU, from 1500 through 9000.
    pub inner_mtu: u16,
    /// Interface MAC, or None for deterministic hostname/name derivation.
    pub mac: Option<MacAddress>,
    /// Number of TAP queues, from 1 through 64.
    #[cfg_attr(feature = "bin", serde(default = "one_queue"))]
    pub queue_count: usize,
}

#[cfg(feature = "bin")]
const fn one_queue() -> usize {
    1
}

impl Session {
    /// Effective TAP name; this does not allocate.
    pub fn tap_name(&self) -> &str {
        self.tap_name.as_deref().unwrap_or(&self.name)
    }
}

/// Validation failure. Diagnostics retain no cookie or user-provided value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigError {
    /// Configuration field, such as peers.endpoint.
    pub field: &'static str,
    /// Zero-based entry index, or None for a daemon-wide constraint.
    pub index: Option<usize>,
    /// Fixed description of the violated constraint.
    pub reason: &'static str,
}
impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(index) = self.index {
            write!(f, "{} entry {}: {}", self.field, index, self.reason)
        } else {
            write!(f, "{}: {}", self.field, self.reason)
        }
    }
}
impl std::error::Error for ConfigError {}

/// Immutable configuration with names resolved once to numeric indices.
///
/// Construction allocates; all lookups are allocation-free and lock-free.
pub struct Validated {
    config: Config,
    endpoint_for_peer: Vec<usize>,
    peer_for_session: Vec<usize>,
    path_group_for_peer: Vec<usize>,
    path_groups: usize,
    cookies: CookieIndex,
}

impl Validated {
    /// Validated configuration, immutable for the lifetime of the indices.
    pub const fn config(&self) -> &Config {
        &self.config
    }

    /// Resolves a peer index to its endpoint index in constant time.
    #[inline]
    pub fn endpoint_for_peer(&self, peer: usize) -> Option<usize> {
        self.endpoint_for_peer.get(peer).copied()
    }

    /// Number of independent sending-direction path groups.
    pub const fn path_groups(&self) -> usize {
        self.path_groups
    }

    /// Resolves a peer to its stable path-group index without allocation.
    ///
    /// Named groups share across endpoints. Default groups share an exact local
    /// address, scope ID and device binding, regardless of ports or peer names.
    #[inline]
    pub fn path_group_for_peer(&self, peer: usize) -> Option<usize> {
        self.path_group_for_peer.get(peer).copied()
    }

    /// Resolves a session index to its peer index in constant time.
    #[inline]
    pub fn peer_for_session(&self, session: usize) -> Option<usize> {
        self.peer_for_session.get(session).copied()
    }

    /// Resolves a cookie to its session index without allocating unknown state.
    #[inline]
    pub fn session_for_cookie(&self, cookie: UnquicUuid) -> Option<usize> {
        self.cookies.find(cookie)
    }
}

impl Config {
    /// Validates all entries and freezes their name/index relationships.
    ///
    /// This performs no file, environment, socket or DNS operations. Fixed
    /// runtime allocations must also be checked against memory_limit at startup.
    pub fn validate(self) -> Result<Validated, ConfigError> {
        if self.daemon.memory_limit == 0 {
            return Err(ConfigError {
                field: "daemon.memory_limit",
                index: None,
                reason: "must be nonzero",
            });
        }
        let endpoints = names(
            self.endpoints.iter().map(|e| e.name.as_str()),
            "endpoints.name",
        )?;
        let peers = names(self.peers.iter().map(|p| p.name.as_str()), "peers.name")?;
        names(
            self.sessions.iter().map(|s| s.name.as_str()),
            "sessions.name",
        )?;
        for (index, endpoint) in self.endpoints.iter().enumerate() {
            if endpoint.listen_port == 0 {
                return Err(invalid("endpoints.listen_port", index, "must be nonzero"));
            }
            if !valid_address(endpoint.listen_address, endpoint.scope_id, true) {
                return Err(invalid(
                    "endpoints.listen_address",
                    index,
                    "requires IPv6 unicast or unspecified; link-local requires scope_id",
                ));
            }
            if let Some(device) = &endpoint.bind_device
                && !interface_name(device.to_bytes())
            {
                return Err(invalid(
                    "endpoints.bind_device",
                    index,
                    "invalid interface name",
                ));
            }
            if self.endpoints[..index].iter().any(|previous| {
                previous.listen_port == endpoint.listen_port
                    && previous.bind_device == endpoint.bind_device
                    && previous.scope_id == endpoint.scope_id
                    && (previous.listen_address == endpoint.listen_address
                        || previous.listen_address.is_unspecified()
                        || endpoint.listen_address.is_unspecified())
            }) {
                return Err(invalid(
                    "endpoints.listen_address",
                    index,
                    "overlapping receive endpoints in the same binding scope",
                ));
            }
        }
        let mut endpoint_for_peer = Vec::with_capacity(self.peers.len());
        for (index, peer) in self.peers.iter().enumerate() {
            let endpoint = endpoints
                .get(peer.endpoint.as_str())
                .ok_or_else(|| invalid("peers.endpoint", index, "unknown endpoint"))?;
            endpoint_for_peer.push(*endpoint);
            if peer.peer_port == 0 {
                return Err(invalid("peers.peer_port", index, "must be nonzero"));
            }
            if let Some(ip) = peer.address
                && !valid_address(ip, peer.scope_id, false)
            {
                return Err(invalid(
                    "peers.address",
                    index,
                    "requires IPv6 unicast; link-local requires scope_id",
                ));
            }
            if !(1..=64).contains(&peer.tx_lanes) {
                return Err(invalid(
                    "peers.tx_lanes",
                    index,
                    "must be from 1 through 64",
                ));
            }
            if !(crate::BASE_UDP_PAYLOAD..=crate::socket::MAX_UDP_PAYLOAD)
                .contains(&peer.outer_payload_limit)
            {
                return Err(invalid(
                    "peers.outer_payload_limit",
                    index,
                    "must be from 1232 through 8952",
                ));
            }
            if peer
                .path_group
                .as_deref()
                .is_some_and(|name| !valid_name(name))
            {
                return Err(invalid(
                    "peers.path_group",
                    index,
                    "must be nonempty without NUL",
                ));
            }
        }
        #[derive(PartialEq, Eq, PartialOrd, Ord)]
        enum Group<'a> {
            Named(&'a str),
            Binding(Ipv6Addr, u32, Option<&'a std::ffi::CStr>),
        }
        let mut groups = BTreeMap::new();
        let path_group_for_peer = self
            .peers
            .iter()
            .enumerate()
            .map(|(index, peer)| {
                let endpoint = &self.endpoints[endpoint_for_peer[index]];
                let key = match peer.path_group.as_deref() {
                    Some(name) => Group::Named(name),
                    None => Group::Binding(
                        endpoint.listen_address,
                        endpoint.scope_id,
                        endpoint.bind_device.as_deref(),
                    ),
                };
                let next = groups.len();
                *groups.entry(key).or_insert(next)
            })
            .collect();
        let path_groups = groups.len();
        let mut peer_for_session = Vec::with_capacity(self.sessions.len());
        let mut taps = BTreeMap::new();
        for (index, session) in self.sessions.iter().enumerate() {
            let peer = peers
                .get(session.peer.as_str())
                .ok_or_else(|| invalid("sessions.peer", index, "unknown peer"))?;
            peer_for_session.push(*peer);
            let tap = session.tap_name();
            if !interface_name(tap.as_bytes()) || tap.contains('%') {
                return Err(invalid(
                    "sessions.tap_name",
                    index,
                    "invalid TAP name or automatic-name pattern",
                ));
            }
            if taps.insert(tap, index).is_some() {
                return Err(invalid(
                    "sessions.tap_name",
                    index,
                    "duplicate effective TAP name",
                ));
            }
            if !(1500..=9000).contains(&session.inner_mtu) {
                return Err(invalid(
                    "sessions.inner_mtu",
                    index,
                    "must be from 1500 through 9000",
                ));
            }
            if !(1..=64).contains(&session.queue_count) {
                return Err(invalid(
                    "sessions.queue_count",
                    index,
                    "must be from 1 through 64",
                ));
            }
        }
        let cookies =
            CookieIndex::new(self.sessions.iter().map(|s| s.cookie)).map_err(|_| ConfigError {
                field: "sessions.cookie",
                index: None,
                reason: "duplicate session cookie",
            })?;
        Ok(Validated {
            config: self,
            endpoint_for_peer,
            peer_for_session,
            path_group_for_peer,
            path_groups,
            cookies,
        })
    }
}

pub(crate) const fn unicast(ip: Ipv6Addr) -> bool {
    !ip.is_unspecified() && !ip.is_multicast() && ip.to_ipv4_mapped().is_none()
}

fn valid_address(ip: Ipv6Addr, scope: u32, allow_unspecified: bool) -> bool {
    (unicast(ip) || (allow_unspecified && ip.is_unspecified()))
        && (!ip.is_unicast_link_local() || scope != 0)
}

fn interface_name(name: &[u8]) -> bool {
    !name.is_empty()
        && name.len() < libc::IFNAMSIZ
        && name != b"."
        && name != b".."
        && !name
            .iter()
            .any(|b| *b == 0 || *b == b'/' || *b == b':' || b.is_ascii_whitespace())
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && !name.contains('\0')
}

fn names<'a>(
    values: impl Iterator<Item = &'a str>,
    field: &'static str,
) -> Result<BTreeMap<&'a str, usize>, ConfigError> {
    let mut names = BTreeMap::new();
    for (index, name) in values.enumerate() {
        if !valid_name(name) {
            return Err(invalid(field, index, "must be nonempty without NUL"));
        }
        if names.insert(name, index).is_some() {
            return Err(invalid(field, index, "duplicate name"));
        }
    }
    Ok(names)
}

fn invalid(field: &'static str, index: usize, reason: &'static str) -> ConfigError {
    ConfigError {
        field,
        index: Some(index),
        reason,
    }
}
