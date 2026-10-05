//! `node:dns` backing ops: name resolution through the system resolver (`getaddrinfo` /
//! `getnameinfo`), run on the runtime's blocking pool since resolution blocks on the network.
//! Record-type queries are made by the JS client (`js/net.js`) over a dgram socket.

use std::net::IpAddr;
#[cfg(unix)]
use std::net::SocketAddr;
#[cfg(not(unix))]
use std::net::ToSocketAddrs;

use lumen::embed::SendError;
use lumen_bind::Data;

pub(crate) use bindings::Module;

#[lumen_bind::module(name = "__dns")]
mod bindings {
    use super::*;

    /// `getaddrinfo(3)` with the caller's family and `AI_*` hints; resolves with
    /// `[[address, family], …]` in the resolver's order, or rejects with `{ code: "EAI_…" }`.
    #[op(async, coerce, name = "getaddrinfo")]
    fn op_getaddrinfo(hostname: String, family: f64, flags: f64) -> Result<Data, SendError> {
        match gai_lookup(&hostname, family as u8, flags as i32) {
            Ok(list) => Ok(Data::List(
                list.into_iter()
                    .map(|(address, family)| {
                        Data::List(vec![Data::Str(address), Data::Int(family as i64)])
                    })
                    .collect(),
            )),
            Err(code) => {
                Err(SendError::new("Error", format!("getaddrinfo {code}")).with_code(code))
            }
        }
    }

    /// `getnameinfo(3)`; resolves with `[hostname, service]` or rejects with `{ code: "EAI_…" }`.
    #[op(async, coerce, name = "getnameinfo")]
    fn op_getnameinfo(address: String, port: f64) -> Result<Data, SendError> {
        match name_info(&address, port as u16) {
            Ok((host, service)) => Ok(Data::List(vec![Data::Str(host), Data::Str(service)])),
            Err(code) => {
                Err(SendError::new("Error", format!("getnameinfo {code}")).with_code(code))
            }
        }
    }

    /// The nameserver addresses from `/etc/resolv.conf`, for `dns.getServers()` and the default
    /// `Resolver`'s initial server list. Reading the file is cheap, so this stays synchronous.
    #[op(name = "getServers")]
    fn op_get_servers() -> Vec<String> {
        resolv_nameservers()
            .into_iter()
            .map(|ip| ip.to_string())
            .collect()
    }
}

#[cfg(unix)]
fn gai_lookup(hostname: &str, family: u8, flags: i32) -> Result<Vec<(String, u8)>, &'static str> {
    use lumen_os::net::{gai_code_name, getaddrinfo, SockAddr};
    let family = match family {
        4 => libc::AF_INET,
        6 => libc::AF_INET6,
        _ => libc::AF_UNSPEC,
    };
    let list = getaddrinfo(Some(hostname), None, family, libc::SOCK_STREAM, 0, flags)
        .map_err(|e| gai_code_name(e.code))?;
    let out: Vec<(String, u8)> = list
        .into_iter()
        .filter_map(|ai| match ai.addr {
            SockAddr::V4(a) => Some((a.ip().to_string(), 4)),
            SockAddr::V6(a) => Some((a.ip().to_string(), 6)),
            _ => None,
        })
        .collect();
    if out.is_empty() {
        return Err("EAI_NONAME");
    }
    Ok(out)
}

/// Resolve through `ToSocketAddrs`, keeping only the requested family (0 = both).
#[cfg(not(unix))]
fn gai_lookup(hostname: &str, family: u8, _flags: i32) -> Result<Vec<(String, u8)>, &'static str> {
    let addrs = (hostname, 0u16)
        .to_socket_addrs()
        .map_err(|_| "EAI_NONAME")?;
    let out: Vec<(String, u8)> = addrs
        .map(|a| (a.ip().to_string(), if a.is_ipv6() { 6 } else { 4 }))
        .filter(|(_, fam)| family == 0 || family == *fam)
        .collect();
    if out.is_empty() {
        return Err("EAI_NONAME");
    }
    Ok(out)
}

#[cfg(unix)]
fn name_info(address: &str, port: u16) -> Result<(String, String), &'static str> {
    use lumen_os::net::{gai_code_name, getnameinfo, SockAddr};
    let ip: IpAddr = address.parse().map_err(|_| "EAI_NONAME")?;
    let addr = match SocketAddr::new(ip, port) {
        SocketAddr::V4(a) => SockAddr::V4(a),
        SocketAddr::V6(a) => SockAddr::V6(a),
    };
    getnameinfo(&addr, 0).map_err(|e| gai_code_name(e.code))
}

#[cfg(not(unix))]
fn name_info(address: &str, port: u16) -> Result<(String, String), &'static str> {
    let ip: IpAddr = address.parse().map_err(|_| "EAI_NONAME")?;
    Ok((ip.to_string(), port.to_string()))
}

/// Nameserver addresses from `/etc/resolv.conf`.
fn resolv_nameservers() -> Vec<IpAddr> {
    let mut out = Vec::new();
    if let Ok(contents) = std::fs::read_to_string("/etc/resolv.conf") {
        for line in contents.lines() {
            if let Some(rest) = line.trim().strip_prefix("nameserver") {
                if let Ok(ip) = rest.trim().parse::<IpAddr>() {
                    out.push(ip);
                }
            }
        }
    }
    out
}
