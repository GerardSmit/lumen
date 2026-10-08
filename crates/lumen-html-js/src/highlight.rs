//! Custom Highlight API. Strong traced entries keep live ranges and registered
//! highlights alive; rendering snapshots read the shared boundary data afresh.
use super::*;
use lumen::embed::{ArgCx, JsHost, NativeIdentityOwner};
use lumen_bind::{FromArg, Host, Slot, This, CtorRet};

#[derive(Clone)]
struct RangeEntry { value: Value, backing: range::AbstractRangeBacking }
impl<'a> FromArg<'a, JsHost> for RangeEntry {
    fn from_arg(cx: &'a ArgCx<'_>, value: &'a Value, at: Slot) -> Result<Self, Value> {
        let range = JsHost::class_ref::<range::DomAbstractRange>(cx, value, at)?;
        Ok(Self { value: value.clone(), backing: range.backing.clone() })
    }
}

#[derive(Default)]
struct HighlightState { ranges: RefCell<Vec<Option<RangeEntry>>>, priority: Cell<i32>, kind: RefCell<String>, iterators: Cell<usize> }
#[lumen_bind::class(name = "Highlight", hint(js(webidl)))]
pub(crate) struct DomHighlight { state: Rc<HighlightState> }
impl NativeIdentityOwner for DomHighlight {
    const TRACES_NATIVE_VALUES: bool = true;
    fn trace_native_identities(&self, _: u64, _: &mut dyn FnMut(&Value)) {}
    fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) {
        for range in self.state.ranges.borrow().iter().flatten() { visit(&range.value); }
    }
}
struct HighlightConstructor(DomHighlight);
impl CtorRet<JsHost, DomHighlight> for HighlightConstructor {
    fn into_ctor(self, cx: &<JsHost as Host>::Cx<'_>) -> Result<Value, Value> {
        let value = JsHost::construct(cx, self.0)?;
        JsHost::with_ctx(cx, |ctx| ctx.set_native_identity_owner::<DomHighlight>(&value)).map_err(|e| JsHost::with_ctx(cx, |ctx| e.to_value(ctx)))?;
        Ok(value)
    }
}
impl DomHighlight {
    fn insert(&self, entry: RangeEntry) {
        let mut ranges = self.state.ranges.borrow_mut();
        if !ranges.iter().flatten().any(|old| old.backing.same_range(&entry.backing)) { ranges.push(Some(entry)); }
    }
}
#[lumen_bind::methods]
impl DomHighlight {
    #[constructor]
    fn new(#[varargs] ranges: Vec<RangeEntry>) -> HighlightConstructor {
        let highlight = Self { state: Rc::new(HighlightState { kind: RefCell::new("highlight".into()), ..Default::default() }) };
        for range in ranges { highlight.insert(range); }
        HighlightConstructor(highlight)
    }
    #[getter] fn size(&self) -> usize { self.state.ranges.borrow().iter().flatten().count() }
    #[getter] fn priority(&self) -> i32 { self.state.priority.get() }
    #[setter(coerce)] fn set_priority(&self, priority: i32) { self.state.priority.set(priority); }
    #[getter] fn r#type(&self) -> String { self.state.kind.borrow().clone() }
    #[setter] fn set_type(&self, kind: String) -> OpResult<()> {
        if !matches!(kind.as_str(), "highlight" | "spelling-error" | "grammar-error") { return Err(OpError::type_error("invalid HighlightType")); }
        *self.state.kind.borrow_mut() = kind; Ok(())
    }
    fn add(&self, this: This<Value>, range: RangeEntry) -> Value { self.insert(range); this.0 }
    fn has(&self, range: RangeEntry) -> bool { self.state.ranges.borrow().iter().flatten().any(|old| old.backing.same_range(&range.backing)) }
    fn delete(&self, range: RangeEntry) -> bool {
        let mut ranges = self.state.ranges.borrow_mut();
        let Some(entry) = ranges.iter_mut().find(|old| old.as_ref().is_some_and(|old| old.backing.same_range(&range.backing))) else { return false; };
        *entry = None;
        if self.state.iterators.get() == 0 { ranges.retain(Option::is_some); }
        true
    }
    fn clear(&self) {
        let mut entries = self.state.ranges.borrow_mut();
        if self.state.iterators.get() == 0 { entries.clear(); } else { for entry in entries.iter_mut() { *entry = None; } }
    }
    #[method(hint(js(also_iterator)))]
    fn values(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> { make_iterator(ctx, this.0, Collection::Highlight(self.state.clone()), IteratorKind::Values) }
    fn keys(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> { self.values(ctx, this) }
    fn entries(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> { make_iterator(ctx, this.0, Collection::Highlight(self.state.clone()), IteratorKind::Entries) }
    fn for_each(&self, ctx: &mut Ctx, this: This<Value>, callback: lumen::embed::JsFunction, #[default(Value::Undefined)] this_arg: Value) -> OpResult<()> {
        for_each(ctx, this.0, Collection::Highlight(self.state.clone()), callback, this_arg)
    }
}

struct RegistryEntry { name: String, value: Value, state: Rc<HighlightState> }
pub(crate) struct RegistryState { realm: std::rc::Weak<DomRealm>, entries: RefCell<Vec<Option<RegistryEntry>>>, iterators: Cell<usize> }
#[lumen_bind::class(name = "HighlightRegistry", hint(js(webidl)))]
pub(crate) struct DomHighlightRegistry { state: Rc<RegistryState> }
const REGISTRY_SLOT: &str = "#lumen_css_highlights\u{1}registry";

#[lumen_bind::op(name = "get highlights")]
fn highlights_getter(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
    ctx.native_private_value_slot(&this.0, REGISTRY_SLOT)
        .ok_or_else(|| OpError::type_error("CSS.highlights receiver is not its namespace"))
}
impl NativeIdentityOwner for DomHighlightRegistry {
    const TRACES_NATIVE_VALUES: bool = true;
    fn trace_native_identities(&self, _: u64, _: &mut dyn FnMut(&Value)) {}
    fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) { for entry in self.state.entries.borrow().iter().flatten() { visit(&entry.value); } }
}
struct HighlightEntry { value: Value, state: Rc<HighlightState> }
impl<'a> FromArg<'a, JsHost> for HighlightEntry {
    fn from_arg(cx: &'a ArgCx<'_>, value: &'a Value, at: Slot) -> Result<Self, Value> {
        let highlight = JsHost::class_ref::<DomHighlight>(cx, value, at)?;
        Ok(Self { value: value.clone(), state: highlight.state.clone() })
    }
}
#[lumen_bind::methods]
impl DomHighlightRegistry {
    #[getter] fn size(&self) -> usize { self.state.entries.borrow().iter().flatten().count() }
    #[method(coerce)] fn set(&self, this: This<Value>, name: String, highlight: HighlightEntry) -> Value {
        let mut entries = self.state.entries.borrow_mut();
        let next = RegistryEntry { name: name.clone(), value: highlight.value, state: highlight.state };
        if let Some(old) = entries.iter_mut().find(|entry| entry.as_ref().is_some_and(|entry| entry.name == name)) { *old = Some(next); }
        else { entries.push(Some(next)); }
        this.0
    }
    #[method(coerce)] fn get(&self, name: String) -> Value { self.state.entries.borrow().iter().flatten().find(|entry| entry.name == name).map_or(Value::Undefined, |entry| entry.value.clone()) }
    #[method(coerce)] fn has(&self, name: String) -> bool { self.state.entries.borrow().iter().flatten().any(|entry| entry.name == name) }
    #[method(coerce)] fn delete(&self, name: String) -> bool {
        let mut entries = self.state.entries.borrow_mut();
        let Some(entry) = entries.iter_mut().find(|entry| entry.as_ref().is_some_and(|entry| entry.name == name)) else { return false; };
        *entry = None;
        if self.state.iterators.get() == 0 { entries.retain(Option::is_some); }
        true
    }
    fn clear(&self) {
        let mut entries = self.state.entries.borrow_mut();
        if self.state.iterators.get() == 0 { entries.clear(); } else { for entry in entries.iter_mut() { *entry = None; } }
    }
    #[method(hint(js(also_iterator)))]
    fn entries(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> { make_iterator(ctx, this.0, Collection::Registry(self.state.clone()), IteratorKind::Entries) }
    fn keys(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> { make_iterator(ctx, this.0, Collection::Registry(self.state.clone()), IteratorKind::Keys) }
    fn values(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> { make_iterator(ctx, this.0, Collection::Registry(self.state.clone()), IteratorKind::Values) }
    fn for_each(&self, ctx: &mut Ctx, this: This<Value>, callback: lumen::embed::JsFunction, #[default(Value::Undefined)] this_arg: Value) -> OpResult<()> {
        for_each(ctx, this.0, Collection::Registry(self.state.clone()), callback, this_arg)
    }
}

pub(crate) fn install(ctx: &mut Ctx, realm: &Rc<DomRealm>) -> OpResult<()> {
    let global = ctx.global_object();
    let constructor = ctx.class_constructor::<DomHighlight>();
    crate::install_interface(ctx, &global, "Highlight", constructor)?;
    let constructor = ctx.class_constructor::<DomHighlightRegistry>();
    crate::install_interface(ctx, &global, "HighlightRegistry", constructor)?;
    let state = Rc::new(RegistryState { realm: Rc::downgrade(realm), entries: RefCell::new(Vec::new()), iterators: Cell::new(0) });
    *realm.highlights.borrow_mut() = Rc::downgrade(&state);
    let registry = ctx.new_instance(DomHighlightRegistry { state });
    ctx.set_native_identity_owner::<DomHighlightRegistry>(&registry)?;
    // The document registry outlives script replacement of the CSS namespace.
    // Root it in the realm's traced global rather than creating an Rc cycle
    // through RangeData's document ownership.
    // Initial about:blank navigation can keep the same Window. Its traced
    // document registry slot must then replace the retired document's value.
    ctx.set_native_internal_value_slot(&global, REGISTRY_SLOT, registry.clone()).map_err(OpError::thrown)?;
    let css = ctx.member_get(&global, "CSS").map_err(OpError::thrown)?;
    ctx.define_native_internal_value_slot(&css, REGISTRY_SLOT, registry).map_err(OpError::thrown)?;
    let getter = ctx.bound_function(&lumen_bind::FnItem::of::<highlights_getter::Op>());
    let descriptor = ctx.new_object_with_proto(&Value::Null);
    for (name, value) in [("get", getter), ("enumerable", Value::Bool(true)), ("configurable", Value::Bool(true))] {
        ctx.set_member(&descriptor, name, value).map_err(lumen::embed::abrupt_value).map_err(OpError::thrown)?;
    }
    ctx.define_property_value(&css, Value::str("highlights"), &descriptor).map_err(OpError::thrown)?;
    Ok(())
}

pub(crate) fn synchronize(realm: &DomRealm) {
    let Some(registry) = realm.highlights.borrow().upgrade() else { return; };
    let Some(owner) = registry.realm.upgrade() else { return; };
    let mut highlights = Vec::new();
    for (order, entry) in registry.entries.borrow().iter().enumerate() {
        let Some(entry) = entry else { continue; };
        let ranges = entry.state.ranges.borrow().iter().flatten().filter_map(|range| range.backing.highlight_points(&owner))
            .map(|(start, end)| lumen_html::highlights::HighlightRange { start, end }).collect();
        highlights.push(lumen_html::highlights::Highlight { name: entry.name.clone(), priority: entry.state.priority.get(), order, ranges });
    }
    realm.session.borrow_mut().set_highlights(highlights);
}

#[derive(Clone)]
enum Collection { Highlight(Rc<HighlightState>), Registry(Rc<RegistryState>) }
impl Collection {
    fn iterators(&self) -> &Cell<usize> { match self { Self::Highlight(state) => &state.iterators, Self::Registry(state) => &state.iterators } }
    fn compact(&self) { match self { Self::Highlight(state) => state.ranges.borrow_mut().retain(Option::is_some), Self::Registry(state) => state.entries.borrow_mut().retain(Option::is_some) } }
    fn next(&self, cursor: &mut usize) -> Option<(Value, Value)> {
        match self {
            Self::Highlight(state) => { let entries = state.ranges.borrow(); while *cursor < entries.len() {
                let entry = entries[*cursor].as_ref(); *cursor += 1;
                if let Some(entry) = entry { return Some((entry.value.clone(), entry.value.clone())); }
            } }
            Self::Registry(state) => { let entries = state.entries.borrow(); while *cursor < entries.len() {
                let entry = entries[*cursor].as_ref(); *cursor += 1;
                if let Some(entry) = entry { return Some((Value::Str(entry.name.clone().into()), entry.value.clone())); }
            } }
        }
        None
    }
}
#[derive(Clone, Copy)]
enum IteratorKind { Keys, Values, Entries }
#[lumen_bind::class(name = "HighlightCollectionIterator")]
struct HighlightIterator { owner: Value, collection: Collection, cursor: Cell<usize>, done: Cell<bool>, kind: IteratorKind }
impl Drop for HighlightIterator {
    fn drop(&mut self) {
        if !self.done.get() { self.release(); }
    }
}
impl NativeIdentityOwner for HighlightIterator {
    const TRACES_NATIVE_VALUES: bool = true;
    fn trace_native_identities(&self, _: u64, _: &mut dyn FnMut(&Value)) {}
    fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) { visit(&self.owner); }
}
impl HighlightIterator {
    fn release(&self) {
        self.done.set(true);
        let count = self.collection.iterators(); count.set(count.get().saturating_sub(1));
        if count.get() == 0 { self.collection.compact(); }
    }
}
#[lumen_bind::methods]
impl HighlightIterator {
    #[proto(iter)] fn iter(&self, this: This<Value>) -> Value { this.0 }
    #[proto(next)] fn next(&self, ctx: &mut Ctx) -> OpResult<Option<Value>> {
        if self.done.get() { return Ok(None); }
        let mut cursor = self.cursor.get(); let pair = self.collection.next(&mut cursor); self.cursor.set(cursor);
        let Some((key, value)) = pair else { self.release(); return Ok(None); };
        Ok(Some(match self.kind { IteratorKind::Keys => key, IteratorKind::Values => value,
            IteratorKind::Entries => JsHost::from_list(ctx, vec![key, value]) }))
    }
}
fn make_iterator(ctx: &mut Ctx, owner: Value, collection: Collection, kind: IteratorKind) -> OpResult<Value> {
    collection.iterators().set(collection.iterators().get() + 1);
    let value = ctx.new_instance(HighlightIterator { owner, collection, cursor: Cell::new(0), done: Cell::new(false), kind });
    ctx.set_native_identity_owner::<HighlightIterator>(&value)?; Ok(value)
}
fn for_each(ctx: &mut Ctx, owner: Value, collection: Collection, callback: lumen::embed::JsFunction, this_arg: Value) -> OpResult<()> {
    collection.iterators().set(collection.iterators().get() + 1);
    let guard = HighlightIterator { owner: owner.clone(), collection, cursor: Cell::new(0), done: Cell::new(false), kind: IteratorKind::Entries };
    let mut cursor = 0;
    while let Some((key, value)) = guard.collection.next(&mut cursor) { callback.call(ctx, this_arg.clone(), &[value, key, owner.clone()])?; }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn evaluate(engine: &mut lumen::Engine, source: &str) {
        match engine.eval_value(&format!("(()=>{{ try {{ {source}; return true }} catch(e) {{ return String(e.stack || e) }} }})()")) {
            Ok(Ok(Value::Bool(true))) => (),
            Ok(Ok(Value::Str(message))) => panic!("highlight regression failed: {}", message.as_str()),
            _ => panic!("highlight regression did not return true"),
        }
    }

    #[test]
    fn custom_highlight_registry_is_typed_iterable_and_mutation_safe() {
        let mut engine = lumen::Engine::new();
        let realm = crate::install(engine.ctx(), "<p>alpha</p>", 128).unwrap();
        evaluate(&mut engine, r#"
            const check=(v,m)=>{if(!v)throw Error(m)}, a=new Range(), b=new Range(), c=new Range();
            const h=new Highlight(a,a,b);check(h.size===2,'range identity dedup');
            const originalRegistry=CSS.highlights;CSS.highlights=null;
            const descriptor=Object.getOwnPropertyDescriptor(CSS,'highlights');
            check(CSS.highlights===originalRegistry&&typeof descriptor.get==='function'&&descriptor.set===undefined,'readonly SameObject registry');
            const iterator=h.values();check(iterator.next().value===a,'ordered range iteration');
            h.delete(b);h.add(c);check(iterator.next().value===c&&iterator.next().done,'live delete/add');
            check(h.add(b)===h,'add chain');
            check(CSS.highlights.set('first',h)===CSS.highlights&&CSS.highlights.get('first')===h,'registry identity');
            const map=CSS.highlights.entries();check(map.next().value[0]==='first','registry entries');
            CSS.highlights.clear();CSS.highlights.set('last',new Highlight(c));check(map.next().value[0]==='last'&&map.next().done,'clear/reappend iteration');
            let typeError=false;try{new Highlight({})}catch(e){typeError=e instanceof TypeError}check(typeError,'typed abstract ranges');
            h.priority=4;h.type='grammar-error';check(h.priority===4&&h.type==='grammar-error','priority/type');
            const text=document.querySelector('p').firstChild;
            const staticRange=new StaticRange({startContainer:text,startOffset:0,endContainer:text,endOffset:2});
            check(new Highlight(staticRange).has(staticRange),'typed StaticRange member');
            let count=0;h.forEach((value,key,owner)=>{check(value===key&&owner===h,'set foreach');count++});check(count===3,'foreach count');
            CSS.highlights.clear();
        "#);
        realm.with_session(|session| assert!(session.highlights().is_empty()));
        let registry = realm.highlights.borrow().upgrade().unwrap();
        assert!(registry.entries.borrow().is_empty(), "exhausted iterators release tombstones");
    }

    #[test]
    fn custom_highlight_live_boundaries_and_registered_gc_ownership() {
        let mut engine = lumen::Engine::new();
        let realm = crate::install(engine.ctx(), "<p>abcdef</p>", 128).unwrap();
        evaluate(&mut engine, r#"
            globalThis.text=document.querySelector('p').firstChild;
            (()=>{const range=new Range();range.setStart(text,2);range.setEnd(text,5);CSS.highlights.set('live',new Highlight(range))})();
            text.insertData(0,'XX');
        "#);
        engine.ctx().collect_garbage();
        realm.with_session(|session| {
            let snapshots = &session.highlights();
            assert_eq!(snapshots.len(), 1);
            assert_eq!(snapshots[0].ranges[0].start.offset, 4);
            assert_eq!(snapshots[0].ranges[0].end.offset, 7);
        });
        evaluate(&mut engine, "CSS.highlights.get('live').values().next().value.setEnd(text,6)");
        realm.with_session(|session| assert_eq!(session.highlights()[0].ranges[0].end.offset, 6));
        evaluate(&mut engine, "CSS.highlights.delete('live')");
        realm.with_session(|session| assert!(session.highlights().is_empty()));
        evaluate(&mut engine, "const range=new Range();range.selectNodeContents(text);CSS.highlights.set('retained',new Highlight(range));globalThis.CSS={};text.appendData('x')");
        engine.ctx().collect_garbage();
        realm.with_session(|session| {
            assert_eq!(session.highlights().len(), 1);
            assert_eq!(session.highlights()[0].name, "retained");
        });
    }
}
