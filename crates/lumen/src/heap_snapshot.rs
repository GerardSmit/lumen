//! Portable initialized heap. References are node indices; host addresses only deduplicate.
//! Decode by allocating all object/environment shells before installing edges and descriptors.
use crate::ast::Function;
use crate::interpreter::Env;
use crate::value::{Callable, Exotic, Gc, Value};
use std::collections::BTreeMap;
use std::rc::Rc;

pub struct Snapshot {
    pub roots: Vec<(String, u32)>,
    pub environments: Vec<(String, u32)>,
    pub global_var_names: Vec<String>,
    pub nodes: Vec<Node>,
}

pub enum Node {
    Undefined,
    Empty,
    Null,
    Bool(bool),
    Number(u64),
    BigInt(String),
    String(String),
    Symbol {
        description: Option<String>,
    },
    Intrinsic(String),
    Object {
        intrinsic: Option<String>,
        prototype: Option<u32>,
        exotic: u8,
        extensible: bool,
        constructor: bool,
        callable: Call,
        properties: Vec<Property>,
        exports: Option<Vec<(String, Export)>>,
        class: Option<Class>,
    },
    Environment {
        parent: Option<u32>,
        flags: u8,
        with_object: Option<u32>,
        bindings: Vec<Binding>,
        lexical_names: Vec<String>,
    },
}

pub enum Call {
    None,
    CjsRequire(u32),
    Native {
        function: u32,
        environment: u32,
    },
    Bound {
        target: u32,
        this: u32,
        arguments: Vec<u32>,
    },
}

pub enum Export {
    Live { environment: u32, local: String },
    Static(u32),
}

pub struct Class {
    pub environment: u32,
    pub derived: bool,
    pub body: Option<u32>,
    pub fields: Vec<Field>,
    pub private_members: Vec<Property>,
    pub initializers: Vec<u32>,
}

pub struct Field {
    pub key: String,
    pub initializer: Option<u32>,
    pub named: bool,
    pub transforms: Vec<u32>,
}

struct SourceClass {
    environment: Env,
    derived: bool,
    fields: Vec<(String, Option<crate::ast::Expr>, Vec<Value>)>,
    private_members: Vec<(String, crate::value::Property)>,
    initializers: Vec<Value>,
}

