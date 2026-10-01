//! `node:dns` backing ops — real name resolution, no stubs.
//!
//! Two layers, both off-thread on the runtime's blocking pool (DNS blocks on the network):
//!
//! - **`lookup` / `resolve4` / `resolve6`** go through `std::net::ToSocketAddrs`, i.e. the system
//!   resolver (`getaddrinfo`) — honoring `/etc/hosts`, search domains, and the platform's DNS
//!   configuration exactly as Node's own `dns.lookup` does.
//! - **`resolve<T>` / `reverse`** (record-type queries: A/AAAA/CNAME/NS/MX/TXT/PTR) speak DNS over
//!   UDP directly (RFC 1035) against the nameservers in `/etc/resolv.conf`, since `getaddrinfo`
//!   can't return record types other than addresses.
//!
//! All using only `std::net` — no third-party crate.

use std::net::{IpAddr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::Duration;

use lumen_host::{Ctx, Value};

use crate::spawn_handle;

/// A resolved resource record, in a shape the JS glue turns into the value Node's API returns.
enum Rec {
    /// A single string (an address for A/AAAA, a name for CNAME/NS/PTR).
    Str(String),
    /// An MX record.
    Mx { priority: u16, exchange: String },
    /// A TXT record: its ordered string chunks.
    Txt(Vec<String>),
}

// ---- lookup / resolve4 / resolve6 via the system resolver ------------------------------------

/// `__dns.lookup(hostname, family, resolve, reject)` — resolve a hostname to addresses through the
/// system resolver. `family` is 0 (any), 4, or 6; the glue picks one vs. all.
pub fn op_lookup(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let hostname = arg_str(ctx, args, 0)?;
    let family = arg_num(args, 1) as u8;
    let (resolve, reject) = settle_pair(ctx, args, 2, "__dns.lookup")?;
    let id = lumen_host::register_task(ctx, resolve, Some(reject), decode_addr_list);
    spawn_handle(ctx).spawn_blocking(id, move || Box::new(system_lookup(&hostname, family)));
    Ok(Value::Undefined)
}

/// Resolve via `getaddrinfo`, keeping only the requested family (0 = both). A trailing `:0`
/// port makes this a pure name lookup.
fn system_lookup(hostname: &str, family: u8) -> Result<Vec<(String, u8)>, String> {
    let addrs = (hostname, 0u16)
        .to_socket_addrs()
        .map_err(|e| format!("getaddrinfo {hostname}: {e}"))?;
    let mut out = Vec::new();
    for a in addrs {
        let fam = if a.is_ipv6() { 6 } else { 4 };
        if family != 0 && family != fam {
            continue;
        }
        out.push((a.ip().to_string(), fam));
    }
    if out.is_empty() {
        return Err(format!("queryA/AAAA {hostname}: no matching records"));
    }
    Ok(out)
}

/// Payload → `[{ address, family }, …]`.
fn decode_addr_list(
    ctx: &mut Ctx,
    payload: Box<dyn std::any::Any + Send>,
) -> Result<Vec<Value>, Value> {
    match *payload
        .downcast::<Result<Vec<(String, u8)>, String>>()
        .expect("lookup payload")
    {
        Ok(list) => {
            let items = list
                .into_iter()
                .map(|(address, family)| {
                    let o = Value::Obj(ctx.new_object());
                    let _ = ctx.member_set(&o, "address", Value::from_string(address));
                    let _ = ctx.member_set(&o, "family", Value::Num(family as f64));
                    o
                })
                .collect();
            Ok(vec![ctx.make_array(items)])
        }
        Err(message) => Err(dns_error(ctx, &message)),
    }
}

/// `__dns.getaddrinfo(hostname, family, flags, resolve, reject)` — `getaddrinfo(3)` with the
/// caller's family and `AI_*` hints; resolves with `[{ address, family }, …]` in the resolver's
/// order, or rejects with `{ code: "EAI_…" }`.
pub fn op_getaddrinfo(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let hostname = arg_str(ctx, args, 0)?;
    let family = arg_num(args, 1) as u8;
    let flags = arg_num(args, 2) as i32;
    let (resolve, reject) = settle_pair(ctx, args, 3, "__dns.getaddrinfo")?;
    let id = lumen_host::register_task(ctx, resolve, Some(reject), decode_gai_list);
    spawn_handle(ctx).spawn_blocking(id, move || Box::new(gai_lookup(&hostname, family, flags)));
    Ok(Value::Undefined)
}

#[cfg(unix)]
fn gai_lookup(hostname: &str, family: u8, flags: i32) -> Result<Vec<(String, u8)>, &'static str> {
    use std::ffi::{CStr, CString};
    let host = CString::new(hostname).map_err(|_| "EAI_NONAME")?;
    // SAFETY: an all-zero addrinfo is the documented "no hints" starting point.
    let mut hints: libc::addrinfo = unsafe { std::mem::zeroed() };
    hints.ai_flags = flags;
    hints.ai_family = match family {
        4 => libc::AF_INET,
        6 => libc::AF_INET6,
        _ => libc::AF_UNSPEC,
    };
    hints.ai_socktype = libc::SOCK_STREAM;
    let mut res: *mut libc::addrinfo = std::ptr::null_mut();
    // SAFETY: `host` is NUL-terminated, `hints` is initialised and `res` receives the list.
    let rc = unsafe { libc::getaddrinfo(host.as_ptr(), std::ptr::null(), &hints, &mut res) };
    if rc != 0 {
        return Err(match rc {
            libc::EAI_AGAIN => "EAI_AGAIN",
            libc::EAI_BADFLAGS => "EAI_BADFLAGS",
            libc::EAI_FAMILY => "EAI_FAMILY",
            libc::EAI_MEMORY => "EAI_MEMORY",
            libc::EAI_NONAME => "EAI_NONAME",
            libc::EAI_SERVICE => "EAI_SERVICE",
            libc::EAI_SOCKTYPE => "EAI_SOCKTYPE",
            _ => "EAI_FAIL",
        });
    }
    let mut out = Vec::new();
    let mut cur = res;
    while !cur.is_null() {
        // SAFETY: `cur` walks the list getaddrinfo returned, which stays alive until freeaddrinfo.
        let info = unsafe { &*cur };
        if !info.ai_addr.is_null() {
            // SAFETY: the family says which sockaddr layout `ai_addr` points to.
            match info.ai_family {
                libc::AF_INET => {
                    let sa = unsafe { &*(info.ai_addr as *const libc::sockaddr_in) };
                    let ip = std::net::Ipv4Addr::from(u32::from_be(sa.sin_addr.s_addr));
                    out.push((ip.to_string(), 4));
                }
                libc::AF_INET6 => {
                    let sa = unsafe { &*(info.ai_addr as *const libc::sockaddr_in6) };
                    let ip = std::net::Ipv6Addr::from(sa.sin6_addr.s6_addr);
                    out.push((ip.to_string(), 6));
                }
                _ => {}
            }
        }
        cur = info.ai_next;
    }
    // SAFETY: `res` came from a successful getaddrinfo.
    unsafe { libc::freeaddrinfo(res) };
    if out.is_empty() {
        return Err("EAI_NONAME");
    }
    Ok(out)
}

