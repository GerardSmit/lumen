//! Versioned native namespace catalogs shared by producers and host runtimes.

#[derive(Clone, Copy, Debug, Default)]
pub struct Features {
    pub node: bool,
    pub parallel: bool,
    pub http2: bool,
    pub cluster: bool,
    pub dgram: bool,
    pub wasi: bool,
    pub bitnest_process: bool,
}

const NODE_NAMES: &[&str] = &[
    "sqlite",
    "util/types",
    "buffer",
    "path",
    "os",
    "fs",
    "module",
    "events",
    "util",
    "sys",
    "console",
    "timers",
    "timers/promises",
    "crypto",
    "querystring",
    "url",
    "net",
    "assert",
    "assert/strict",
    "string_decoder",
    "tty",
    "async_hooks",
    "zlib",
    "stream",
    "stream/web",
    "stream/promises",
    "stream/consumers",
    "http",
    "https",
    "http2",
    "perf_hooks",
    "fs/promises",
    "path/posix",
    "path/win32",
    "child_process",
    "dns",
    "dns/promises",
    "v8",
    "inspector",
    "inspector/promises",
    "worker_threads",
    "readline",
    "readline/promises",
    "test",
    "test/reporters",
    "tls",
    "diagnostics_channel",
    "domain",
    "trace_events",
    "vm",
    "repl",
    "cluster",
    "dgram",
    "wasi",
    "process",
];

pub fn entries(features: Features) -> Vec<(String, String, u64)> {
    let mut entries = Vec::new();
    if features.node {
        let signature =
            super::fingerprint::binding_signature_hash("lumen-node/native-namespace/v1");
        for name in NODE_NAMES {
            if !match *name {
                "http2" => features.http2,
                "cluster" => features.cluster,
                "dgram" => features.dgram,
                "wasi" => features.wasi,
                _ => true,
            } {
                continue;
            }
            entries.push(((*name).into(), String::new(), signature));
            entries.push((format!("node:{name}"), String::new(), signature));
        }
    }
    if features.parallel {
        entries.push((
            "lumen:parallel".into(),
            String::new(),
            super::fingerprint::binding_signature_hash("run/spawn/v1"),
        ));
    }
    if features.bitnest_process {
        let signature = super::fingerprint::binding_signature_hash(
            "bitnest-process/spawn,fork,Process,streams,wait,signals,ipc/v1",
        );
        for name in ["bitnest:process", "node:child_process", "node:process"] {
            entries.push((name.into(), String::new(), signature));
        }
    }
    entries.sort_unstable();
    entries
}

pub fn hash(features: Features) -> u64 {
    let entries = entries(features);
    super::fingerprint::builtin_modules_hash(
        &entries
            .iter()
            .map(|(module, name, signature)| (module.as_str(), name.as_str(), *signature))
            .collect::<Vec<_>>(),
    )
}

/// Recognize only the finite catalogs whose exact namespaces this version defines.
pub fn known(hash_value: u64) -> Option<Features> {
    for bits in 0..128u8 {
        let features = Features {
            node: bits & 1 != 0,
            parallel: bits & 2 != 0,
            http2: bits & 4 != 0,
            cluster: bits & 8 != 0,
            dgram: bits & 16 != 0,
            wasi: bits & 32 != 0,
            bitnest_process: bits & 64 != 0,
        };
        if hash(features) == hash_value {
            return Some(features);
        }
    }
    None
}