impl crate::Engine {
    /// Save this identity table before executing the initializer. The target enumerates the
    /// same table on its fresh engine; values are compared by object/symbol identity.
    pub fn snapshot_intrinsics(&self) -> Vec<(String, Value)> {
        crate::native_ops::snapshot_intrinsics(&self.interp)
    }
    /// Capture the initialized global/module graph in this engine. Run initialization with
    /// the ordinary host eval API first; this method never evaluates source or calls getters.
    pub fn capture_initialized_heap(
        &self,
        function: impl Fn(&Rc<Function>, bool) -> Option<u32>,
        intrinsic: impl Fn(&Value) -> Option<String>,
    ) -> Result<Snapshot, String> {
        let i = &self.interp;
        #[cfg(feature = "jit")]
        if i.snapshot_cjs_require.is_some() {
            return Err("restore the CommonJS snapshot require router before capture".into());
        }
        if !i.microtasks.is_empty()
            || !i.pending_timers.is_empty()
            || !i.pending_async_waits.is_empty()
            || i.agent.is_some()
            || !i.deferred_ns.is_empty()
        {
            return Err(
                "snapshot initialization has pending jobs, timers, agents or deferred modules"
                    .into(),
            );
        }
        if i.module_recs
            .values()
            .any(|module| !module.snapshot_ready())
        {
            return Err("snapshot module initialization is incomplete or failed".into());
        }
        let mut global_var_names: Vec<_> = i.global_var_names.iter().cloned().collect();
        global_var_names.sort();
        let mut capture = Capture {
            snapshot: Snapshot {
                roots: Vec::new(),
                environments: Vec::new(),
                global_var_names,
                nodes: Vec::new(),
            },
            depth: 0,
            objects: BTreeMap::new(),
            environments: BTreeMap::new(),
            symbols: BTreeMap::new(),
            classes: i
                .class_info
                .iter()
                .map(|(&key, class)| {
                    (
                        key,
                        SourceClass {
                            environment: class.field_env.clone(),
                            derived: class.derived,
                            fields: class
                                .fields
                                .iter()
                                .map(|field| {
                                    (
                                        field.key.clone(),
                                        field.init.clone(),
                                        field.transforms.clone(),
                                    )
                                })
                                .collect(),
                            private_members: class.private_members.clone(),
                            initializers: class.instance_initializers.clone(),
                        },
                    )
                })
                .collect(),
            unsupported: i
                .map_data
                .keys()
                .chain(i.array_buffers.keys())
                .chain(i.shared_buffers.keys())
                .chain(i.typed_arrays.keys())
                .chain(i.shadow_realms.keys())
                .chain(i.data_views.keys())
                .chain(i.regexps.keys())
                .chain(i.proxies.keys())
                .chain(i.temporal.keys())
                .copied()
                .collect(),
            namespaces: i
                .module_ns
                .iter()
                .map(|(&key, exports)| {
                    let mut exports: Vec<_> = exports
                        .iter()
                        .map(|(name, binding)| (name.clone(), binding.clone()))
                        .collect();
                    exports.sort_by(|a, b| a.0.cmp(&b.0));
                    (key, exports)
                })
                .collect(),
            cjs_requires: {
                #[cfg(feature = "jit")]
                {
                    crate::snapshot_cjs::require_ids(&i.snapshot_cjs)
                }
                #[cfg(not(feature = "jit"))]
                {
                    BTreeMap::new()
                }
            },
            function,
            intrinsic: |value: &Value| {
                if let Value::Sym(symbol) = value {
                    if let Some(key) = crate::interpreter::sym_for_key_of(symbol) {
                        return Some(format!("symbol.for:{key}"));
                    }
                }
                intrinsic(value)
            },
            symbol: |key: &str| i.sym_from_key(key),
        };
        let global = capture.value(&Value::Obj(i.global.clone()))?;
        capture.snapshot.roots.push(("globalThis".into(), global));
        let environment = capture.environment(&i.global_env)?;
        capture
            .snapshot
            .environments
            .push(("global".into(), environment));
        #[cfg(feature = "jit")]
        if let Some(units) = &i.snapshot_cjs {
            for unit in units
                .borrow()
                .iter()
                .flatten()
                .filter(|unit| unit.state == 2)
            {
                let node = capture.value(&unit.module)?;
                capture
                    .snapshot
                    .roots
                    .push((format!("cjs:{}", unit.path), node));
                let environment = capture.environment(&unit.env)?;
                capture
                    .snapshot
                    .environments
                    .push((format!("cjs:{}", unit.path), environment));
            }
        }
        let modules: BTreeMap<_, _> = i.modules.iter().collect();
        for (name, namespace) in modules {
            let namespace = capture.value(namespace)?;
            capture
                .snapshot
                .roots
                .push((format!("module:{name}"), namespace));
        }
        let modules: BTreeMap<_, _> = i.module_recs.iter().collect();
        for (name, module) in modules {
            let environment = capture.environment(&module.snapshot_env())?;
            capture
                .snapshot
                .environments
                .push((format!("module:{name}"), environment));
        }
        Ok(capture.snapshot)
    }
}

pub struct Property {
    pub name: String,
    /// A symbol node replaces the host's internal symbol-key string on decode.
    pub symbol: Option<u32>,
    /// bit0 accessor, bit1 writable, bit2 enumerable, bit3 configurable.
    pub flags: u8,
    pub value: u32,
    pub getter: Option<u32>,
    pub setter: Option<u32>,
}

pub struct Binding {
    pub name: String,
    /// bit0 mutable, bit1 initialized, bit2 import, bit3 deletable, bit4 strict immutable.
    pub flags: u8,
    pub value: u32,
    pub import: Option<(u32, String)>,
}

impl Snapshot {
    /// Capture after initialization and before dropping its engine. Intrinsics must resolve to
    /// stable target-runtime names; function indices must reference emitted native metadata.
    pub fn capture(
        roots: &[(String, Value)],
        environments: &[(String, Env)],
        function: impl Fn(&Rc<Function>, bool) -> Option<u32>,
        intrinsic: impl Fn(&Value) -> Option<String>,
        symbol: impl Fn(&str) -> Option<Value>,
    ) -> Result<Self, String> {
        let mut capture = Capture {
            snapshot: Self {
                roots: Vec::new(),
                environments: Vec::new(),
                global_var_names: Vec::new(),
                nodes: Vec::new(),
            },
            depth: 0,
            objects: BTreeMap::new(),
            environments: BTreeMap::new(),
            symbols: BTreeMap::new(),
            classes: BTreeMap::new(),
            unsupported: Default::default(),
            namespaces: BTreeMap::new(),
            cjs_requires: BTreeMap::new(),
            function,
            intrinsic,
            symbol,
        };
        for (name, value) in roots {
            let node = capture.value(value)?;
            capture.snapshot.roots.push((name.clone(), node));
        }
        for (name, environment) in environments {
            let node = capture.environment(environment)?;
            capture.snapshot.environments.push((name.clone(), node));
        }
        Ok(capture.snapshot)
    }

