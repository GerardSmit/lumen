//! Ops for targets where the host process offers no sockets, subprocesses, native libraries or
//! threads (`wasm32-unknown-unknown`). The tables keep the names the JS glue reads; calling one
//! throws `ERR_NOT_SUPPORTED_IN_BROWSER`, so `require('node:net')` loads and only use fails.

use lumen::embed::OpError;
use lumen_host::{Ctx, Value};

fn unsupported_error() -> OpError {
    OpError::error("this API is not available in the browser runtime").with_code("ERR_NOT_SUPPORTED_IN_BROWSER")
}

pub mod child {
    use super::*;

    #[derive(Default)]
    pub struct ChildRegistry;

    pub fn close_child_pipes(_ctx: &mut Ctx) {}

    pub(crate) use bindings::Module;

    #[lumen_bind::module(name = "__child")]
    pub(crate) mod bindings {
        use super::*;

        #[op]
        pub fn spawn(_a0: Value, _a1: Value, _a2: Value, _a3: Value, _a4: Value, _a5: Value, _a6: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op]
        pub fn read(_a0: Value, _a1: Value, _a2: Value, _a3: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op]
        pub fn write(_a0: Value, _a1: Value, _a2: Value, _a3: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op]
        pub fn wait(_a0: Value, _a1: Value, _a2: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op]
        pub fn unref(_a0: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "ref")]
        pub fn ref_(_a0: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op]
        pub fn kill(_a0: Value, _a1: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "closeStdin")]
        pub fn close_stdin(_a0: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "writeFd")]
        pub fn write_fd(_a0: Value, _a1: Value, _a2: Value, _a3: Value, _a4: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "closeFd")]
        pub fn close_fd(_a0: Value, _a1: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "execSync")]
        pub fn exec_sync(_a0: Value, _a1: Value, _a2: Value, _a3: Value, _a4: Value, _a5: Value, _a6: Value, _a7: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "ipcOpen")]
        pub fn ipc_open(_a0: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "ipcRead")]
        pub fn ipc_read(_a0: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "ipcWrite")]
        pub fn ipc_write(_a0: Value, _a1: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "ipcClose")]
        pub fn ipc_close(_a0: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }
    }
}

pub mod net {
    use super::*;

    #[derive(Default)]
    pub struct NetRegistry;

    #[derive(Default)]
    pub struct DgramRegistry;

    pub(crate) use bindings::Module;
    pub(crate) use udp_bindings::Module as UdpModule;

    #[lumen_bind::module(name = "__net")]
    pub(crate) mod bindings {
        use super::*;

