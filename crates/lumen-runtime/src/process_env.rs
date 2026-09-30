//! Realm-owned environment data. SHARE_ENV passes this backing only to admitted child workers;
//! writes never mutate the embedding process's OS environment.
use lumen_host::{Ctx, Extension, OpState, Value, ops};
use std::sync::{Arc, Mutex};

const MAX_KEYS: usize = 16_384;
const MAX_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Default)]
pub(crate) struct RealmEnvironment(Arc<Mutex<Environment>>);
#[derive(Default)]
struct Environment {
    values: Vec<(String, String)>,
    bytes: usize,
    insensitive: bool,
}
impl Environment {
    fn index(&self, key: &str) -> Option<usize> {
        self.values
            .iter()
            .position(|(name, _)| name == key || self.insensitive && name.eq_ignore_ascii_case(key))
    }
    fn set(&mut self, key: String, value: String) -> Result<(), &'static str> {
        if key.contains('\0') || key.contains('=') || value.contains('\0') {
            return Err("environment keys/values cannot contain NUL and keys cannot contain '='");
        }
        let index = self.index(&key);
        let old = index
            .map(|i| self.values[i].0.len() + self.values[i].1.len())
            .unwrap_or(0);
        let cost = index.map(|i| self.values[i].0.len()).unwrap_or(key.len()) + value.len();
        if self.bytes - old + cost > MAX_BYTES || index.is_none() && self.values.len() >= MAX_KEYS {
            return Err("realm environment limit exceeded (16384 keys or 16 MiB)");
        }
        self.bytes = self.bytes - old + cost;
        match index {
            Some(i) => self.values[i].1 = value,
            None => self.values.push((key, value)),
        }
        Ok(())
    }
}

pub(crate) fn replace(
    ctx: &mut Ctx,
    values: Vec<(String, String)>,
    insensitive: bool,
) -> Result<(), Value> {
    let mut next = Environment {
        insensitive,
        ..Default::default()
    };
    for (key, value) in values {
        next.set(key, value)
            .map_err(|msg| ctx.make_error("RangeError", msg))?;
    }
    let backing = ctx
        .op_state()
        .get::<RealmEnvironment>()
        .expect("environment installed")
        .clone();
    *backing.0.lock().unwrap() = next;
    Ok(())
}
pub(crate) fn backing(ctx: &mut Ctx) -> RealmEnvironment {
    ctx.op_state()
        .get::<RealmEnvironment>()
        .expect("environment installed")
        .clone()
}
pub(crate) fn bind(ctx: &mut Ctx, backing: RealmEnvironment) {
    ctx.op_state().put(backing);
}

