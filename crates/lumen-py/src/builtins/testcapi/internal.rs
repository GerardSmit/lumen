//! `_testinternalcapi`: interpreter-level introspection used by CPython's own tests.

use crate::object::*;
use crate::vm::{dict_set_str, Interp};

fn config_dict(it: &mut Interp) -> Obj {
    let d = it.new_dict();
    for (k, v) in [
        ("code_debug_ranges", Value::Int(1)),
        ("isolated", Value::Int(0)),
        ("use_environment", Value::Int(1)),
        ("verbose", Value::Int(0)),
        ("bytes_warning", Value::Int(0)),
        ("optimization_level", Value::Int(0)),
        ("safe_path", Value::Int(0)),
        ("int_max_str_digits", Value::Int(4300)),
        ("tracemalloc", Value::Int(0)),
        ("import_time", Value::Int(0)),
        ("perf_profiling", Value::Int(0)),
    ] {
        dict_set_str(&d, k, v);
    }
    d
}

#[lumen_bind::module(name = "_testinternalcapi")]
pub mod _testinternalcapi {
    use super::*;

    #[constant(name = "SIZEOF_PYGC_HEAD")]
    const SIZEOF_PYGC_HEAD: i64 = 16;
    #[constant(name = "SIZEOF_TIME_T")]
    const SIZEOF_TIME_T: i64 = 8;

    /// get_recursion_depth() -> int
    #[op]
    fn get_recursion_depth(it: &mut Interp) -> i64 {
        i64::from(it.depth)
    }

    /// get_config() -> dict
    #[op]
    fn get_config(it: &mut Interp) -> Value {
        Value::Obj(config_dict(it))
    }

    /// get_configs() -> dict
    #[op]
    fn get_configs(it: &mut Interp) -> Value {
        let d = it.new_dict();
        let c = config_dict(it);
        dict_set_str(&d, "config", Value::Obj(c));
        Value::Obj(d)
    }

    /// normalize_path(path) -> str: collapse `.`, `..` and duplicate separators.
    #[op]
    fn normalize_path(it: &mut Interp, path: &str) -> R<String> {
        if path.contains('\0') {
            return Err(it.value_error("embedded null character"));
        }
        let absolute = path.starts_with('/');
        let mut parts: Vec<&str> = Vec::new();
        for p in path.split('/') {
            match p {
                "" | "." => {}
                ".." => {
                    if parts.last().is_some_and(|l| *l != "..") {
                        parts.pop();
                    } else if !absolute {
                        parts.push("..");
                    }
                }
                _ => parts.push(p),
            }
        }
        let joined = parts.join("/");
        Ok(if absolute { format!("/{joined}") } else if joined.is_empty() { ".".to_string() } else { joined })
    }
}
