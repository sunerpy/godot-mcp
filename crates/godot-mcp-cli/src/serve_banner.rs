//! What `godot-mcp serve` prints on stderr once an `open` HTTP listener is up
//! (ADR 0014): the URL a client on this host dials, the one another host on the
//! network dials, and a warning when anyone who can reach the port can use it.
//!
//! The lines are decided from the address the listener actually bound. A
//! wildcard bind (`0.0.0.0`, `::`) is named as an address, never as a URL, and
//! followed by the URLs a client dials, since no client can connect to the
//! wildcard itself.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};

/// The path every HTTP listener serves MCP at.
const MCP_PATH: &str = "/mcp";

/// What an open listener enforces, as the banner reports it.
#[derive(Debug, Clone)]
pub struct OpenListener {
    /// The address the listener bound.
    pub bound: SocketAddr,
    /// Whether every request must present the configured token.
    pub token: bool,
    /// The capability profile it serves.
    pub profile: &'static str,
    /// Whether the script tools run caller-written GDScript.
    pub allow_scripts: bool,
}

fn url(address: SocketAddr) -> String {
    format!("http://{address}{MCP_PATH}")
}

/// The lines to print, in order.
///
/// `route` names the address this host sends from to reach another host of the
/// bound address family, or `None` when it has no such route; [`route_probe`]
/// is the production value.
#[must_use]
pub fn lines(listener: &OpenListener, route: impl Fn(IpAddr) -> Option<IpAddr>) -> Vec<String> {
    let ip = listener.bound.ip();
    let port = listener.bound.port();
    let mut lines = vec![if ip.is_unspecified() {
        format!(
            "godot-mcp serve: HTTP (open) listening on {}, every interface",
            listener.bound
        )
    } else {
        format!(
            "godot-mcp serve: HTTP (open) listening at {}",
            url(listener.bound)
        )
    }];
    if ip.is_unspecified() {
        let this_host = match ip {
            IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
        };
        lines.push(format!(
            "godot-mcp serve: this host: {}",
            url(SocketAddr::new(this_host, port))
        ));
        let other_hosts = match route(ip) {
            Some(address) => url(SocketAddr::new(address, port)),
            None => format!("http://<this host's LAN address>:{port}{MCP_PATH}"),
        };
        lines.push(format!("godot-mcp serve: other hosts: {other_hosts}"));
    }
    if !ip.is_loopback() && !listener.token {
        lines.push(format!(
            "godot-mcp serve: warning: no token; anyone who can reach this port can use every \
             tool of the {} profile",
            listener.profile
        ));
        if listener.allow_scripts {
            lines.push(
                "godot-mcp serve: warning: script execution is on, so they can also run any \
                 GDScript with this user's privileges"
                    .to_owned(),
            );
        }
    }
    lines
}

/// The address this host sends from to reach another host of `family`'s
/// address family: the local address of a UDP socket connected to a
/// documentation address, `192.0.2.1` (RFC 5737) or `2001:db8::1` (RFC 3849).
///
/// Connecting a UDP socket sends no packet; the operating system only selects
/// the route. `None` when there is no route, or the route leaves through
/// loopback.
#[must_use]
pub fn route_probe(family: IpAddr) -> Option<IpAddr> {
    let (bind, peer) = match family {
        IpAddr::V4(_) => (
            SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)), 9),
        ),
        IpAddr::V6(_) => (
            SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
            SocketAddr::new(
                IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1)),
                9,
            ),
        ),
    };
    let socket = UdpSocket::bind(bind).ok()?;
    socket.connect(peer).ok()?;
    let local = socket.local_addr().ok()?.ip();
    (!local.is_unspecified() && !local.is_loopback()).then_some(local)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listener(bound: &str, token: bool, allow_scripts: bool) -> OpenListener {
        OpenListener {
            bound: bound.parse().expect("socket address"),
            token,
            profile: "full",
            allow_scripts,
        }
    }

    const WARNING: &str = "godot-mcp serve: warning: no token; anyone who can reach this port \
                           can use every tool of the full profile";
    const SCRIPTS: &str = "godot-mcp serve: warning: script execution is on, so they can also \
                           run any GDScript with this user's privileges";

    #[test]
    fn a_wildcard_bind_names_this_host_and_the_lan_address() {
        let lan: IpAddr = "192.168.1.20".parse().expect("address");
        let lines = lines(&listener("0.0.0.0:8765", false, false), |family| {
            assert!(family.is_ipv4());
            Some(lan)
        });
        assert_eq!(
            lines,
            [
                "godot-mcp serve: HTTP (open) listening on 0.0.0.0:8765, every interface",
                "godot-mcp serve: this host: http://127.0.0.1:8765/mcp",
                "godot-mcp serve: other hosts: http://192.168.1.20:8765/mcp",
                WARNING,
            ]
        );
    }

    #[test]
    fn an_ipv6_wildcard_uses_the_ipv6_loopback_and_route() {
        let lan: IpAddr = "fd00::20".parse().expect("address");
        let lines = lines(&listener("[::]:8765", true, true), |family| {
            assert!(family.is_ipv6());
            Some(lan)
        });
        assert_eq!(
            lines,
            [
                "godot-mcp serve: HTTP (open) listening on [::]:8765, every interface",
                "godot-mcp serve: this host: http://[::1]:8765/mcp",
                "godot-mcp serve: other hosts: http://[fd00::20]:8765/mcp",
            ],
            "a token silences the warning, whatever the script setting"
        );
    }

    #[test]
    fn without_a_route_the_other_hosts_line_says_what_to_fill_in() {
        let lines = lines(&listener("0.0.0.0:9000", true, false), |_| None);
        assert_eq!(
            lines[2],
            "godot-mcp serve: other hosts: http://<this host's LAN address>:9000/mcp"
        );
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn a_specific_address_is_named_as_it_is() {
        let never = |_: IpAddr| -> Option<IpAddr> { panic!("a specific bind needs no route") };
        assert_eq!(
            lines(&listener("192.168.1.20:8765", false, true), never),
            [
                "godot-mcp serve: HTTP (open) listening at http://192.168.1.20:8765/mcp",
                WARNING,
                SCRIPTS,
            ]
        );
        // Only processes on this host reach a loopback listener: no warning.
        assert_eq!(
            lines(&listener("127.0.0.1:8765", false, true), never),
            ["godot-mcp serve: HTTP (open) listening at http://127.0.0.1:8765/mcp"]
        );
        assert_eq!(lines(&listener("[::1]:8765", false, false), never).len(), 1);
    }

    #[test]
    fn the_route_probe_never_answers_a_wildcard_or_loopback() {
        for family in [
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            IpAddr::V6(Ipv6Addr::UNSPECIFIED),
        ] {
            if let Some(address) = route_probe(family) {
                assert!(
                    !address.is_unspecified() && !address.is_loopback(),
                    "{address}"
                );
                assert_eq!(address.is_ipv4(), family.is_ipv4(), "{address}");
            }
        }
    }
}
