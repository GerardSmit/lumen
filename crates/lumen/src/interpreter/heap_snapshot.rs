//! A V8-format `.heapsnapshot` of this thread's heap, for `v8.writeHeapSnapshot` /
//! `v8.getHeapSnapshot` and the tools that read them (Chrome DevTools' Memory panel, Node's
//! `test/common/heap.js`).
//!
//! The graph is the collector's: every live object and scope (a closure's captured environment
//! is a V8 "system / Context" with a `context` edge per variable), the strings they hold, and
//! the side tables that hold object edges (collections, proxies, class field environments).
//! "(GC roots)" are the nodes the collector treats as roots: those with references from outside
//! the heap graph (the Rust stack, the interpreter's own fields, registries). The snapshot is
//! written in three walks over the graph (count, nodes, edges) straight to the output, so the
//! only extra memory is the node index (a map entry per node) and the string table.

use super::{Callable, Env, Interp, Scope};
use crate::fasthash::FastMap;
use crate::lstr::LStr;
use crate::value::{self, Gc, ObjCell, Object, Value};
use std::io::{self, Write};
use std::rc::Rc;

const NODE_TYPES: &[&str] = &[
    "hidden",
    "array",
    "string",
    "object",
    "code",
    "closure",
    "regexp",
    "number",
    "native",
    "synthetic",
    "concatenated string",
    "sliced string",
    "symbol",
    "bigint",
    "object shape",
];
const T_STRING: u8 = 2;
const T_OBJECT: u8 = 3;
const T_CLOSURE: u8 = 5;
const T_SYNTHETIC: u8 = 9;
const T_SLICED: u8 = 11;

const EDGE_TYPES: &[&str] = &[
    "context", "element", "property", "internal", "hidden", "shortcut", "weak",
];
const E_CONTEXT: u8 = 0;
const E_ELEMENT: u8 = 1;
const E_PROPERTY: u8 = 2;
const E_INTERNAL: u8 = 3;
const E_SHORTCUT: u8 = 5;
const E_WEAK: u8 = 6;

const NODE_FIELDS: u32 = 7;
/// Longest string-node name written (V8 truncates long string contents too).
const MAX_STRING_NAME: usize = 1024;

const ROOT: u32 = 0;
const GC_ROOTS: u32 = 1;
const FIRST_OBJECT: u32 = 2;

enum Name {
    Static(&'static str),
    Key(Rc<str>),
    Owned(String),
    Index(u32),
}

enum Target {
    Obj(*const ObjCell),
    Scope(*const std::cell::RefCell<Scope>),
    Str(LStr),
    Node(u32),
}

struct Edge {
    kind: u8,
    name: Name,
    target: Target,
    /// Whether the edge is a strong reference the source owns (counted against the target's
    /// reference count to find roots).
    owned: bool,
}

struct Graph {
    objects: Vec<Gc>,
    scopes: Vec<Env>,
    strings: Vec<LStr>,
    object_index: FastMap<usize, u32>,
    scope_index: FastMap<usize, u32>,
    string_index: FastMap<usize, u32>,
    roots: Vec<u32>,
    names: Vec<String>,
    name_index: FastMap<String, u32>,
}

impl Graph {
    fn first_scope(&self) -> u32 {
        FIRST_OBJECT + self.objects.len() as u32
    }

    fn first_string(&self) -> u32 {
        self.first_scope() + self.scopes.len() as u32
    }

    fn node_count(&self) -> u32 {
        self.first_string() + self.strings.len() as u32
    }

    /// The node a target is, adding a string node the first time a string is seen.
    fn resolve(&mut self, target: Target) -> Option<u32> {
        match target {
            Target::Obj(p) => self.object_index.get(&(p as usize)).copied(),
            Target::Scope(p) => self.scope_index.get(&(p as usize)).copied(),
            Target::Node(n) => Some(n),
            Target::Str(s) => {
                let key = s.as_ptr() as usize;
                if let Some(&n) = self.string_index.get(&key) {
                    return Some(n);
                }
                let n = self.node_count();
                self.string_index.insert(key, n);
                self.strings.push(s);
                Some(n)
            }
        }
    }