    /// LMSNAP v1: LE u32 counts/indices, strings are length-prefixed engine string bytes.
    /// Optional indices use u32::MAX. Node tags follow the order in `Node` (0..10).
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        let mut out = b"LMSNAP\x01\0".to_vec();
        count(&mut out, self.roots.len())?;
        for (name, node) in &self.roots {
            string(&mut out, name)?;
            word(&mut out, *node);
        }
        count(&mut out, self.environments.len())?;
        for (name, node) in &self.environments {
            string(&mut out, name)?;
            word(&mut out, *node);
        }
        count(&mut out, self.global_var_names.len())?;
        for name in &self.global_var_names {
            string(&mut out, name)?;
        }
        count(&mut out, self.nodes.len())?;
        for node in &self.nodes {
            match node {
                Node::Undefined => out.push(0),
                Node::Empty => out.push(1),
                Node::Null => out.push(2),
                Node::Bool(value) => {
                    out.push(3);
                    out.push(*value as u8);
                }
                Node::Number(bits) => {
                    out.push(4);
                    out.extend_from_slice(&bits.to_le_bytes());
                }
                Node::BigInt(value) => {
                    out.push(5);
                    string(&mut out, value)?;
                }
                Node::String(value) => {
                    out.push(6);
                    string(&mut out, value)?;
                }
                Node::Symbol { description } => {
                    out.push(7);
                    out.push(description.is_some() as u8);
                    if let Some(s) = description {
                        string(&mut out, s)?;
                    }
                }
                Node::Intrinsic(name) => {
                    out.push(8);
                    string(&mut out, name)?;
                }
                Node::Object {
                    intrinsic,
                    prototype,
                    exotic,
                    extensible,
                    constructor,
                    callable,
                    properties,
                    exports,
                    class,
                } => {
                    out.push(9);
                    out.push(intrinsic.is_some() as u8);
                    if let Some(name) = intrinsic {
                        string(&mut out, name)?;
                    }
                    optional(&mut out, *prototype);
                    out.extend_from_slice(&[*exotic, *extensible as u8, *constructor as u8]);
                    match callable {
                        Call::None => out.push(0),
                        Call::CjsRequire(unit) => {
                            out.push(3);
                            word(&mut out, *unit);
                        }
                        Call::Native {
                            function,
                            environment,
                        } => {
                            out.push(1);
                            word(&mut out, *function);
                            word(&mut out, *environment);
                        }
                        Call::Bound {
                            target,
                            this,
                            arguments,
                        } => {
                            out.push(2);
                            word(&mut out, *target);
                            word(&mut out, *this);
                            count(&mut out, arguments.len())?;
                            for &a in arguments {
                                word(&mut out, a);
                            }
                        }
                    }
                    count(&mut out, properties.len())?;
                    for p in properties {
                        string(&mut out, &p.name)?;
                        optional(&mut out, p.symbol);
                        out.push(p.flags);
                        word(&mut out, p.value);
                        optional(&mut out, p.getter);
                        optional(&mut out, p.setter);
                    }
                    out.push(exports.is_some() as u8);
                    if let Some(exports) = exports {
                        count(&mut out, exports.len())?;
                        for (name, export) in exports {
                            string(&mut out, name)?;
                            match export {
                                Export::Live { environment, local } => {
                                    out.push(0);
                                    word(&mut out, *environment);
                                    string(&mut out, local)?;
                                }
                                Export::Static(value) => {
                                    out.push(1);
                                    word(&mut out, *value);
                                }
                            }
                        }
                    }
                    out.push(class.is_some() as u8);
                    if let Some(class) = class {
                        word(&mut out, class.environment);
                        out.push(class.derived as u8);
                        optional(&mut out, class.body);
                        count(&mut out, class.fields.len())?;
                        for field in &class.fields {
                            string(&mut out, &field.key)?;
                            optional(&mut out, field.initializer);
                            out.push(field.named as u8);
                            count(&mut out, field.transforms.len())?;
                            for &value in &field.transforms {
                                word(&mut out, value);
                            }
                        }
                        count(&mut out, class.private_members.len())?;
                        for p in &class.private_members {
                            string(&mut out, &p.name)?;
                            optional(&mut out, p.symbol);
                            out.push(p.flags);
                            word(&mut out, p.value);
                            optional(&mut out, p.getter);
                            optional(&mut out, p.setter);
                        }
                        count(&mut out, class.initializers.len())?;
                        for &value in &class.initializers {
                            word(&mut out, value);
                        }
                    }
                }
                Node::Environment {
                    parent,
                    flags,
                    with_object,
                    bindings,
                    lexical_names,
                } => {
                    out.push(10);
                    optional(&mut out, *parent);
                    out.push(*flags);
                    optional(&mut out, *with_object);
                    count(&mut out, bindings.len())?;
                    for b in bindings {
                        string(&mut out, &b.name)?;
                        out.push(b.flags);
                        word(&mut out, b.value);
                        optional(&mut out, b.import.as_ref().map(|i| i.0));
                        if let Some((_, name)) = &b.import {
                            string(&mut out, name)?;
                        }
                    }
                    count(&mut out, lexical_names.len())?;
                    for name in lexical_names {
                        string(&mut out, name)?;
                    }
                }
            }
        }
        Ok(out)
    }
}

