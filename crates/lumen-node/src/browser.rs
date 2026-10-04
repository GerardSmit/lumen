//! Ops for targets where the host process offers no sockets, subprocesses, native libraries or
//! threads (`wasm32-unknown-unknown`). The tables keep the names the JS glue reads; calling one
//! throws `ERR_NOT_SUPPORTED_IN_BROWSER`, so `require('node:net')` loads and only use fails.

use lumen_host::{ops, Ctx, OpDecl, Value};

fn unsupported(ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    let err = ctx.make_error(
        "Error",
        "this API is not available in the browser runtime".to_string(),
    );
    let _ = ctx.set_member(&err, "code", Value::str("ERR_NOT_SUPPORTED_IN_BROWSER"));
    Err(err)
}

fn empty_list(ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    Ok(ctx.make_array(Vec::new()))
}

fn no(_ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    Ok(Value::Bool(false))
}

pub mod child {
    use super::*;

    #[derive(Default)]
    pub struct ChildRegistry;

    pub const CHILD_OPS: &[OpDecl] = ops![
    "spawn" (7) => unsupported,
    "read" (4) => unsupported,
    "write" (4) => unsupported,
    "wait" (3) => unsupported,
    "unref" (1) => unsupported,
    "ref" (1) => unsupported,
    "kill" (2) => unsupported,
    "closeStdin" (1) => unsupported,
    "writeFd" (5) => unsupported,
    "closeFd" (2) => unsupported,
    "execSync" (8) => unsupported,
    "ipcOpen" (1) => unsupported,
    "ipcRead" (1) => unsupported,
    "ipcWrite" (2) => unsupported,
    "ipcClose" (1) => unsupported,
    ];
}

pub mod net {
    use super::*;

    #[derive(Default)]
    pub struct NetRegistry;

    #[derive(Default)]
    pub struct DgramRegistry;

    pub const NET_OPS: &[OpDecl] = ops![
    "connect" (6) => unsupported,
    "connectPath" (3) => unsupported,
    "read" (3) => unsupported,
    "write" (4) => unsupported,
    "tryWrite" (2) => unsupported,
    "endWritable" (1) => unsupported,
    "close" (1) => unsupported,
    "setNoDelay" (2) => unsupported,
    "setKeepAlive" (3) => unsupported,
    "address" (1) => unsupported,
    "socketRef" (2) => unsupported,
    "listen" (4) => unsupported,
    "listenPath" (2) => unsupported,
    "accept" (3) => unsupported,
    "closeServer" (1) => unsupported,
    "serverAddress" (1) => unsupported,
    "serverRef" (2) => unsupported,
    ];

    pub const UDP_OPS: &[OpDecl] = ops![
    "bind" (4) => unsupported,
    "recv" (3) => unsupported,
    "send" (4) => unsupported,
    "connect" (3) => unsupported,
    "disconnect" (1) => unsupported,
    "peer" (1) => unsupported,
    "close" (1) => unsupported,
    "address" (1) => unsupported,
    "setBroadcast" (2) => unsupported,
    "setTTL" (2) => unsupported,
    "setMulticastTTL" (2) => unsupported,
    "setMulticastLoopback" (2) => unsupported,
    "setMulticastInterface" (2) => unsupported,
    "addMembership" (3) => unsupported,
    "dropMembership" (3) => unsupported,
    "addSourceMembership" (4) => unsupported,
    "dropSourceMembership" (4) => unsupported,
    "getBufferSize" (2) => unsupported,
    "setBufferSize" (3) => unsupported,
    "udpRef" (2) => unsupported,
    ];
}

pub mod tls {
    use super::*;

    #[derive(Default)]
    pub struct TlsRegistry;

    pub const TLS_OPS: &[OpDecl] = ops![
        "available" (0) => no,
        "rootCertificates" (0) => empty_list,
        "ciphers" (0) => empty_list,
    "ctxOp" (4) => unsupported,
    "sessNew" (2) => unsupported,
    "sessFree" (1) => unsupported,
    "sessOp" (4) => unsupported,
    "feed" (2) => unsupported,
    "output" (1) => unsupported,
    "read" (2) => unsupported,
    "write" (2) => unsupported,
    "events" (1) => unsupported,
    "lastError" (1) => unsupported,
    ];
}

pub mod dns {
    pub use super::empty_list as op_get_servers;
    pub use super::unsupported as op_lookup;
    pub use super::unsupported as op_resolve;
    pub use super::unsupported as op_getaddrinfo;
    pub use super::unsupported as op_getnameinfo;
}

pub mod napi {
    use super::*;

    pub use super::unsupported as op_load_addon;

    pub fn shutdown(_ctx: &mut Ctx) {}
}

pub mod ffi {
    pub use super::unsupported as op_dlopen;
    pub use super::unsupported as op_dlsym;
    pub use super::unsupported as op_dlclose;
    pub use super::unsupported as op_call;
    pub use super::unsupported as op_ptr;
    pub use super::unsupported as op_read;
    pub use super::unsupported as op_read_cstring;
    pub use super::unsupported as op_to_array_buffer;
    pub use super::unsupported as op_to_buffer;
    pub use super::unsupported as op_register_callback;
    pub use super::unsupported as op_unregister_callback;
    pub use super::unsupported as op_cc;
}

pub mod sqlite {
    use super::*;

    pub const SQLITE_OPS: &[OpDecl] = ops![
    "open" (2) => unsupported,
    "function" (6) => unsupported,
    "close" (1) => unsupported,
    "prepare" (2) => unsupported,
    "finalize" (1) => unsupported,
    "reset" (2) => unsupported,
    "bind" (3) => unsupported,
    "step" (1) => unsupported,
    "columnCount" (1) => unsupported,
    "columnNames" (1) => unsupported,
    "columns" (1) => unsupported,
    "row" (2) => unsupported,
    "bindParameterCount" (1) => unsupported,
    "bindParameterName" (2) => unsupported,
    "bindParameterIndex" (2) => unsupported,
    "exec" (2) => unsupported,
    "changes" (1) => unsupported,
    "totalChanges" (1) => unsupported,
    "isTransaction" (1) => unsupported,
    "location" (2) => unsupported,
    "doubleQuotedStringLiterals" (2) => unsupported,
    "defensive" (2) => unsupported,
    "lastInsertRowid" (2) => unsupported,
    "expandedSql" (1) => unsupported,
    "libversion" (0) => unsupported,
    "serialize" (1) => unsupported,
    "deserialize" (1) => unsupported,
    "setCustomSQLite" (1) => unsupported,
    "enableLoadExtension" (2) => unsupported,
    "loadExtension" (2) => unsupported,
    "fileControl" (3) => unsupported,
    ];
}

pub mod vm_timeout {
    use super::*;

    pub const VM_OPS: &[OpDecl] = ops![
        "runWithTimeout" (2) => run_with_timeout,
    ];

    fn run_with_timeout(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
        let callee = args.get(1).cloned().unwrap_or(Value::Undefined);
        ctx.invoke(callee, Value::Undefined, &[])
    }
}