    fn name_id(&mut self, name: &str) -> u32 {
        if let Some(&id) = self.name_index.get(name) {
            return id;
        }
        let id = self.names.len() as u32;
        self.names.push(name.to_string());
        self.name_index.insert(name.to_string(), id);
        id
    }
}

fn value_target(v: &Value) -> Option<Target> {
    match v {
        Value::Obj(o) => Some(Target::Obj(Gc::as_ptr(o))),
        Value::Str(s) => Some(Target::Str(s.clone())),
        _ => None,
    }
}

fn is_index_key(k: &str) -> Option<u32> {
    if k.is_empty() || k.len() > 10 || (k.len() > 1 && k.starts_with('0')) {
        return None;
    }
    k.parse::<u32>().ok().filter(|&n| n != u32::MAX)
}

/// The name a property key is shown under: a symbol's description, a private name without its
/// brand suffix; `None` for the engine's internal slots (shown as internal edges).
fn display_key(it: &Interp, key: &str) -> Result<String, String> {
    if Interp::is_private_key(key) {
        let end = key.find(['\u{0}', '\u{1}']).unwrap_or(key.len());
        return Ok(key[..end].to_string());
    }
    if Interp::is_sym_key(key) {
        if let Some(Value::Sym(s)) = it.sym_from_key(key) {
            return Ok(match &s.description {
                Some(d) => format!("<symbol {d}>"),
                None => "<symbol>".to_string(),
            });
        }
        return Err(key.trim_start_matches('\u{0}').to_string());
    }
    if key.starts_with('#') && key.contains('\u{0}') {
        return Err(key
            .trim_start_matches('#')
            .trim_end_matches('\u{0}')
            .replace('\u{0}', ""));
    }
    Ok(key.to_string())
}

/// A function's `name`: its own data property when it is a string, else the parsed name.
fn function_name(o: &Object) -> String {
    if let Some(p) = o.props.get("name") {
        if !p.accessor() {
            if let Value::Str(s) = p.value() {
                return s.as_str().to_string();
            }
        }
    }
    match &o.call {
        Callable::User(u) => u.func.name.clone().unwrap_or_default(),
        _ => String::new(),
    }
}

/// V8's class name for an object: the name of the constructor its prototype chain names.
fn class_name(o: &Object) -> String {
    if o.exotic.is_array() {
        return "Array".to_string();
    }
    let mut proto = o.proto.clone();
    let mut depth = 0;
    while let Some(p) = proto {
        if depth > 32 {
            break;
        }
        depth += 1;
        let b = p.borrow();
        if let Some(c) = b.props.get("constructor") {
            if !c.accessor() {
                if let Value::Obj(ctor) = c.value() {
                    if ctor.borrow().call.is_fn() {
                        let name = function_name(&ctor.borrow());
                        if !name.is_empty() {
                            return name;
                        }
                    }
                }
            }
        }
        proto = b.proto.clone();
    }
    "Object".to_string()
}

impl Interp {
    /// Every edge of node `n` (see the module docs for the shapes), in a stable order.
    fn snapshot_edges(&self, g: &Graph, n: u32, out: &mut Vec<Edge>) {
        out.clear();
        if n == ROOT {
            out.push(Edge {
                kind: E_ELEMENT,
                name: Name::Index(1),
                target: Target::Node(GC_ROOTS),
                owned: false,
            });
            out.push(Edge {
                kind: E_SHORTCUT,
                name: Name::Static("global"),
                target: Target::Obj(Gc::as_ptr(&self.global)),
                owned: false,
            });
            return;
        }
        if n == GC_ROOTS {
            for (i, &r) in g.roots.iter().enumerate() {
                out.push(Edge {
                    kind: E_ELEMENT,
                    name: Name::Index(i as u32 + 1),
                    target: Target::Node(r),
                    owned: false,
                });
            }
            return;
        }
        if n < g.first_scope() {
            let o = &g.objects[(n - FIRST_OBJECT) as usize];
            self.snapshot_object_edges(o, out);
        } else if n < g.first_string() {
            let e = &g.scopes[(n - g.first_scope()) as usize];
            let s = e.borrow();
            for (name, b) in s.vars.iter() {
                if let Some(t) = value_target(&b.value) {
                    let owned = matches!(b.value, Value::Obj(_));
                    out.push(Edge {
                        kind: E_CONTEXT,
                        name: Name::Key(name.clone()),
                        target: t,
                        owned,
                    });
                }
            }
            if let Some(Value::Obj(o)) = s.with_obj() {
                out.push(Edge {
                    kind: E_INTERNAL,
                    name: Name::Static("with"),
                    target: Target::Obj(Gc::as_ptr(o)),
                    owned: true,
                });
            }
            if let Some(p) = &s.parent {
                out.push(Edge {
                    kind: E_INTERNAL,
                    name: Name::Static("previous"),
                    target: Target::Scope(Rc::as_ptr(p)),
                    owned: true,
                });
            }
            for imp in s.import_envs() {
                out.push(Edge {
                    kind: E_INTERNAL,
                    name: Name::Static("import"),
                    target: Target::Scope(Rc::as_ptr(imp)),
                    owned: true,
                });
            }
        }
    }