#[cfg(not(unix))]
fn gai_lookup(hostname: &str, family: u8, _flags: i32) -> Result<Vec<(String, u8)>, &'static str> {
    system_lookup(hostname, family).map_err(|_| "EAI_NONAME")
}

fn decode_gai_list(
    ctx: &mut Ctx,
    payload: Box<dyn std::any::Any + Send>,
) -> Result<Vec<Value>, Value> {
    match *payload
        .downcast::<Result<Vec<(String, u8)>, &'static str>>()
        .expect("getaddrinfo payload")
    {
        Ok(list) => {
            let items = list
                .into_iter()
                .map(|(address, family)| {
                    let o = Value::Obj(ctx.new_object());
                    let _ = ctx.member_set(&o, "address", Value::from_string(address));
                    let _ = ctx.member_set(&o, "family", Value::Num(family as f64));
                    o
                })
                .collect();
            Ok(vec![ctx.make_array(items)])
        }
        Err(code) => {
            let err = ctx.make_error("Error", format!("getaddrinfo {code}"));
            let _ = ctx.set_member(&err, "code", Value::str(code));
            Err(err)
        }
    }
}

/// `__dns.getnameinfo(address, port, resolve, reject)` — `getnameinfo(3)`; resolves with
/// `[hostname, service]` or rejects with `{ code: "EAI_…" }`.
pub fn op_getnameinfo(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let address = arg_str(ctx, args, 0)?;
    let port = arg_num(args, 1) as u16;
    let (resolve, reject) = settle_pair(ctx, args, 2, "__dns.getnameinfo")?;
    let id = lumen_host::register_task(ctx, resolve, Some(reject), decode_name_info);
    spawn_handle(ctx).spawn_blocking(id, move || Box::new(name_info(&address, port)));
    Ok(Value::Undefined)
}