fn get(ctx: &mut Ctx, _: Value, args: &[Value]) -> Result<Value, Value> {
    let key = ctx
        .coerce_string(args.first().unwrap_or(&Value::Undefined))?
        .to_string();
    let backing = backing(ctx);
    let env = backing.0.lock().unwrap();
    Ok(env
        .index(&key)
        .map(|i| Value::str(&env.values[i].1))
        .unwrap_or(Value::Undefined))
}
fn set(ctx: &mut Ctx, _: Value, args: &[Value]) -> Result<Value, Value> {
    let key = ctx
        .coerce_string(args.first().unwrap_or(&Value::Undefined))?
        .to_string();
    let value = ctx
        .coerce_string(args.get(1).unwrap_or(&Value::Undefined))?
        .to_string();
    let backing = backing(ctx);
    backing
        .0
        .lock()
        .unwrap()
        .set(key, value)
        .map_err(|msg| ctx.make_error("RangeError", msg))?;
    Ok(Value::Undefined)
}
fn delete(ctx: &mut Ctx, _: Value, args: &[Value]) -> Result<Value, Value> {
    let key = ctx
        .coerce_string(args.first().unwrap_or(&Value::Undefined))?
        .to_string();
    let backing = backing(ctx);
    let mut env = backing.0.lock().unwrap();
    if let Some(i) = env.index(&key) {
        let (key, value) = env.values.remove(i);
        env.bytes -= key.len() + value.len();
    }
    Ok(Value::Undefined)
}
fn keys(ctx: &mut Ctx, _: Value, _: &[Value]) -> Result<Value, Value> {
    let backing = backing(ctx);
    let names = backing
        .0
        .lock()
        .unwrap()
        .values
        .iter()
        .map(|(key, _)| Value::str(key))
        .collect();
    Ok(ctx.make_array(names))
}
fn reset(ctx: &mut Ctx, _: Value, args: &[Value]) -> Result<Value, Value> {
    let object = args.first().unwrap_or(&Value::Undefined);
    let mut values = Vec::new();
    let length = match ctx
        .get_member(object, "length")
        .map_err(|_| ctx.make_error("TypeError", "invalid environment entries"))?
    {
        Value::Num(n) if n >= 0.0 && n <= MAX_KEYS as f64 && n.fract() == 0.0 => n as usize,
        _ => return Err(ctx.make_error("RangeError", "invalid environment entry count")),
    };
    for index in 0..length {
        let pair = ctx
            .get_member(object, &index.to_string())
            .map_err(|_| ctx.make_error("TypeError", "invalid environment entry"))?;
        let key = ctx
            .get_member(&pair, "0")
            .map_err(|_| ctx.make_error("TypeError", "invalid environment key"))?;
        let value = ctx
            .get_member(&pair, "1")
            .map_err(|_| ctx.make_error("TypeError", "invalid environment value"))?;
        if !matches!(value, Value::Undefined) {
            values.push((
                ctx.coerce_string(&key)?.to_string(),
                ctx.coerce_string(&value)?.to_string(),
            ));
        }
    }
    replace(ctx, values, false)?;
    Ok(Value::Undefined)
}
pub(crate) fn extension() -> Extension {
    Extension {
        name: "realm-environment",
        globals: &[],
        namespaces: &[(
            "__env",
            ops!["get" (1) => get, "set" (2) => set, "delete" (1) => delete, "keys" (0) => keys, "reset" (1) => reset],
        )],
        state_init: Some(|state: &mut OpState| state.put(RealmEnvironment::default())),
        js_init: None,
        js_init_snapshot: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn environment_limits_reject_atomically_and_allow_replacement() {
        let mut env = Environment::default();
        for index in 0..MAX_KEYS {
            env.set(index.to_string(), "old".into()).unwrap();
        }
        let before = env.bytes;
        assert!(env.set("extra".into(), "no".into()).is_err());
        assert_eq!(env.values.len(), MAX_KEYS);
        assert_eq!(env.bytes, before);
        env.set("0".into(), "new".into()).unwrap();
        assert_eq!(env.values[0].1, "new");
        assert!(env.set("bad=key".into(), "no".into()).is_err());
        let mut bytes = Environment::default();
        bytes.set("k".into(), "x".repeat(MAX_BYTES - 1)).unwrap();
        assert_eq!(bytes.bytes, MAX_BYTES);
        assert!(bytes.set("extra".into(), "no".into()).is_err());
        assert!(bytes.set("k".into(), "x".repeat(MAX_BYTES)).is_err());
        assert_eq!(bytes.bytes, MAX_BYTES);
        assert_eq!(bytes.values.len(), 1);
    }
    #[test]
    fn windows_shared_environment_preserves_names_and_byte_accounting() {
        let mut env = Environment {
            insensitive: true,
            ..Default::default()
        };
        env.set("Path".into(), "first".into()).unwrap();
        env.set("PATH".into(), "second".into()).unwrap();
        assert_eq!(env.values, [("Path".into(), "second".into())]);
        assert_eq!(env.bytes, 10);
        assert_eq!(env.index("path"), Some(0));
        env.insensitive = false;
        assert_eq!(env.index("PATH"), None);
    }
}