fn word(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}
fn optional(out: &mut Vec<u8>, value: Option<u32>) {
    word(out, value.unwrap_or(u32::MAX));
}
fn count(out: &mut Vec<u8>, value: usize) -> Result<(), String> {
    let value = u32::try_from(value).map_err(|_| "snapshot exceeds 32-bit limits")?;
    if value == u32::MAX {
        return Err("snapshot exceeds 32-bit limits".into());
    }
    word(out, value);
    Ok(())
}
fn string(out: &mut Vec<u8>, value: &str) -> Result<(), String> {
    count(out, value.len())?;
    out.extend_from_slice(value.as_bytes());
    Ok(())
}

struct Capture<F, I, S> {
    snapshot: Snapshot,
    depth: usize,
    objects: BTreeMap<usize, u32>,
    environments: BTreeMap<usize, u32>,
    symbols: BTreeMap<u64, u32>,
    unsupported: std::collections::BTreeSet<usize>,
    namespaces: BTreeMap<usize, Vec<(String, crate::modules::NsBinding)>>,
    classes: BTreeMap<usize, SourceClass>,
    cjs_requires: BTreeMap<usize, u32>,
    function: F,
    intrinsic: I,
    symbol: S,
}

impl<
        F: Fn(&Rc<Function>, bool) -> Option<u32>,
        I: Fn(&Value) -> Option<String>,
        S: Fn(&str) -> Option<Value>,
    > Capture<F, I, S>
{
    fn reserve(&mut self) -> Result<u32, String> {
        let id = u32::try_from(self.snapshot.nodes.len())
            .map_err(|_| "snapshot exceeds 32-bit limits")?;
        if id == u32::MAX {
            return Err("snapshot exceeds 32-bit limits".into());
        }
        self.snapshot.nodes.push(Node::Undefined);
        Ok(id)
    }

    fn value(&mut self, value: &Value) -> Result<u32, String> {
        match value {
            Value::Obj(object) => return self.object(object),
            Value::Sym(symbol) => {
                if let Some(&id) = self.symbols.get(&symbol.id) {
                    return Ok(id);
                }
                let id = self.reserve()?;
                self.symbols.insert(symbol.id, id);
                self.snapshot.nodes[id as usize] = if let Some(name) = (self.intrinsic)(value) {
                    Node::Intrinsic(name)
                } else {
                    Node::Symbol {
                        description: symbol.description.as_ref().map(|s| s.to_string()),
                    }
                };
                return Ok(id);
            }
            _ => {}
        }
        let id = self.reserve()?;
        self.snapshot.nodes[id as usize] = match value {
            Value::Undefined => Node::Undefined,
            Value::Empty => Node::Empty,
            Value::Null => Node::Null,
            Value::Bool(b) => Node::Bool(*b),
            Value::Num(n) => Node::Number(n.to_bits()),
            Value::BigInt(b) => Node::BigInt(b.to_string_radix(10)),
            Value::Str(s) => Node::String(s.as_str().to_owned()),
            _ => unreachable!(),
        };
        Ok(id)
    }

    fn object(&mut self, object: &Gc) -> Result<u32, String> {
        if self.depth >= 256 {
            return Err("snapshot graph exceeds maximum traversal depth".into());
        }
        self.depth += 1;
        let result = (|| {
            let key = Gc::as_ptr(object) as usize;
            if self.unsupported.contains(&key) {
                return Err(
                    "snapshot contains an unsupported class, live handle or side-table object"
                        .into(),
                );
            }
            if let Some(&id) = self.objects.get(&key) {
                return Ok(id);
            }
            let id = self.reserve()?;
            self.objects.insert(key, id);
            let intrinsic = (self.intrinsic)(&Value::Obj(object.clone()));
            let exports = self
                .namespaces
                .get(&key)
                .cloned()
                .map(|exports| {
                    exports
                        .into_iter()
                        .map(|(name, export)| {
                            Ok((
                                name,
                                match export {
                                    crate::modules::NsBinding::Live(environment, local) => {
                                        Export::Live {
                                            environment: self.environment(&environment)?,
                                            local,
                                        }
                                    }
                                    crate::modules::NsBinding::Static(value) => {
                                        Export::Static(self.value(&value)?)
                                    }
                                },
                            ))
                        })
                        .collect::<Result<Vec<_>, String>>()
                })
                .transpose()?;
            let object = object
                .try_borrow()
                .map_err(|_| "snapshot object is mutably borrowed")?;
            let lazy_user_function =
                matches!(object.call, Callable::User(_)) && matches!(object.exotic, Exotic::None);
            if (!object.ic_plain.get()
                && exports.is_none()
                && !lazy_user_function
                && intrinsic.is_none())
                || matches!(object.exotic, Exotic::Arguments | Exotic::SplitView)
            {
                return Err(
                    "snapshot contains a live handle or unsupported side-table object".into(),
                );
            }
            let source_class = self.classes.remove(&key).filter(|_| intrinsic.is_none());
            let callable = if intrinsic.is_some() {
                Call::None
            } else if let Some(unit) = self.cjs_requires.get(&key) {
                Call::CjsRequire(*unit)
            } else {
                match &object.call {
            Callable::None => Call::None,
            Callable::User(user) => Call::Native {
                function: (self.function)(&user.func, source_class.as_ref().is_some_and(|class| class.derived)).ok_or("snapshot closure has no native function index")?,
                environment: self.environment(&user.env)?,
            },
            Callable::Bound(bound) => Call::Bound { target: self.object(&bound.target)?, this: self.value(&bound.this)?,
                arguments: bound.args.iter().map(|v| self.value(v)).collect::<Result<_, _>>()? },
            Callable::Native(_) | Callable::NativeData(_) if source_class.is_some() => Call::None,
            _ => return Err("snapshot contains an unregistered intrinsic, live handle or unsupported callable".into()),
        }
            };
            let prototype = object.proto.as_ref().map(|p| self.object(p)).transpose()?;
            let mut properties = Vec::new();
            for name in object.props.keys() {
                let property = object
                    .props
                    .get(&name)
                    .ok_or("snapshot property disappeared")?;
                let symbol = if crate::interpreter::Interp::is_sym_key(&name) {
                    let value =
                        (self.symbol)(&name).ok_or("snapshot symbol key is unregistered")?;
                    Some(self.value(&value)?)
                } else {
                    None
                };
                properties.push(Property {
                    name: if symbol.is_some() {
                        String::new()
                    } else {
                        name.to_string()
                    },
                    symbol,
                    flags: property.accessor() as u8
                        | (property.writable() as u8) << 1
                        | (property.enumerable() as u8) << 2
                        | (property.configurable() as u8) << 3,
                    value: self.value(&property.value())?,
                    getter: property.getter().map(|v| self.value(v)).transpose()?,
                    setter: property.setter().map(|v| self.value(v)).transpose()?,
                });
            }
            let class = source_class
                .map(|source| {
                    let env = crate::interpreter::new_scope(Some(source.environment));
                    crate::eval::bind(&env, "%fieldinit%", Value::Bool(true));
                    let mut fields = Vec::new();
                    for (key, expression, transforms) in source.fields {
                        let named = expression
                            .as_ref()
                            .is_some_and(crate::eval::is_anonymous_fn);
                        let initializer = expression
                            .map(|expression| {
                                let function = initializer_function(expression);
                                (self.function)(&function, false).ok_or(
                                    "snapshot field initializer has no native function index"
                                        .to_owned(),
                                )
                            })
                            .transpose()?;
                        fields.push(Field {
                            key,
                            initializer,
                            named,
                            transforms: transforms
                                .iter()
                                .map(|v| self.value(v))
                                .collect::<Result<_, _>>()?,
                        });
                    }
                    let mut private_members = Vec::new();
                    for (name, property) in source.private_members {
                        private_members.push(Property {
                            name,
                            symbol: None,
                            flags: property.accessor() as u8
                                | (property.writable() as u8) << 1
                                | (property.enumerable() as u8) << 2
                                | (property.configurable() as u8) << 3,
                            value: self.value(&property.value())?,
                            getter: property.getter().map(|v| self.value(v)).transpose()?,
                            setter: property.setter().map(|v| self.value(v)).transpose()?,
                        });
                    }
                    Ok::<_, String>(Class {
                        environment: self.environment(&env)?,
                        derived: source.derived,
                        body: match callable {
                            Call::Native { function, .. } => Some(function),
                            _ => None,
                        },
                        fields,
                        private_members,
                        initializers: source
                            .initializers
                            .iter()
                            .map(|v| self.value(v))
                            .collect::<Result<_, _>>()?,
                    })
                })
                .transpose()?;
            self.snapshot.nodes[id as usize] = Node::Object {
                intrinsic,
                prototype,
                exotic: object.exotic as u8,
                extensible: object.extensible,
                constructor: object.is_constructor,
                callable,
                properties,
                exports,
                class,
            };
            Ok(id)
        })();
        self.depth -= 1;
        result
    }

    fn environment(&mut self, environment: &Env) -> Result<u32, String> {
        if self.depth >= 256 {
            return Err("snapshot graph exceeds maximum traversal depth".into());
        }
        self.depth += 1;
        let result = (|| {
            let key = Rc::as_ptr(environment) as usize;
            if let Some(&id) = self.environments.get(&key) {
                return Ok(id);
            }
            let id = self.reserve()?;
            self.environments.insert(key, id);
            let environment = environment
                .try_borrow()
                .map_err(|_| "snapshot environment is mutably borrowed")?;
            if environment.under_with() {
                return Err("snapshot contains an unsupported with environment".into());
            }
            let parent = environment
                .parent
                .as_ref()
                .map(|p| self.environment(p))
                .transpose()?;
            let with_object = environment.with_obj().map(|v| self.value(v)).transpose()?;
            let mut bindings = Vec::new();
            for (name, binding) in environment.vars.iter() {
                let import = if binding.import {
                    let (target, local) = environment
                        .import_of(name)
                        .ok_or("snapshot import has no target")?;
                    Some((self.environment(target)?, local.clone()))
                } else {
                    None
                };
                bindings.push(Binding {
                    name: name.to_string(),
                    flags: binding.mutable as u8
                        | (binding.initialized as u8) << 1
                        | (binding.import as u8) << 2
                        | (binding.deletable as u8) << 3
                        | (binding.strict_immutable as u8) << 4,
                    value: self.value(&binding.value)?,
                    import,
                });
            }
            self.snapshot.nodes[id as usize] = Node::Environment {
                parent,
                flags: environment.vars.scope_flags(),
                with_object,
                bindings,
                lexical_names: environment
                    .snapshot_lexical_names()
                    .iter()
                    .map(|name| name.to_string())
                    .collect(),
            };
            Ok(id)
        })();
        self.depth -= 1;
        result
    }
}

fn initializer_function(expression: crate::ast::Expr) -> Rc<Function> {
    use std::cell::{Cell, OnceCell, RefCell};
    Rc::new(Function {
        name: Some("<field-initializer>".into()),
        params: Vec::new(),
        body: RefCell::new(Some(Rc::new(vec![crate::ast::Stmt::Return(Some(
            expression,
        ))]))),
        lazy: RefCell::new(None),
        lazy_error: OnceCell::new(),
        is_arrow: false,
        is_strict: true,
        expr_body: false,
        is_generator: false,
        is_async: false,
        is_method: true,
        is_fn_expr: false,
        source: crate::ast::FnSource::None,
        scan: Cell::new(0),
        hoist: RefCell::new(None),
        body_used: Cell::new(false),
        calls: Cell::new(0),
        code: OnceCell::new(),
        fn_maps: OnceCell::new(),
    })
}