        #[op]
        pub fn connect(_a0: Value, _a1: Value, _a2: Value, _a3: Value, _a4: Value, _a5: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "connectPath")]
        pub fn connect_path(_a0: Value, _a1: Value, _a2: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op]
        pub fn read(_a0: Value, _a1: Value, _a2: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op]
        pub fn write(_a0: Value, _a1: Value, _a2: Value, _a3: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "tryWrite")]
        pub fn try_write(_a0: Value, _a1: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "endWritable")]
        pub fn end_writable(_a0: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op]
        pub fn close(_a0: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "setNoDelay")]
        pub fn set_no_delay(_a0: Value, _a1: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "setKeepAlive")]
        pub fn set_keep_alive(_a0: Value, _a1: Value, _a2: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op]
        pub fn address(_a0: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "socketRef")]
        pub fn socket_ref(_a0: Value, _a1: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op]
        pub fn listen(_a0: Value, _a1: Value, _a2: Value, _a3: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "listenPath")]
        pub fn listen_path(_a0: Value, _a1: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op]
        pub fn accept(_a0: Value, _a1: Value, _a2: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "closeServer")]
        pub fn close_server(_a0: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "serverAddress")]
        pub fn server_address(_a0: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "serverRef")]
        pub fn server_ref(_a0: Value, _a1: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }
    }

    #[lumen_bind::module(name = "__udp")]
    pub(crate) mod udp_bindings {
        use super::*;

        #[op(name = "bind")]
        pub fn bind_(_a0: Value, _a1: Value, _a2: Value, _a3: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op]
        pub fn recv(_a0: Value, _a1: Value, _a2: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op]
        pub fn send(_a0: Value, _a1: Value, _a2: Value, _a3: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op]
        pub fn connect(_a0: Value, _a1: Value, _a2: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op]
        pub fn disconnect(_a0: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op]
        pub fn peer(_a0: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op]
        pub fn close(_a0: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op]
        pub fn address(_a0: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "setBroadcast")]
        pub fn set_broadcast(_a0: Value, _a1: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "setTTL")]
        pub fn set_ttl(_a0: Value, _a1: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "setMulticastTTL")]
        pub fn set_multicast_ttl(_a0: Value, _a1: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "setMulticastLoopback")]
        pub fn set_multicast_loopback(_a0: Value, _a1: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "setMulticastInterface")]
        pub fn set_multicast_interface(_a0: Value, _a1: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "addMembership")]
        pub fn add_membership(_a0: Value, _a1: Value, _a2: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "dropMembership")]
        pub fn drop_membership(_a0: Value, _a1: Value, _a2: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "addSourceMembership")]
        pub fn add_source_membership(_a0: Value, _a1: Value, _a2: Value, _a3: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "dropSourceMembership")]
        pub fn drop_source_membership(_a0: Value, _a1: Value, _a2: Value, _a3: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "getBufferSize")]
        pub fn get_buffer_size(_a0: Value, _a1: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "setBufferSize")]
        pub fn set_buffer_size(_a0: Value, _a1: Value, _a2: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "udpRef")]
        pub fn udp_ref(_a0: Value, _a1: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }
    }
}

pub mod tls {
    use super::*;

    #[derive(Default)]
    pub struct TlsRegistry;

    pub(crate) use bindings::Module;

    #[lumen_bind::module(name = "__tls")]
    pub(crate) mod bindings {
        use super::*;

        #[op]
        pub fn available() -> bool {
            false
        }

        #[op(name = "rootCertificates")]
        pub fn root_certificates() -> Vec<String> {
            Vec::new()
        }

        #[op]
        pub fn ciphers() -> Vec<String> {
            Vec::new()
        }

        #[op(name = "ctxOp")]
        pub fn ctx_op(_a0: Value, _a1: Value, _a2: Value, _a3: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "sessNew")]
        pub fn sess_new(_a0: Value, _a1: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "sessFree")]
        pub fn sess_free(_a0: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "sessOp")]
        pub fn sess_op(_a0: Value, _a1: Value, _a2: Value, _a3: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op]
        pub fn feed(_a0: Value, _a1: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op]
        pub fn output(_a0: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op]
        pub fn read(_a0: Value, _a1: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op]
        pub fn write(_a0: Value, _a1: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op]
        pub fn events(_a0: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "lastError")]
        pub fn last_error(_a0: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }
    }
}

pub mod dns {
    use super::*;

    pub(crate) use bindings::Module;

    #[lumen_bind::module(name = "__dns")]
    pub(crate) mod bindings {
        use super::*;

        #[op(name = "getaddrinfo")]
        pub fn getaddrinfo(#[varargs] _args: &[Value]) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "getnameinfo")]
        pub fn getnameinfo(#[varargs] _args: &[Value]) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "getServers")]
        pub fn get_servers() -> Vec<String> {
            Vec::new()
        }
    }
}

pub mod napi {
    use super::*;

    pub(crate) use bindings::Module;

    #[lumen_bind::module(name = "__node")]
    pub(crate) mod bindings {
        use super::*;

        #[op(name = "loadNativeAddon")]
        pub fn load_native_addon(#[varargs] _args: &[Value]) -> Result<(), OpError> {
            Err(unsupported_error())
        }
    }

    pub fn shutdown(_ctx: &mut Ctx) {}
}

pub mod ffi {
    use super::*;

    pub(crate) use bindings::Module;