#[cfg(unix)]
fn name_info(address: &str, port: u16) -> Result<(String, String), &'static str> {
    use std::ffi::CStr;
    let ip: IpAddr = address.parse().map_err(|_| "EAI_NONAME")?;
    let addr = SocketAddr::new(ip, port);
    let (storage, len) = crate::net::sockaddr_of(&addr);
    let mut host = [0 as libc::c_char; 1025];
    let mut serv = [0 as libc::c_char; 64];
    // SAFETY: `storage` holds a valid sockaddr of `len` bytes; the output buffers are sized as
    // declared.
    let rc = unsafe {
        libc::getnameinfo(
            (&storage as *const libc::sockaddr_storage).cast(),
            len,
            host.as_mut_ptr(),
            host.len() as _,
            serv.as_mut_ptr(),
            serv.len() as _,
            0,
        )
    };
    if rc != 0 {
        return Err(match rc {
            libc::EAI_AGAIN => "EAI_AGAIN",
            libc::EAI_BADFLAGS => "EAI_BADFLAGS",
            libc::EAI_FAMILY => "EAI_FAMILY",
            libc::EAI_MEMORY => "EAI_MEMORY",
            libc::EAI_NONAME => "EAI_NONAME",
            _ => "EAI_FAIL",
        });
    }
    // SAFETY: getnameinfo wrote NUL-terminated strings.
    let text = |buf: &[libc::c_char]| unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned();
    Ok((text(&host), text(&serv)))
}

#[cfg(not(unix))]
fn name_info(address: &str, port: u16) -> Result<(String, String), &'static str> {
    let ip: IpAddr = address.parse().map_err(|_| "EAI_NONAME")?;
    Ok((ip.to_string(), port.to_string()))
}

fn decode_name_info(
    ctx: &mut Ctx,
    payload: Box<dyn std::any::Any + Send>,
) -> Result<Vec<Value>, Value> {
    match *payload
        .downcast::<Result<(String, String), &'static str>>()
        .expect("getnameinfo payload")
    {
        Ok((host, service)) => Ok(vec![ctx.make_array(vec![
            Value::from_string(host),
            Value::from_string(service),
        ])]),
        Err(code) => {
            let err = ctx.make_error("Error", format!("getnameinfo {code}"));
            let _ = ctx.set_member(&err, "code", Value::str(code));
            Err(err)
        }
    }
}

// ---- record-type queries via DNS-over-UDP ----------------------------------------------------

