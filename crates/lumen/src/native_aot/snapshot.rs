//! Portable initialized-heap graph reader. No host pointers enter this format.

pub(super) struct Snapshot {
    pub roots: Vec<(String, u32)>,
    pub environments: Vec<(String, u32)>,
    pub global_var_names: Vec<String>,
    pub nodes: Vec<Node>,
}

pub(super) struct Restored {
    pub roots: std::collections::BTreeMap<String, crate::value::Value>,
    pub environments: std::collections::BTreeMap<String, crate::interpreter::Env>,
    pub functions: std::collections::BTreeMap<u32, Vec<crate::interpreter::Env>>,
}

enum Shell {
    Value(crate::value::Value),
    Environment(crate::interpreter::Env),
}

fn value(shells: &[Shell], index: u32) -> Result<crate::value::Value, String> {
    match shells.get(index as usize) {
        Some(Shell::Value(value)) => Ok(value.clone()),
        _ => Err("snapshot value references an environment".into()),
    }
}

fn environment(shells: &[Shell], index: u32) -> Result<crate::interpreter::Env, String> {
    match shells.get(index as usize) {
        Some(Shell::Environment(environment)) => Ok(environment.clone()),
        _ => Err("snapshot environment reference is invalid".into()),
    }
}

pub(super) fn restore(
    interp: &mut crate::interpreter::Interp,
    program: &std::rc::Rc<super::NativeProgram>,
    snapshot: &Snapshot,
) -> Result<Restored, String> {
    use crate::interpreter::{new_scope, Binding as RuntimeBinding};
    use crate::value::{AotCallable, BoundCallable, Callable, Exotic, Gc, Object, Property as RuntimeProperty, Props, Value};
    use std::collections::{BTreeMap, BTreeSet};
    use std::rc::Rc;
    let intrinsics: BTreeMap<_, _> = crate::native_ops::snapshot_intrinsics(interp).into_iter().collect();
    let global_environment = snapshot.environments.iter().find(|(name, _)| name == "global").map(|(_, id)| *id);
    let mut used_intrinsics = BTreeSet::new();
    let mut shells = Vec::with_capacity(snapshot.nodes.len());
    for (index, node) in snapshot.nodes.iter().enumerate() {
        let shell = match node {
            Node::Environment { .. } => Shell::Environment(if global_environment == Some(index as u32) { interp.global_env.clone() } else { new_scope(None) }),
            Node::Object { intrinsic: Some(name), callable, .. } => {
                if !matches!(callable, Call::None) || !used_intrinsics.insert(name) {
                    return Err("snapshot intrinsic identity is duplicated or overrides its callable".into());
                }
                let value = intrinsics.get(name).ok_or("snapshot intrinsic is unavailable on this runtime")?.clone();
                if !matches!(value, Value::Obj(_)) { return Err("snapshot object intrinsic is not an object".into()); }
                Shell::Value(value)
            }
            Node::Object { .. } => Shell::Value(Value::Obj(Object::new(None))),
            Node::Undefined => Shell::Value(Value::Undefined),
            Node::Empty => Shell::Value(Value::Empty),
            Node::Null => Shell::Value(Value::Null),
            Node::Bool(b) => Shell::Value(Value::Bool(*b)),
            Node::Number(bits) => Shell::Value(Value::Num(f64::from_bits(*bits))),
            Node::BigInt(number) => Shell::Value(Value::BigInt(crate::bigint::JsBigInt::parse_radix(number, 10).ok_or("invalid snapshot BigInt")?)),
            Node::String(string) => Shell::Value(Value::str(string)),
            Node::Symbol(description) => Shell::Value(interp.new_symbol(description.as_deref().map(Rc::from))),
            Node::Intrinsic(name) => {
                let value = if let Some(key) = name.strip_prefix("symbol.for:") {
                    let symbol = if let Some(symbol) = crate::interpreter::sym_for_get(key) { symbol } else {
                        let Value::Sym(symbol) = interp.new_symbol(Some(Rc::from(key))) else { unreachable!() };
                        crate::interpreter::sym_for_insert(key.to_owned(), symbol.clone()); symbol
                    };
                    interp.sym_registry.insert(symbol.id, symbol.clone()); Value::Sym(symbol)
                } else { intrinsics.get(name).ok_or("snapshot symbol intrinsic is unavailable")?.clone() };
                if !matches!(value, Value::Sym(_)) { return Err("snapshot symbol intrinsic is not a symbol".into()); }
                Shell::Value(value)
            }
        };
        shells.push(shell);
    }
    // Validate descriptor/namespace value edges before changing existing intrinsic objects.
    for node in &snapshot.nodes {
        match node {
            Node::Object { properties, namespace, callable, class, intrinsic, constructor, .. } => {
                let mut keys = BTreeSet::new();
                for property in properties.iter().chain(class.iter().flat_map(|class| class.private_members.iter())) {
                    let key = property_key(&shells, property)?;
                    if !keys.insert(key) { return Err("duplicate snapshot property key".into()); }
                    value(&shells, property.value)?;
                    if property.flags & 1 == 0 && (property.getter.is_some() || property.setter.is_some()) {
                        return Err("snapshot data property has accessor functions".into());
                    }
                    for index in [property.getter, property.setter].into_iter().flatten() {
                        let getter = value(&shells, index)?;
                        let Value::Obj(_) = getter else { return Err("snapshot accessor is not callable".into()) };
                        if !snapshot_callable(snapshot, &shells, index) { return Err("snapshot accessor is not callable".into()); }
                    }
                }
                if let Call::CjsRequire(unit) = callable {
                    if program.metadata.units.get(*unit as usize).is_none_or(|record| record.kind != 2) { return Err("snapshot require references a non-CommonJS unit".into()); }
                }
                if let Call::Bound(target, this, args) = callable {
                    if !snapshot_callable(snapshot, &shells, *target) { return Err("snapshot bound target is not callable".into()); }
                    value(&shells, *this)?; for &arg in args { value(&shells, arg)?; }
                }
                if let Some(exports) = namespace {
                    let mut names = BTreeSet::new();
                    for (name, binding) in exports {
                        if !names.insert(name) { return Err("duplicate snapshot namespace export".into()); }
                        if let NamespaceBinding::Static(index) = binding { value(&shells, *index)?; }
                    }
                }
                if let Some(class) = class {
                    if intrinsic.is_some() || namespace.is_some() || !*constructor { return Err("invalid snapshot class object".into()); }
                    if class.derived && class.body.is_some_and(|id| program.metadata.functions[id as usize].frame_flags & 4 == 0) {
                        return Err("snapshot derived constructor lacks derived native metadata".into());
                    }
                    for &id in class.initializers.iter().chain(class.fields.iter().flat_map(|field| field.transforms.iter())) {
                        if !snapshot_callable(snapshot, &shells, id) { return Err("snapshot class initializer is not callable".into()); }
                    }
                }
            }
            Node::Environment { with_object, bindings, .. } => {
                if let Some(index) = with_object { value(&shells, *index)?; }
                let mut names = BTreeSet::new();
                for binding in bindings {
                    if !names.insert(&binding.name) || (binding.flags & 4 != 0) != binding.import.is_some() {
                        return Err("invalid or duplicate snapshot binding".into());
                    }
                    value(&shells, binding.value)?;
                }
            }
            _ => {}
        }
    }
    let mut root_names = BTreeSet::new();
    for (name, id) in &snapshot.roots {
        if !root_names.insert(name) { return Err("duplicate snapshot root".into()); }
        let root = value(&shells, *id)?;
        if name == "globalThis" && !matches!(root, Value::Obj(ref global) if Gc::ptr_eq(global, &interp.global)) {
            return Err("snapshot global root does not reference the global intrinsic".into());
        }
    }
    let mut environment_names = BTreeSet::new();
    for (name, _) in &snapshot.environments {
        if !environment_names.insert(name) { return Err("duplicate snapshot environment root".into()); }
    }
    for (index, node) in snapshot.nodes.iter().enumerate() {
        if let Node::Environment { parent, flags, with_object, bindings, lexical_names } = node {
            let environment = environment(&shells, index as u32)?;
            let mut scope = environment.try_borrow_mut().map_err(|_| "snapshot environment is borrowed")?;
            scope.restore_snapshot_state(parent.map(|id| self::environment(&shells, id)).transpose()?, *flags,
                with_object.map(|id| value(&shells, id)).transpose()?, lexical_names.iter().map(|name| Rc::from(name.as_str())).collect());
            for binding in bindings {
                if let Some((target, local)) = &binding.import { scope.link_import(&binding.name, self::environment(&shells, *target)?, local.clone()); }
                scope.vars.insert(binding.name.as_str(), RuntimeBinding { value: value(&shells, binding.value)?,
                    mutable: binding.flags & 1 != 0, initialized: binding.flags & 2 != 0,
                    import: binding.flags & 4 != 0, deletable: binding.flags & 8 != 0, strict_immutable: binding.flags & 16 != 0 });
            }
        }
    }
    for (index, node) in snapshot.nodes.iter().enumerate() {
        let Node::Object { intrinsic, prototype, exotic, extensible, constructor, callable, properties, namespace, class } = node else { continue };
        let Value::Obj(object) = value(&shells, index as u32)? else { unreachable!() };
        let mut props = Props::new();
        for property in properties {
            let descriptor = if property.flags & 1 != 0 {
                RuntimeProperty::accessor_prop(property.getter.map(|id| value(&shells, id)).transpose()?,
                    property.setter.map(|id| value(&shells, id)).transpose()?, property.flags & 4 != 0, property.flags & 8 != 0)
            } else { RuntimeProperty::data(value(&shells, property.value)?, property.flags & 2 != 0, property.flags & 4 != 0, property.flags & 8 != 0) };
            props.insert(property_key(&shells, property)?, descriptor);
        }
        let mut object_ref = object.try_borrow_mut().map_err(|_| "snapshot intrinsic object is borrowed")?;
        object_ref.proto = prototype.map(|id| value(&shells, id).and_then(|v| v.as_obj().cloned().ok_or("invalid snapshot prototype".into()))).transpose()?;
        object_ref.props = props;
        object_ref.exotic = match exotic { 0 => Exotic::None, 1 => Exotic::Array, 2 => Exotic::BoolWrap,
            3 => Exotic::NumWrap, 4 => Exotic::StrWrap, 5 => Exotic::SymWrap, 6 => Exotic::BigIntWrap, 7 => Exotic::Error,
            _ => return Err("unsupported snapshot exotic".into()) };
        object_ref.extensible = *extensible;
        object_ref.is_constructor = *constructor;
        object_ref.ic_plain.set(namespace.is_none());
        if intrinsic.is_none() {
            object_ref.call = match callable {
                Call::None => Callable::None,
                Call::CjsRequire(unit) => {
                    let require = super::modules::make_require(interp, program, *unit)?;
                    let call = std::mem::replace(&mut require.as_obj().unwrap().borrow_mut().call, Callable::None);
                    call
                }
                Call::Native(function, env) => Callable::Aot(Box::new(AotCallable { program: program.clone(), function_index: *function, env: environment(&shells, *env)? })),
                Call::Bound(target, this, args) => Callable::Bound(Box::new(BoundCallable {
                    target: value(&shells, *target)?.as_obj().cloned().ok_or("invalid snapshot bound target")?,
                    this: value(&shells, *this)?, args: args.iter().map(|&id| value(&shells, id)).collect::<Result<_, _>>()? })),
            };
        }
        drop(object_ref);
        if let Some(exports) = namespace {
            let mut bindings = crate::fasthash::FastMap::default();
            for (name, binding) in exports { bindings.insert(name.clone(), match binding {
                NamespaceBinding::Live(env, local) => crate::modules::NsBinding::Live(environment(&shells, *env)?, local.clone()),
                NamespaceBinding::Static(id) => crate::modules::NsBinding::Static(value(&shells, *id)?),
            }); }
            interp.module_ns.insert(Gc::as_ptr(&object) as usize, bindings);
        }
        for property in properties { super::classes::restore_private_key(interp, &property.name)?; }
        if let Some(class) = class {
            let fields = class.fields.iter().map(|field| Ok(super::classes::Field { key: field.key.clone(), initializer: field.initializer,
                named: field.named, transforms: field.transforms.iter().map(|&id| value(&shells, id)).collect::<Result<_, _>>()? })).collect::<Result<_, String>>()?;
            let private_members = class.private_members.iter().map(|property| {
                let descriptor = if property.flags & 1 != 0 {
                    RuntimeProperty::accessor_prop(property.getter.map(|id| value(&shells, id)).transpose()?,
                        property.setter.map(|id| value(&shells, id)).transpose()?, property.flags & 4 != 0, property.flags & 8 != 0)
                } else { RuntimeProperty::data(value(&shells, property.value)?, property.flags & 2 != 0, property.flags & 4 != 0, property.flags & 8 != 0) };
                Ok((property.name.clone(), descriptor))
            }).collect::<Result<_, String>>()?;
            super::classes::restore_class(interp, &object, super::classes::NativeClass { program: program.clone(),
                env: environment(&shells, class.environment)?, derived: class.derived, body: class.body, fields, private_members,
                initializers: class.initializers.iter().map(|&id| value(&shells, id)).collect::<Result<_, _>>()? })?;
        }
    }
    let mut roots = BTreeMap::new();
    for (name, id) in &snapshot.roots {
        let root = value(&shells, *id)?;
        if name == "globalThis" {
            let Value::Obj(global) = &root else { return Err("snapshot global root is not an object".into()) };
            if !Gc::ptr_eq(global, &interp.global) { return Err("snapshot global root does not reference the global intrinsic".into()); }
        }
        if let Some(key) = name.strip_prefix("module:") { interp.modules.insert(key.to_owned(), root.clone()); }
        if roots.insert(name.clone(), root).is_some() { return Err("duplicate snapshot root".into()); }
    }
    let mut environments = BTreeMap::new();
    for (name, id) in &snapshot.environments {
        if environments.insert(name.clone(), environment(&shells, *id)?).is_some() { return Err("duplicate snapshot environment root".into()); }
    }
    interp.global_var_names = snapshot.global_var_names.iter().cloned().collect();
    let mut functions: BTreeMap<u32, Vec<crate::interpreter::Env>> = BTreeMap::new();
    for node in &snapshot.nodes {
        if let Node::Object { callable: Call::Native(function, env), .. } = node {
            let env = environment(&shells, *env)?;
            let entries = functions.entry(*function).or_default();
            if !entries.iter().any(|known| Rc::ptr_eq(known, &env)) { entries.push(env); }
        }
    }
    Ok(Restored { roots, environments, functions })
}