    fn snapshot_object_edges(&self, o: &Gc, out: &mut Vec<Edge>) {
        let ptr = Gc::as_ptr(o) as usize;
        let b = o.borrow();
        let array = b.exotic.is_array();
        b.props.visit_snapshot_slots(&mut |key, p| {
            let (kind, name) = match key {
                Ok(i) => (E_ELEMENT, Name::Index(i)),
                Err(k) => match (array, is_index_key(&k)) {
                    (true, Some(i)) => (E_ELEMENT, Name::Index(i)),
                    _ => match display_key(self, &k) {
                        Ok(shown) if shown.as_str() == &*k => (E_PROPERTY, Name::Key(k)),
                        Ok(shown) => (E_PROPERTY, Name::Owned(shown)),
                        Err(internal) => (E_INTERNAL, Name::Owned(internal)),
                    },
                },
            };
            if p.accessor() {
                let label = match &name {
                    Name::Key(k) => k.to_string(),
                    Name::Owned(s) => s.clone(),
                    Name::Index(i) => i.to_string(),
                    Name::Static(s) => s.to_string(),
                };
                for (prefix, f) in [("get ", p.getter()), ("set ", p.setter())] {
                    if let Some(Value::Obj(f)) = f {
                        out.push(Edge {
                            kind: E_PROPERTY,
                            name: Name::Owned(format!("{prefix}{label}")),
                            target: Target::Obj(Gc::as_ptr(f)),
                            owned: true,
                        });
                    }
                }
            } else {
                let v = p.value();
                if let Some(t) = value_target(&v) {
                    let owned = matches!(v, Value::Obj(_));
                    out.push(Edge {
                        kind,
                        name,
                        target: t,
                        owned,
                    });
                }
            }
        });
        if let Some(proto) = &b.proto {
            out.push(Edge {
                kind: E_PROPERTY,
                name: Name::Static("__proto__"),
                target: Target::Obj(Gc::as_ptr(proto)),
                owned: true,
            });
        }
        let obj = |g: &Gc| Target::Obj(Gc::as_ptr(g));
        match &b.call {
            #[cfg(feature = "aot-native")]
            Callable::Aot(native) => {
                out.push(Edge {
                    kind: E_INTERNAL,
                    name: Name::Static("context"),
                    target: Target::Scope(Rc::as_ptr(&native.env)),
                    owned: true,
                });
            }
            Callable::User(u) => {
                out.push(Edge {
                    kind: E_INTERNAL,
                    name: Name::Static("context"),
                    target: Target::Scope(Rc::as_ptr(&u.env)),
                    owned: true,
                });
            }
            Callable::Bound(bound) => {
                out.push(Edge {
                    kind: E_INTERNAL,
                    name: Name::Static("bound_function"),
                    target: obj(&bound.target),
                    owned: true,
                });
                if let Some(t) = value_target(&bound.this) {
                    let owned = matches!(bound.this, Value::Obj(_));
                    out.push(Edge {
                        kind: E_INTERNAL,
                        name: Name::Static("bound_this"),
                        target: t,
                        owned,
                    });
                }
                for (i, a) in bound.args.iter().enumerate() {
                    if let Some(t) = value_target(a) {
                        let owned = matches!(a, Value::Obj(_));
                        out.push(Edge {
                            kind: E_INTERNAL,
                            name: Name::Owned(format!("bound_argument_{i}")),
                            target: t,
                            owned,
                        });
                    }
                }
            }
            Callable::Promise(slot) => {
                slot.visit_object_refs(&mut |g| {
                    out.push(Edge {
                        kind: E_INTERNAL,
                        name: Name::Static("promise_reaction"),
                        target: obj(g),
                        owned: true,
                    });
                });
            }
            Callable::Resolver(cell, resolve) => {
                if let Value::Obj(p) = &cell.promise {
                    // The pair shares one counted reference (see `gc_edges::visit_object_head`).
                    out.push(Edge {
                        kind: E_INTERNAL,
                        name: Name::Static("promise"),
                        target: obj(p),
                        owned: *resolve,
                    });
                }
            }
            _ => {}
        }
        if let Some(data) = self.map_data.get(&ptr) {
            use crate::builtins::collection_data::CollectionKind as K;
            let weak = matches!(data.kind(), K::WeakMap | K::WeakSet);
            let values = data.has_values();
            for (k, v) in data.iter() {
                let (k, v) = (k.unpack(), v.unpack());
                if let Some(t) = value_target(&k) {
                    let owned = matches!(k, Value::Obj(_));
                    out.push(Edge {
                        kind: if weak { E_WEAK } else { E_INTERNAL },
                        name: Name::Static("key"),
                        target: t,
                        owned,
                    });
                }
                if values {
                    if let Some(t) = value_target(&v) {
                        let owned = matches!(v, Value::Obj(_));
                        out.push(Edge {
                            kind: E_INTERNAL,
                            name: Name::Static("value"),
                            target: t,
                            owned,
                        });
                    }
                }
            }
        }
        if let Some((t, h)) = self.proxies.get(&ptr) {
            for (label, v) in [("target", t), ("handler", h)] {
                if let Value::Obj(g) = v {
                    out.push(Edge {
                        kind: E_INTERNAL,
                        name: Name::Static(label),
                        target: obj(g),
                        owned: true,
                    });
                }
            }
        }
        if let Some((env, _)) = self.mapped_arguments.get(&ptr) {
            out.push(Edge {
                kind: E_INTERNAL,
                name: Name::Static("context"),
                target: Target::Scope(Rc::as_ptr(env)),
                owned: true,
            });
        }
        if let Some(ci) = self.class_info.get(&ptr) {
            out.push(Edge {
                kind: E_INTERNAL,
                name: Name::Static("field_context"),
                target: Target::Scope(Rc::as_ptr(&ci.field_env)),
                owned: true,
            });
        }
        #[cfg(feature = "aot-native")]
        if let Some(class) = self.native_classes.get(&ptr) {
            out.push(Edge { kind: E_INTERNAL, name: Name::Static("field_context"), target: Target::Scope(Rc::as_ptr(&class.env)), owned: true });
            class.visit_values(|value| {
                if let Some(target) = value_target(value) { out.push(Edge { kind: E_INTERNAL, name: Name::Static("class_initializer"), target, owned: matches!(value, Value::Obj(_)) }); }
            });
        }
    }