/// `__dns.resolve(hostname, rrtype, resolve, reject)` — a record-type query (A/AAAA/CNAME/NS/MX/
/// TXT/PTR) over UDP.
pub fn op_resolve(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let name = arg_str(ctx, args, 0)?;
    let rrtype = arg_str(ctx, args, 1)?;
    let (resolve, reject) = settle_pair(ctx, args, 2, "__dns.resolve")?;
    let id = lumen_host::register_task(ctx, resolve, Some(reject), decode_records);
    spawn_handle(ctx).spawn_blocking(id, move || Box::new(udp_resolve(&name, &rrtype)));
    Ok(Value::Undefined)
}

/// `__dns.getServers()` — the nameserver addresses from `/etc/resolv.conf`, as strings, for
/// `dns.getServers()` / the default `Resolver`'s initial server list. Reading the file is cheap,
/// so this stays synchronous rather than going through the blocking pool.
pub fn op_get_servers(ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    let items = resolv_nameservers()
        .into_iter()
        .map(|ip| Value::from_string(ip.to_string()))
        .collect();
    Ok(ctx.make_array(items))
}

/// The numeric DNS type code for a Node record-type string.
fn qtype_code(rrtype: &str) -> Option<u16> {
    Some(match rrtype {
        "A" => 1,
        "NS" => 2,
        "CNAME" => 5,
        "PTR" => 12,
        "MX" => 15,
        "TXT" => 16,
        "AAAA" => 28,
        _ => return None,
    })
}

fn udp_resolve(name: &str, rrtype: &str) -> Result<Vec<Rec>, String> {
    let qtype = qtype_code(rrtype).ok_or_else(|| format!("unsupported query type {rrtype}"))?;
    let servers = resolv_nameservers();
    if servers.is_empty() {
        return Err("no nameservers configured (empty /etc/resolv.conf)".to_string());
    }
    let packet = build_query(name, qtype);
    let mut last_err = String::from("no nameserver responded");
    for ns in servers {
        match query_server(ns, &packet) {
            Ok(response) => return parse_answers(&response, qtype),
            Err(e) => last_err = format!("{ns}: {e}"),
        }
    }
    Err(format!("query{rrtype} {name}: {last_err}"))
}

/// Nameserver addresses from `/etc/resolv.conf`.
fn resolv_nameservers() -> Vec<IpAddr> {
    let mut out = Vec::new();
    if let Ok(contents) = std::fs::read_to_string("/etc/resolv.conf") {
        for line in contents.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("nameserver") {
                if let Ok(ip) = rest.trim().parse::<IpAddr>() {
                    out.push(ip);
                }
            }
        }
    }
    out
}

/// Build a standard recursive query packet for one question.
fn build_query(name: &str, qtype: u16) -> Vec<u8> {
    let mut p = Vec::with_capacity(name.len() + 18);
    p.extend_from_slice(&0x1234u16.to_be_bytes()); // transaction id (own socket per query)
    p.extend_from_slice(&0x0100u16.to_be_bytes()); // flags: recursion desired
    p.extend_from_slice(&1u16.to_be_bytes()); // qdcount
    p.extend_from_slice(&[0, 0, 0, 0, 0, 0]); // ancount / nscount / arcount
    for label in name.trim_end_matches('.').split('.') {
        if label.is_empty() {
            continue;
        }
        let bytes = label.as_bytes();
        p.push(bytes.len().min(63) as u8);
        p.extend_from_slice(&bytes[..bytes.len().min(63)]);
    }
    p.push(0); // root label
    p.extend_from_slice(&qtype.to_be_bytes());
    p.extend_from_slice(&1u16.to_be_bytes()); // class IN
    p
}

/// Send a query to one nameserver and read the reply (5s timeout).
fn query_server(ns: IpAddr, packet: &[u8]) -> std::io::Result<Vec<u8>> {
    let bind = if ns.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
    let sock = UdpSocket::bind(bind)?;
    sock.set_read_timeout(Some(Duration::from_secs(5)))?;
    sock.send_to(packet, SocketAddr::new(ns, 53))?;
    let mut buf = vec![0u8; 4096];
    let (n, _) = sock.recv_from(&mut buf)?;
    buf.truncate(n);
    Ok(buf)
}