    #[lumen_bind::module(name = "__ffi")]
    pub(crate) mod bindings {
        use super::*;

        #[op(name = "dlopen")]
        pub fn dlopen(#[varargs] _args: &[Value]) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "dlsym")]
        pub fn dlsym(#[varargs] _args: &[Value]) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "dlclose")]
        pub fn dlclose(#[varargs] _args: &[Value]) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "call")]
        pub fn call(#[varargs] _args: &[Value]) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "ptr")]
        pub fn ptr(#[varargs] _args: &[Value]) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "read")]
        pub fn read(#[varargs] _args: &[Value]) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "readCString")]
        pub fn read_cstring(#[varargs] _args: &[Value]) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "toArrayBuffer")]
        pub fn to_array_buffer(#[varargs] _args: &[Value]) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "toBuffer")]
        pub fn to_buffer(#[varargs] _args: &[Value]) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "registerCallback")]
        pub fn register_callback(#[varargs] _args: &[Value]) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "unregisterCallback")]
        pub fn unregister_callback(#[varargs] _args: &[Value]) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "cc")]
        pub fn cc(#[varargs] _args: &[Value]) -> Result<(), OpError> {
            Err(unsupported_error())
        }
    }
}

pub mod sqlite {
    use super::*;

    pub(crate) use bindings::Module;

    #[lumen_bind::module(name = "__sqlite")]
    pub(crate) mod bindings {
        use super::*;

        #[op]
        pub fn open(_a0: Value, _a1: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op]
        pub fn function(_a0: Value, _a1: Value, _a2: Value, _a3: Value, _a4: Value, _a5: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op]
        pub fn close(_a0: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op]
        pub fn prepare(_a0: Value, _a1: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op]
        pub fn finalize(_a0: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op]
        pub fn reset(_a0: Value, _a1: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "bind")]
        pub fn bind_(_a0: Value, _a1: Value, _a2: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op]
        pub fn step(_a0: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "columnCount")]
        pub fn column_count(_a0: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "columnNames")]
        pub fn column_names(_a0: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op]
        pub fn columns(_a0: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op]
        pub fn row(_a0: Value, _a1: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "bindParameterCount")]
        pub fn bind_parameter_count(_a0: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "bindParameterName")]
        pub fn bind_parameter_name(_a0: Value, _a1: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "bindParameterIndex")]
        pub fn bind_parameter_index(_a0: Value, _a1: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op]
        pub fn exec(_a0: Value, _a1: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op]
        pub fn changes(_a0: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "totalChanges")]
        pub fn total_changes(_a0: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "isTransaction")]
        pub fn is_transaction(_a0: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op]
        pub fn location(_a0: Value, _a1: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "doubleQuotedStringLiterals")]
        pub fn double_quoted_string_literals(_a0: Value, _a1: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op]
        pub fn defensive(_a0: Value, _a1: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "lastInsertRowid")]
        pub fn last_insert_rowid(_a0: Value, _a1: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "expandedSql")]
        pub fn expanded_sql(_a0: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op]
        pub fn libversion() -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op]
        pub fn serialize(_a0: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op]
        pub fn deserialize(_a0: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "setCustomSQLite")]
        pub fn set_custom_sq_lite(_a0: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "enableLoadExtension")]
        pub fn enable_load_extension(_a0: Value, _a1: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "loadExtension")]
        pub fn load_extension(_a0: Value, _a1: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }

        #[op(name = "fileControl")]
        pub fn file_control(_a0: Value, _a1: Value, _a2: Value) -> Result<(), OpError> {
            Err(unsupported_error())
        }
    }
}

pub mod vm_timeout {
    use super::*;

    pub(crate) use bindings::Module;

    #[lumen_bind::module(name = "__vm")]
    pub(crate) mod bindings {
        use super::*;

        #[op(name = "runWithTimeout")]
        pub fn run_with_timeout(ctx: &mut Ctx, _timeout_ms: Value, callee: Value) -> Result<Value, OpError> {
            ctx.invoke(callee, Value::Undefined, &[]).map_err(OpError::thrown)
        }
    }
}