    fn snapshot_node_name(&self, g: &Graph, n: u32) -> (u8, String, usize) {
        match n {
            ROOT => (T_SYNTHETIC, String::new(), 0),
            GC_ROOTS => (T_SYNTHETIC, "(GC roots)".to_string(), 0),
            _ if n < g.first_scope() => {
                let o = g.objects[(n - FIRST_OBJECT) as usize].borrow();
                let size = std::mem::size_of::<Object>()
                    + 16
                    + o.props.census().entries_cap * crate::value::memwalk::property_size();
                if o.call.is_fn() {
                    (T_CLOSURE, function_name(&o), size)
                } else {
                    (T_OBJECT, class_name(&o), size)
                }
            }
            _ if n < g.first_string() => {
                let s = g.scopes[(n - g.first_scope()) as usize].borrow();
                (
                    T_OBJECT,
                    "system / Context".to_string(),
                    64 + s.vars.census().1 * 48,
                )
            }
            _ => {
                let s = &g.strings[(n - g.first_string()) as usize];
                let text = s.as_str();
                let mut end = text.len().min(MAX_STRING_NAME);
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                let kind = if s.is_view() { T_SLICED } else { T_STRING };
                (kind, text[..end].to_string(), 16 + text.len())
            }
        }
    }

    /// Write a V8 heap snapshot of this thread's heap to `out` (after a full collection, as V8
    /// takes one).
    pub fn write_heap_snapshot(&mut self, out: &mut dyn Write) -> io::Result<()> {
        self.gc_collect();
        let objects = value::gc_snapshot();
        let scopes = value::scope_snapshot();
        let mut g = Graph {
            object_index: FastMap::default(),
            scope_index: FastMap::default(),
            string_index: FastMap::default(),
            strings: Vec::new(),
            roots: Vec::new(),
            names: Vec::new(),
            name_index: FastMap::default(),
            objects,
            scopes,
        };
        g.object_index.reserve(g.objects.len());
        for (i, o) in g.objects.iter().enumerate() {
            g.object_index
                .insert(Gc::as_ptr(o) as usize, FIRST_OBJECT + i as u32);
        }
        let first_scope = g.first_scope();
        g.scope_index.reserve(g.scopes.len());
        for (i, e) in g.scopes.iter().enumerate() {
            g.scope_index
                .insert(Rc::as_ptr(e) as usize, first_scope + i as u32);
        }
        g.name_id("");

        // Pass 1: edge counts, string nodes, and the owned references each object and scope
        // receives (an excess of strong references over those makes a root).
        let mut edge_counts: Vec<u32> = Vec::new();
        let mut incoming: Vec<u32> = vec![0; g.first_string() as usize];
        let mut edges = Vec::new();
        let mut n = 0;
        while n < g.node_count() {
            self.snapshot_edges(&g, n, &mut edges);
            let mut count = 0;
            for e in edges.drain(..) {
                let owned = e.owned;
                if let Some(to) = g.resolve(e.target) {
                    count += 1;
                    if owned && (to as usize) < incoming.len() && to >= FIRST_OBJECT {
                        incoming[to as usize] += 1;
                    }
                }
            }
            edge_counts.push(count);
            n += 1;
        }
        // A pin is bookkeeping, not a holder (as the collector counts it).
        for pin in self.gc_pins.values() {
            if let Some(&i) = g.object_index.get(&(Gc::as_ptr(pin) as usize)) {
                incoming[i as usize] += 1;
            }
        }
        // Each handle in `objects` / `scopes` is one strong reference of the snapshot's own.
        for (i, o) in g.objects.iter().enumerate() {
            if Gc::strong_count(o).saturating_sub(1) > incoming[FIRST_OBJECT as usize + i] as usize
            {
                g.roots.push(FIRST_OBJECT + i as u32);
            }
        }
        for (i, e) in g.scopes.iter().enumerate() {
            if Rc::strong_count(e).saturating_sub(1) > incoming[(first_scope as usize) + i] as usize
            {
                g.roots.push(first_scope + i as u32);
            }
        }
        drop(incoming);
        #[cfg(feature = "aot-native")]
        for unit in self.native_units.values().flatten() {
            if let Some(&node) = g.scope_index.get(&(Rc::as_ptr(&unit.env) as usize)) {
                if !g.roots.contains(&node) { g.roots.push(node); }
            }
            for value in std::iter::once(&unit.namespace).chain(unit.evaluation.iter()) {
              if let Value::Obj(namespace) = value {
                if let Some(&node) = g.object_index.get(&(Gc::as_ptr(namespace) as usize)) {
                    if !g.roots.contains(&node) { g.roots.push(node); }
                }
              }
            }
        }
        edge_counts[GC_ROOTS as usize] = g.roots.len() as u32;
        let node_count = g.node_count();
        let edge_total: u64 = edge_counts.iter().map(|&c| c as u64).sum();

        write!(
            out,
            "{{\"snapshot\":{{\"meta\":{{\"node_fields\":[\"type\",\"name\",\"id\",\"self_size\",\"edge_count\",\"trace_node_id\",\"detachedness\"],\
\"node_types\":[{},\"string\",\"number\",\"number\",\"number\",\"number\",\"number\"],\
\"edge_fields\":[\"type\",\"name_or_index\",\"to_node\"],\
\"edge_types\":[{},\"string_or_number\",\"node\"],\
\"trace_function_info_fields\":[\"function_id\",\"name\",\"script_name\",\"script_id\",\"line\",\"column\"],\
\"trace_node_fields\":[\"id\",\"function_info_index\",\"count\",\"size\",\"children\"],\
\"sample_fields\":[\"timestamp_us\",\"last_assigned_id\"],\
\"location_fields\":[\"object_index\",\"script_id\",\"line\",\"column\"]}},\
\"node_count\":{node_count},\"edge_count\":{edge_total},\"trace_function_count\":0}},\n\"nodes\":[",
            json_list(NODE_TYPES),
            json_list(EDGE_TYPES),
        )?;

        // Pass 2: nodes.
        for n in 0..node_count {
            let (kind, name, size) = self.snapshot_node_name(&g, n);
            let name = g.name_id(&name);
            let id = match n {
                ROOT => 1,
                GC_ROOTS => 3,
                _ => 2 * n as u64 + 3,
            };
            let sep = if n == 0 { "" } else { "," };
            writeln!(
                out,
                "{sep}{kind},{name},{id},{size},{},0,0",
                edge_counts[n as usize]
            )?;
        }
        out.write_all(b"],\n\"edges\":[")?;

        // Pass 3: edges, in node order.
        let mut first = true;
        for n in 0..node_count {
            self.snapshot_edges(&g, n, &mut edges);
            for e in edges.drain(..) {
                let Some(to) = g.resolve(e.target) else {
                    continue;
                };
                let name = match e.name {
                    Name::Index(i) => i,
                    Name::Static(s) => g.name_id(s),
                    Name::Key(k) => g.name_id(&k),
                    Name::Owned(s) => g.name_id(&s),
                };
                let sep = if first { "" } else { "," };
                first = false;
                writeln!(out, "{sep}{},{name},{}", e.kind, to * NODE_FIELDS)?;
            }
        }
        out.write_all(
            b"],\n\"trace_function_infos\":[],\n\"trace_tree\":[],\n\"samples\":[],\n\"locations\":[],\n\"strings\":[",
        )?;
        for (i, s) in g.names.iter().enumerate() {
            if i > 0 {
                out.write_all(b",\n")?;
            }
            write_json_string(out, s)?;
        }
        out.write_all(b"]}\n")?;
        out.flush()
    }
}

fn json_list(items: &[&str]) -> String {
    let quoted: Vec<String> = items.iter().map(|s| format!("\"{s}\"")).collect();
    format!("[{}]", quoted.join(","))
}

fn write_json_string(out: &mut dyn Write, s: &str) -> io::Result<()> {
    out.write_all(b"\"")?;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        let esc: Option<&str> = match c {
            '"' => Some("\\\""),
            '\\' => Some("\\\\"),
            '\n' => Some("\\n"),
            '\r' => Some("\\r"),
            '\t' => Some("\\t"),
            c if (c as u32) < 0x20 || c == '\u{2028}' || c == '\u{2029}' => None,
            _ => continue,
        };
        out.write_all(s[start..i].as_bytes())?;
        match esc {
            Some(e) => out.write_all(e.as_bytes())?,
            None => write!(out, "\\u{:04x}", c as u32)?,
        }
        start = i + c.len_utf8();
    }
    out.write_all(s[start..].as_bytes())?;
    out.write_all(b"\"")
}