/// Parse the answer section, returning the records that match `qtype`.
fn parse_answers(buf: &[u8], qtype: u16) -> Result<Vec<Rec>, String> {
    if buf.len() < 12 {
        return Err("short DNS response".to_string());
    }
    let rcode = buf[3] & 0x0F;
    if rcode == 3 {
        return Err("NXDOMAIN".to_string());
    }
    if rcode != 0 {
        return Err(format!("server returned rcode {rcode}"));
    }
    let qdcount = u16::from_be_bytes([buf[4], buf[5]]) as usize;
    let ancount = u16::from_be_bytes([buf[6], buf[7]]) as usize;

    let mut pos = 12;
    // Skip the question section.
    for _ in 0..qdcount {
        pos = skip_name(buf, pos)?;
        pos += 4; // qtype + qclass
    }

    let mut out = Vec::new();
    for _ in 0..ancount {
        pos = skip_name(buf, pos)?;
        if pos + 10 > buf.len() {
            break;
        }
        let rtype = u16::from_be_bytes([buf[pos], buf[pos + 1]]);
        let rdlen = u16::from_be_bytes([buf[pos + 8], buf[pos + 9]]) as usize;
        pos += 10;
        let rdata_start = pos;
        if rdata_start + rdlen > buf.len() {
            break;
        }
        if rtype == qtype {
            if let Some(rec) = parse_rdata(buf, rdata_start, rdlen, rtype) {
                out.push(rec);
            }
        }
        pos = rdata_start + rdlen;
    }
    Ok(out)
}

fn parse_rdata(buf: &[u8], start: usize, len: usize, rtype: u16) -> Option<Rec> {
    let rdata = &buf[start..start + len];
    match rtype {
        1 if len == 4 => Some(Rec::Str(format!(
            "{}.{}.{}.{}",
            rdata[0], rdata[1], rdata[2], rdata[3]
        ))),
        28 if len == 16 => {
            let mut seg = [0u16; 8];
            for (i, s) in seg.iter_mut().enumerate() {
                *s = u16::from_be_bytes([rdata[i * 2], rdata[i * 2 + 1]]);
            }
            Some(Rec::Str(
                std::net::Ipv6Addr::new(
                    seg[0], seg[1], seg[2], seg[3], seg[4], seg[5], seg[6], seg[7],
                )
                .to_string(),
            ))
        }
        5 | 2 | 12 => Some(Rec::Str(read_name(buf, start).0)), // CNAME / NS / PTR
        15 if len >= 3 => {
            let priority = u16::from_be_bytes([rdata[0], rdata[1]]);
            let exchange = read_name(buf, start + 2).0;
            Some(Rec::Mx { priority, exchange })
        }
        16 => {
            // One or more length-prefixed character-strings.
            let mut chunks = Vec::new();
            let mut i = 0;
            while i < len {
                let clen = rdata[i] as usize;
                i += 1;
                if i + clen > len {
                    break;
                }
                chunks.push(String::from_utf8_lossy(&rdata[i..i + clen]).into_owned());
                i += clen;
            }
            Some(Rec::Txt(chunks))
        }
        _ => None,
    }
}