fn property_key(shells: &[Shell], property: &Property) -> Result<String, String> {
    if let Some(id) = property.symbol {
        let crate::value::Value::Sym(symbol) = value(shells, id)? else { return Err("snapshot property key is not a symbol".into()) };
        Ok(crate::interpreter::Interp::sym_key(&symbol))
    } else { Ok(property.name.clone()) }
}

fn snapshot_callable(snapshot: &Snapshot, shells: &[Shell], id: u32) -> bool {
    match snapshot.nodes.get(id as usize) {
        Some(Node::Object { intrinsic: Some(_), .. }) => value(shells, id).is_ok_and(|value| value.is_callable()),
        Some(Node::Object { callable: Call::Native(..) | Call::Bound(..) | Call::CjsRequire(..), .. }) | Some(Node::Object { class: Some(_), .. }) => true,
        _ => false,
    }
}

pub(super) enum Node {
    Undefined,
    Empty,
    Null,
    Bool(bool),
    Number(u64),
    BigInt(String),
    String(String),
    Symbol(Option<String>),
    Intrinsic(String),
    Object {
        intrinsic: Option<String>,
        prototype: Option<u32>,
        exotic: u8,
        extensible: bool,
        constructor: bool,
        callable: Call,
        properties: Vec<Property>,
        namespace: Option<Vec<(String, NamespaceBinding)>>,
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

pub(super) enum Call {
    None,
    CjsRequire(u32),
    Native(u32, u32),
    Bound(u32, u32, Vec<u32>),
}

pub(super) struct Property {
    pub name: String,
    pub symbol: Option<u32>,
    pub flags: u8,
    pub value: u32,
    pub getter: Option<u32>,
    pub setter: Option<u32>,
}

pub(super) enum NamespaceBinding {
    Live(u32, String),
    Static(u32),
}

pub(super) struct Binding {
    pub name: String,
    pub flags: u8,
    pub value: u32,
    pub import: Option<(u32, String)>,
}

pub(super) struct Class {
    environment: u32, derived: bool, body: Option<u32>,
    fields: Vec<Field>, private_members: Vec<Property>, initializers: Vec<u32>,
}

pub(super) struct Field {
    key: String, initializer: Option<u32>, named: bool, transforms: Vec<u32>,
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8], String> {
        let end = self.at.checked_add(len).ok_or("snapshot is too large")?;
        let result = self.bytes.get(self.at..end).ok_or("truncated snapshot")?;
        self.at = end;
        Ok(result)
    }

    fn byte(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn word(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn count(&mut self, min_bytes: usize) -> Result<usize, String> {
        let count = self.word()? as usize;
        if count > self.bytes.len().saturating_sub(self.at) / min_bytes {
            return Err("snapshot count exceeds input".into());
        }
        Ok(count)
    }

    fn string(&mut self) -> Result<String, String> {
        let len = self.word()? as usize;
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| "invalid snapshot string".into())
    }

    fn optional(&mut self) -> Result<Option<u32>, String> {
        let value = self.word()?;
        Ok((value != u32::MAX).then_some(value))
    }

    fn flag(&mut self) -> Result<bool, String> {
        match self.byte()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err("invalid snapshot flag".into()),
        }
    }

    fn references(&mut self) -> Result<Vec<u32>, String> {
        let count = self.count(4)?;
        (0..count).map(|_| self.word()).collect()
    }
}

pub(super) fn decode(bytes: &[u8], functions: usize) -> Result<Snapshot, String> {
    let mut reader = Reader { bytes, at: 0 };
    if reader.take(8)? != b"LMSNAP\x01\0" {
        return Err("unsupported initialized-heap snapshot".into());
    }
    let root_count = reader.count(8)?;
    let mut roots = Vec::with_capacity(root_count);
    for _ in 0..root_count {
        roots.push((reader.string()?, reader.word()?));
    }
    let environment_count = reader.count(8)?;
    let mut environments = Vec::with_capacity(environment_count);
    for _ in 0..environment_count {
        environments.push((reader.string()?, reader.word()?));
    }
    let global_count = reader.count(4)?;
    let mut global_var_names = Vec::with_capacity(global_count);
    for _ in 0..global_count { global_var_names.push(reader.string()?); }
    let node_count = reader.count(1)?;
    let mut nodes = Vec::with_capacity(node_count);
    for _ in 0..node_count {
        nodes.push(match reader.byte()? {
            0 => Node::Undefined,
            1 => Node::Empty,
            2 => Node::Null,
            3 => Node::Bool(reader.flag()?),
            4 => Node::Number(u64::from_le_bytes(reader.take(8)?.try_into().unwrap())),
            5 => Node::BigInt(reader.string()?),
            6 => Node::String(reader.string()?),
            7 => {
                let has_description = reader.flag()?;
                Node::Symbol(has_description.then(|| reader.string()).transpose()?)
            }
            8 => Node::Intrinsic(reader.string()?),
            9 => {
                let has_intrinsic = reader.flag()?;
                let intrinsic = has_intrinsic.then(|| reader.string()).transpose()?;
                let prototype = reader.optional()?;
                let exotic = reader.byte()?;
                let extensible = reader.flag()?;
                let constructor = reader.flag()?;
                let callable = match reader.byte()? {
                    0 => Call::None,
                    1 => Call::Native(reader.word()?, reader.word()?),
                    2 => Call::Bound(reader.word()?, reader.word()?, reader.references()?),
                    3 => Call::CjsRequire(reader.word()?),
                    _ => return Err("unknown snapshot call kind".into()),
                };
                let property_count = reader.count(17)?;
                let mut properties = Vec::with_capacity(property_count);
                for _ in 0..property_count {
                    properties.push(Property {
                        name: reader.string()?,
                        symbol: reader.optional()?,
                        flags: reader.byte()?,
                        value: reader.word()?,
                        getter: reader.optional()?,
                        setter: reader.optional()?,
                    });
                }
                let has_namespace = reader.flag()?;
                let namespace_count = if has_namespace { reader.count(9)? } else { 0 };
                let mut namespace = Vec::with_capacity(namespace_count);
                for _ in 0..namespace_count {
                    let name = reader.string()?;
                    let binding = match reader.byte()? {
                        0 => NamespaceBinding::Live(reader.word()?, reader.string()?),
                        1 => NamespaceBinding::Static(reader.word()?),
                        _ => return Err("unknown snapshot namespace binding".into()),
                    };
                    namespace.push((name, binding));
                }
                let class = if reader.flag()? {
                    let environment = reader.word()?;
                    let derived = reader.flag()?;
                    let body = reader.optional()?;
                    let field_count = reader.count(13)?;
                    let mut fields = Vec::with_capacity(field_count);
                    for _ in 0..field_count {
                        fields.push(Field { key: reader.string()?, initializer: reader.optional()?, named: reader.flag()?, transforms: reader.references()? });
                    }
                    let private_count = reader.count(21)?;
                    let mut private_members = Vec::with_capacity(private_count);
                    for _ in 0..private_count {
                        private_members.push(Property { name: reader.string()?, symbol: reader.optional()?, flags: reader.byte()?,
                            value: reader.word()?, getter: reader.optional()?, setter: reader.optional()? });
                    }
                    Some(Class { environment, derived, body, fields, private_members, initializers: reader.references()? })
                } else { None };
                Node::Object {
                    intrinsic,
                    prototype,
                    exotic,
                    extensible,
                    constructor,
                    callable,
                    properties,
                    namespace: has_namespace.then_some(namespace),
                    class,
                }
            }
            10 => {
                let parent = reader.optional()?;
                let flags = reader.byte()?;
                let with_object = reader.optional()?;
                let binding_count = reader.count(13)?;
                let mut bindings = Vec::with_capacity(binding_count);
                for _ in 0..binding_count {
                    let name = reader.string()?;
                    let flags = reader.byte()?;
                    let value = reader.word()?;
                    let target = reader.optional()?;
                    let import = target
                        .map(|target| reader.string().map(|local| (target, local)))
                        .transpose()?;
                    bindings.push(Binding {
                        name,
                        flags,
                        value,
                        import,
                    });
                }
                let lexical_count = reader.count(4)?;
                let mut lexical_names = Vec::with_capacity(lexical_count);
                for _ in 0..lexical_count { lexical_names.push(reader.string()?); }
                Node::Environment {
                    parent,
                    flags,
                    with_object,
                    bindings,
                    lexical_names,
                }
            }
            _ => return Err("unknown snapshot node kind".into()),
        });
    }
    if reader.at != bytes.len() {
        return Err("trailing initialized-heap bytes".into());
    }
    let get = |index: u32| {
        nodes
            .get(index as usize)
            .ok_or("snapshot reference out of range")
    };
    for (_, index) in &roots {
        get(*index)?;
    }
    for (_, index) in &environments {
        if !matches!(get(*index)?, Node::Environment { .. }) {
            return Err("snapshot environment root is not an environment".into());
        }
    }
    for node in &nodes {
        match node {
            Node::Object {
                prototype,
                exotic,
                callable,
                properties,
                namespace,
                class,
                ..
            } => {
                if *exotic > 7 {
                    return Err("unsupported snapshot exotic".into());
                }
                if let Some(index) = prototype {
                    if !matches!(get(*index)?, Node::Object { .. }) {
                        return Err("snapshot prototype is not an object".into());
                    }
                }
                match callable {
                    Call::None => {}
                    Call::CjsRequire(_) => {}
                    Call::Native(function, env) => {
                        if *function as usize >= functions
                            || !matches!(get(*env)?, Node::Environment { .. })
                        {
                            return Err("invalid snapshot native closure".into());
                        }
                    }
                    Call::Bound(target, this, args) => {
                        if !matches!(get(*target)?, Node::Object { .. }) {
                            return Err("invalid snapshot bound target".into());
                        }
                        get(*this)?;
                        for index in args {
                            get(*index)?;
                        }
                    }
                }
                for property in properties {
                    if property.flags & !0x0f != 0 || property.flags & 3 == 3 {
                        return Err("invalid snapshot property flags".into());
                    }
                    get(property.value)?;
                    if let Some(index) = property.symbol {
                        if !matches!(get(index)?, Node::Symbol(_) | Node::Intrinsic(_)) {
                            return Err("snapshot property key is not a symbol".into());
                        }
                    }
                    for index in [property.getter, property.setter].into_iter().flatten() {
                        get(index)?;
                    }
                }
                for (_, binding) in namespace.iter().flatten() {
                    match binding {
                        NamespaceBinding::Live(env, _)
                            if !matches!(get(*env)?, Node::Environment { .. }) =>
                        {
                            return Err("invalid snapshot namespace environment".into())
                        }
                        NamespaceBinding::Static(value) => {
                            get(*value)?;
                        }
                        _ => {}
                    }
                }
                if let Some(class) = class {
                    if !matches!(get(class.environment)?, Node::Environment { .. }) { return Err("invalid snapshot class environment".into()); }
                    if class.body.is_some_and(|index| index as usize >= functions) { return Err("invalid snapshot class body".into()); }
                    for field in &class.fields {
                        if field.initializer.is_some_and(|index| index as usize >= functions) { return Err("invalid snapshot field initializer".into()); }
                        for &index in &field.transforms { get(index)?; }
                    }
                    for property in &class.private_members {
                        if property.flags & !0xf != 0 || property.flags & 3 == 3 || property.symbol.is_some() { return Err("invalid snapshot private member".into()); }
                        get(property.value)?;
                        for index in [property.getter, property.setter].into_iter().flatten() { get(index)?; }
                    }
                    for &index in &class.initializers { get(index)?; }
                }
            }
            Node::Environment {
                parent,
                flags,
                with_object,
                bindings,
                lexical_names,
            } => {
                if *flags & !7 != 0 {
                    return Err("unsupported snapshot scope flags".into());
                }
                if *flags & crate::interpreter::SCOPE_UNDER_WITH != 0 || with_object.is_some() {
                    return Err("unsupported snapshot with environment".into());
                }
                if let Some(parent) = parent {
                    if !matches!(get(*parent)?, Node::Environment { .. }) {
                        return Err("invalid snapshot parent environment".into());
                    }
                }
                if let Some(value) = with_object {
                    get(*value)?;
                }
                for binding in bindings {
                    if binding.flags & !0x1f != 0 {
                        return Err("invalid snapshot binding flags".into());
                    }
                    get(binding.value)?;
                    if let Some((env, _)) = &binding.import {
                        if !matches!(get(*env)?, Node::Environment { .. }) {
                            return Err("invalid snapshot import environment".into());
                        }
                    }
                }
                if lexical_names.iter().collect::<std::collections::BTreeSet<_>>().len() != lexical_names.len() {
                    return Err("duplicate snapshot lexical declaration".into());
                }
            }
            _ => {}
        }
    }
    // Property/capture cycles are valid; prototype and lexical-parent cycles are not.
    let mut completed = std::collections::BTreeSet::new();
    for (start, node) in nodes.iter().enumerate() {
        let mut seen = std::collections::BTreeSet::new();
        let mut next = Some(start as u32);
        while let Some(index) = next {
            if completed.contains(&index) { break; }
            if !seen.insert(index) { return Err("cyclic snapshot prototype or environment parent".into()); }
            next = match (node, get(index)?) {
                (Node::Object { .. }, Node::Object { prototype, .. }) => *prototype,
                (Node::Environment { .. }, Node::Environment { parent, .. }) => *parent,
                _ => None,
            };
        }
        completed.extend(seen);
    }
    if global_var_names.iter().collect::<std::collections::BTreeSet<_>>().len() != global_var_names.len() {
        return Err("duplicate snapshot global declaration".into());
    }
    Ok(Snapshot {
        roots,
        environments,
        global_var_names,
        nodes,
    })
}