/// Read a (possibly compressed) domain name, returning it and the offset just past the *first*
/// pointer or terminating zero (the position to resume linear parsing from).
fn read_name(buf: &[u8], mut pos: usize) -> (String, usize) {
    let mut labels: Vec<String> = Vec::new();
    let mut jumped = false;
    let mut resume = pos;
    let mut guard = 0;
    loop {
        guard += 1;
        if pos >= buf.len() || guard > 128 {
            break;
        }
        let len = buf[pos];
        if len & 0xC0 == 0xC0 {
            if pos + 1 >= buf.len() {
                break;
            }
            let ptr = (((len & 0x3F) as usize) << 8) | buf[pos + 1] as usize;
            if !jumped {
                resume = pos + 2;
            }
            jumped = true;
            pos = ptr;
            continue;
        }
        if len == 0 {
            pos += 1;
            if !jumped {
                resume = pos;
            }
            break;
        }
        pos += 1;
        if pos + len as usize > buf.len() {
            break;
        }
        labels.push(String::from_utf8_lossy(&buf[pos..pos + len as usize]).into_owned());
        pos += len as usize;
        if !jumped {
            resume = pos;
        }
    }
    (labels.join("."), resume)
}

/// Advance past a domain name (following no pointers — returns the offset after the name's
/// on-the-wire encoding).
fn skip_name(buf: &[u8], mut pos: usize) -> Result<usize, String> {
    loop {
        if pos >= buf.len() {
            return Err("truncated name".to_string());
        }
        let len = buf[pos];
        if len & 0xC0 == 0xC0 {
            return Ok(pos + 2);
        }
        if len == 0 {
            return Ok(pos + 1);
        }
        pos += 1 + len as usize;
    }
}

/// Records payload → the value array Node's `resolve*` callbacks receive.
fn decode_records(
    ctx: &mut Ctx,
    payload: Box<dyn std::any::Any + Send>,
) -> Result<Vec<Value>, Value> {
    match *payload
        .downcast::<Result<Vec<Rec>, String>>()
        .expect("resolve payload")
    {
        Ok(records) => {
            let items = records
                .into_iter()
                .map(|rec| match rec {
                    Rec::Str(s) => Value::from_string(s),
                    Rec::Mx { priority, exchange } => {
                        let o = Value::Obj(ctx.new_object());
                        let _ = ctx.member_set(&o, "priority", Value::Num(priority as f64));
                        let _ = ctx.member_set(&o, "exchange", Value::from_string(exchange));
                        o
                    }
                    Rec::Txt(chunks) => {
                        ctx.make_array(chunks.into_iter().map(Value::from_string).collect())
                    }
                })
                .collect();
            Ok(vec![ctx.make_array(items)])
        }
        Err(message) => Err(dns_error(ctx, &message)),
    }
}

// ---- shared helpers --------------------------------------------------------------------------

fn arg_str(ctx: &mut Ctx, args: &[Value], i: usize) -> Result<String, Value> {
    Ok(ctx
        .coerce_string(args.get(i).unwrap_or(&Value::Undefined))?
        .to_string())
}

fn arg_num(args: &[Value], i: usize) -> f64 {
    args.get(i).and_then(|v| v.as_num_opt()).unwrap_or(0.0)
}

/// The trailing `(resolve, reject)` callback pair the glue passes at index `at`.
fn settle_pair(
    ctx: &mut Ctx,
    args: &[Value],
    at: usize,
    who: &str,
) -> Result<(Value, Value), Value> {
    match (args.get(at), args.get(at + 1)) {
        (Some(res), Some(rej)) if res.is_callable() && rej.is_callable() => {
            Ok((res.clone(), rej.clone()))
        }
        _ => Err(ctx.make_error("TypeError", format!("{who} expects (resolve, reject)"))),
    }
}

/// A DNS error carrying the `code` Node users switch on (best-effort mapping).
fn dns_error(ctx: &mut Ctx, message: &str) -> Value {
    let err = ctx.make_error("Error", message.to_string());
    let code = if message.contains("NXDOMAIN") || message.contains("no matching") {
        "ENOTFOUND"
    } else if message.contains("timed out") || message.contains("timeout") {
        "ETIMEOUT"
    } else {
        "EAI_FAIL"
    };
    let _ = ctx.set_member(&err, "code", Value::str(code));
    err
}
