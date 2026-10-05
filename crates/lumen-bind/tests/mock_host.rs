//! A third, minimal host: proves the macros need nothing language-specific, and exercises the
//! generated binding code (binding, defaults, keywords, classes, modules, errors).

use lumen_bind::*;
use std::any::Any;
use std::cell::RefCell;
use std::rc::Rc;

type Entry = fn(&mut MockCtx, &V, &[V], &[(String, V)]) -> Result<V, String>;

#[derive(Clone)]
enum V {
    Nil,
    Num(f64),
    Int(i128),
    Bool(bool),
    Str(String),
    Bytes(Vec<u8>),
    List(Vec<V>),
    Obj(Rc<RefCell<Box<dyn Any>>>),
    Fn(Entry),
    Map(Vec<(String, V)>),
}

impl std::fmt::Debug for V {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("V")
    }
}

impl V {
    fn get(&self, k: &str) -> V {
        match self {
            V::Map(m) => m
                .iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.clone())
                .unwrap_or(V::Nil),
            _ => V::Nil,
        }
    }
    fn num(&self) -> f64 {
        match self {
            V::Num(x) => *x,
            V::Int(n) => *n as f64,
            _ => panic!("not a number"),
        }
    }
    fn s(&self) -> String {
        match self {
            V::Str(s) => s.clone(),
            _ => panic!("not a str"),
        }
    }
}

#[derive(Default)]
struct MockCtx {
    log: Vec<String>,
}

struct Cx<'s> {
    ctx: *mut MockCtx,
    this: &'s V,
    args: &'s [V],
    kw: &'s [(String, V)],
    desc: &'static FnDesc,
    guards: RefCell<Vec<Box<dyn Any>>>,
}

struct Mock;

fn call<N: Native<Mock>>(
    ctx: &mut MockCtx,
    this: &V,
    args: &[V],
    kw: &[(String, V)],
) -> Result<V, String> {
    let cx = Cx {
        ctx,
        this,
        args,
        kw,
        desc: N::DESC,
        guards: RefCell::new(Vec::new()),
    };
    N::call(&cx)
}

struct SyncV(V);
// SAFETY: `V::Nil` holds no `Rc`.
unsafe impl Sync for SyncV {}
static NIL: SyncV = SyncV(V::Nil);

fn instance<'c, T: Class>(cx: &'c Cx<'_>, v: &'c V, mutable: bool) -> Result<*mut T, String> {
    let V::Obj(rc) = v else {
        return Err(format!("{}: expected {}", cx.desc.name, T::DESC.name));
    };
    if !rc.borrow().is::<T>() {
        return Err(format!("{}: expected {}", cx.desc.name, T::DESC.name));
    }
    let conflict = || format!("{}: {} already borrowed", cx.desc.name, T::DESC.name);
    let rc = rc.clone();
    if mutable {
        let mut g = rc.try_borrow_mut().map_err(|_| conflict())?;
        let p: *mut T = g.downcast_mut::<T>().unwrap();
        let g: std::cell::RefMut<'static, Box<dyn Any>> = unsafe { std::mem::transmute(g) };
        cx.guards.borrow_mut().push(Box::new((Some(g), rc.clone())));
        Ok(p)
    } else {
        let g = rc.try_borrow().map_err(|_| conflict())?;
        let p = g.downcast_ref::<T>().unwrap() as *const T as *mut T;
        let g: std::cell::Ref<'static, Box<dyn Any>> = unsafe { std::mem::transmute(g) };
        cx.guards.borrow_mut().push(Box::new((Some(g), rc.clone())));
        Ok(p)
    }
}

impl Host for Mock {
    const NAME: &'static str = "mock";
    type Value = V;
    type Error = String;
    type Ctx = MockCtx;
    type Cx<'s> = Cx<'s>;
    type Entry = Entry;

    fn entry<N: Native<Self>>() -> Entry {
        call::<N>
    }

    fn bind<'c, const N: usize>(cx: &'c Cx<'_>) -> Result<[Option<&'c V>; N], String> {
        let d = cx.desc;
        let mut out = [None; N];
        if cx.args.len() > d.max_pos as usize && !d.has_varargs() {
            return Err(format!("{}: too many arguments", d.name));
        }
        for (i, a) in cx.args.iter().take(d.max_pos as usize).enumerate() {
            out[i] = Some(a);
        }
        for (k, v) in cx.kw {
            match d
                .named()
                .position(|p| p.name == k && p.kind != ParamKind::PosOnly)
            {
                Some(i) if out[i].is_some() => return Err(format!("{}: duplicate {k}", d.name)),
                Some(i) => out[i] = Some(v),
                None if d.has_varkw() => {}
                None => return Err(format!("{}: unexpected keyword {k}", d.name)),
            }
        }
        for (i, p) in d.named().enumerate() {
            if out[i].is_none() && !p.optional() {
                return Err(format!("{}: missing {}", d.name, p.name));
            }
        }
        Ok(out)
    }
    fn rest<'c>(cx: &'c Cx<'_>) -> &'c [V] {
        cx.args.get(cx.desc.max_pos as usize..).unwrap_or(&[])
    }
    fn varkw<'c>(cx: &'c Cx<'_>) -> Vec<(&'c str, &'c V)> {
        cx.kw
            .iter()
            .filter(|(k, _)| !cx.desc.named().any(|p| p.name == k))
            .map(|(k, v)| (k.as_str(), v))
            .collect()
    }
    fn this<'c>(cx: &'c Cx<'_>) -> &'c V {
        cx.this
    }
    fn absent() -> &'static V {
        &NIL.0
    }
    fn with_ctx<R>(cx: &Cx<'_>, f: impl FnOnce(&mut MockCtx) -> R) -> R {
        f(unsafe { &mut *cx.ctx })
    }
    fn ret<R: IntoRet<Self>>(cx: &Cx<'_>, r: R) -> Result<V, String> {
        r.into_ret(unsafe { &mut *cx.ctx })
    }
    fn ret_next<R: NextRet<Self>>(cx: &Cx<'_>, r: R) -> Result<V, String> {
        r.into_next(unsafe { &mut *cx.ctx })
    }
    fn construct<T: Class>(cx: &Cx<'_>, value: T) -> Result<V, String> {
        Self::new_instance(unsafe { &mut *cx.ctx }, value)
    }

    fn is_none(v: &V) -> bool {
        matches!(v, V::Nil)
    }
    fn to_f64(cx: &Cx<'_>, v: &V, at: Slot) -> Result<f64, String> {
        match v {
            V::Num(x) => Ok(*x),
            V::Int(n) => Ok(*n as f64),
            _ => Err(format!(
                "{}: argument {:?} must be a number",
                cx.desc.name,
                at.index()
            )),
        }
    }
    fn to_int(cx: &Cx<'_>, v: &V, _: Slot, kind: IntKind) -> Result<i128, String> {
        match v {
            V::Int(n) if (kind.min()..=kind.max()).contains(n) => Ok(*n),
            V::Int(_) => Err(format!("{}: out of range", cx.desc.name)),
            _ => Err(format!("{}: expected int", cx.desc.name)),
        }
    }
    fn to_bigint(cx: &Cx<'_>, _: &V, _: Slot) -> Result<BigInt, String> {
        Err(format!("{}: no bigints", cx.desc.name))
    }
    fn is_true(v: &V) -> bool {
        matches!(v, V::Bool(true))
    }
    fn to_bool(cx: &Cx<'_>, v: &V, _: Slot) -> Result<bool, String> {
        match v {
            V::Bool(b) => Ok(*b),
            _ => Err(format!("{}: expected bool", cx.desc.name)),
        }
    }
    fn to_str<'c>(cx: &'c Cx<'_>, v: &'c V, _: Slot) -> Result<&'c str, String> {
        match v {
            V::Str(s) => Ok(s),
            _ => Err(format!("{}: expected str", cx.desc.name)),
        }
    }
    fn to_bytes<'c>(cx: &'c Cx<'_>, v: &'c V, _: Slot) -> Result<&'c [u8], String> {
        match v {
            V::Bytes(b) => Ok(b),
            _ => Err(format!("{}: expected bytes", cx.desc.name)),
        }
    }
    fn to_bytes_mut<'c>(cx: &'c Cx<'_>, _: &'c V, _: Slot) -> Result<&'c mut [u8], String> {
        Err(format!("{}: no mutable buffers", cx.desc.name))
    }
    fn to_byte_vec(cx: &Cx<'_>, v: &V, at: Slot) -> Result<Vec<u8>, String> {
        Self::to_bytes(cx, v, at).map(<[u8]>::to_vec)
    }
    fn to_seq<'c>(cx: &'c Cx<'_>, v: &'c V, _: Slot) -> Result<&'c [V], String> {
        match v {
            V::List(l) => Ok(l),
            _ => Err(format!("{}: expected list", cx.desc.name)),
        }
    }
    fn class_ref<'c, T: Class>(cx: &'c Cx<'_>, v: &'c V, _: Slot) -> Result<&'c T, String> {
        instance::<T>(cx, v, false).map(|p| unsafe { &*p })
    }
    fn class_mut<'c, T: Class>(cx: &'c Cx<'_>, v: &'c V, _: Slot) -> Result<&'c mut T, String> {
        instance::<T>(cx, v, true).map(|p| unsafe { &mut *p })
    }

    fn unit(_: &mut MockCtx) -> V {
        V::Nil
    }
    fn none(_: &mut MockCtx) -> V {
        V::Nil
    }
    fn from_bool(_: &mut MockCtx, b: bool) -> V {
        V::Bool(b)
    }
    fn from_f64(_: &mut MockCtx, x: f64) -> V {
        V::Num(x)
    }
    fn from_int(_: &mut MockCtx, n: i128) -> Result<V, String> {
        Ok(V::Int(n))
    }
    fn from_bigint(_: &mut MockCtx, _: BigInt) -> V {
        V::Nil
    }
    fn from_str(_: &mut MockCtx, s: &str) -> V {
        V::Str(s.into())
    }
    fn from_string(_: &mut MockCtx, s: String) -> V {
        V::Str(s)
    }
    fn from_bytes(_: &mut MockCtx, b: Vec<u8>) -> V {
        V::Bytes(b)
    }
    fn from_list(_: &mut MockCtx, items: Vec<V>) -> V {
        V::List(items)
    }
    fn from_tuple(_: &mut MockCtx, items: Vec<V>) -> V {
        V::List(items)
    }
    fn new_instance<T: Class>(_: &mut MockCtx, value: T) -> Result<V, String> {
        Ok(V::Obj(Rc::new(RefCell::new(Box::new(value)))))
    }
    fn iter_step(_: &mut MockCtx, item: Option<V>) -> Result<V, String> {
        Ok(item.unwrap_or(V::Str("<done>".into())))
    }
    fn error(_: &mut MockCtx, e: NativeError) -> String {
        e.message.into_owned()
    }

    fn class_object<T: Methods<Self>>(_: &mut MockCtx) -> Result<V, String> {
        let mut ms = Vec::new();
        T::members(&mut ms);
        let entries = ms
            .into_iter()
            .filter(|m| m.desc.exposed_to("mock"))
            .map(|m| {
                (
                    m.desc.fixed_name("mock").unwrap_or(m.desc.name).to_string(),
                    V::Fn(m.entry),
                )
            })
            .collect();
        Ok(V::Map(entries))
    }
    fn module_object<M: Module<Self>>(ctx: &mut MockCtx) -> Result<V, String> {
        let items = ModuleItems::<Mock>::of::<M>();
        let mut out = Vec::new();
        for f in items.functions.iter().filter(|f| f.desc.exposed_to("mock")) {
            out.push((
                f.desc.fixed_name("mock").unwrap_or(f.desc.name).to_string(),
                V::Fn(f.entry),
            ));
        }
        for c in &items.classes {
            out.push((c.desc.name_for("mock").to_string(), (c.object)(ctx)?));
        }
        for k in &items.constants {
            out.push((k.name.to_string(), (k.value)(ctx)?));
        }
        let m = V::Map(out);
        if let Some(init) = items.init {
            init(ctx, &m)?;
        }
        Ok(m)
    }
}

fn invoke(ctx: &mut MockCtx, f: &V, this: &V, args: &[V], kw: &[(&str, V)]) -> Result<V, String> {
    let V::Fn(e) = f else { panic!("not a fn") };
    let kw: Vec<(String, V)> = kw.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
    e(ctx, this, args, &kw)
}

#[lumen_bind::module(name = "geo")]
mod geo {
    use super::*;

    /// Clamps.
    #[op]
    pub fn clamp(x: f64, lo: f64, hi: f64) -> f64 {
        x.max(lo).min(hi)
    }

    #[op]
    pub fn close(
        a: f64,
        b: f64,
        #[kwonly]
        #[default(1e-09)]
        rel_tol: f64,
    ) -> bool {
        (a - b).abs() <= rel_tol * a.abs().max(b.abs())
    }

    #[op(rename(mock = "total"))]
    pub fn sum(#[varargs] xs: Vec<f64>, #[varkw] kw: Vec<(String, i64)>) -> f64 {
        xs.iter().sum::<f64>() + kw.iter().map(|(_, v)| *v as f64).sum::<f64>()
    }

    #[op]
    pub fn greet(name: &str, #[kw] greeting: Option<&str>) -> String {
        format!("{}, {name}", greeting.unwrap_or("hello"))
    }

    #[op(only(js))]
    pub fn js_only() {}

    #[op]
    pub fn fail(n: u8) -> NativeResult<u8> {
        if n > 3 {
            Err(NativeError::value_error("too big"))
        } else {
            Ok(n * 2)
        }
    }

    #[op]
    pub fn logs(ctx: &mut MockCtx, msg: String) -> usize {
        ctx.log.push(msg);
        ctx.log.len()
    }

    #[op]
    pub fn blen(b: &[u8], extra: Vec<i32>) -> (usize, i32) {
        (b.len(), extra.iter().sum())
    }

    #[constant(name = "TAU")]
    const TAU: f64 = std::f64::consts::TAU;

    #[init]
    fn init(ctx: &mut MockCtx, _m: &V) {
        ctx.log.push("init".into());
    }

    #[class]
    pub struct Point {
        x: f64,
        y: f64,
    }

    #[methods]
    impl Point {
        #[constructor]
        fn new(x: f64, y: f64) -> Point {
            Point { x, y }
        }
        #[getter]
        fn x(&self) -> f64 {
            self.x
        }
        #[setter]
        fn set_x(&mut self, x: f64) {
            self.x = x;
        }
        fn scale(&mut self, k: f64) {
            self.x *= k;
            self.y *= k;
        }
        fn add_from(&mut self, other: &Point) {
            self.x += other.x;
            self.y += other.y;
        }
        #[proto(repr)]
        fn repr(&self) -> String {
            format!("Point({}, {})", self.x, self.y)
        }
        fn origin() -> Point {
            Point { x: 0.0, y: 0.0 }
        }
        #[skip]
        #[allow(dead_code)]
        fn hidden(&self) {}
    }
}

#[test]
fn functions() {
    let mut ctx = MockCtx::default();
    let m = <Mock as Host>::module_object::<geo::Module>(&mut ctx).unwrap();
    assert_eq!(ctx.log, ["init"]);
    let n = |x: f64| V::Num(x);
    assert_eq!(
        invoke(
            &mut ctx,
            &m.get("clamp"),
            &V::Nil,
            &[n(5.0), n(0.0), n(3.0)],
            &[]
        )
        .unwrap()
        .num(),
        3.0
    );
    assert!(geo::clamp::DESC.scalar.is_some());
    assert_eq!(geo::clamp::DESC.doc, Some("Clamps."));
    assert!(invoke(&mut ctx, &m.get("clamp"), &V::Nil, &[n(5.0)], &[])
        .unwrap_err()
        .contains("missing lo"));

    let close = m.get("close");
    assert!(matches!(
        invoke(&mut ctx, &close, &V::Nil, &[n(1.0), n(1.0 + 1e-12)], &[]).unwrap(),
        V::Bool(true)
    ));
    assert!(matches!(
        invoke(
            &mut ctx,
            &close,
            &V::Nil,
            &[n(1.0), n(1.1)],
            &[("rel_tol", n(0.5))]
        )
        .unwrap(),
        V::Bool(true)
    ));
    assert!(invoke(&mut ctx, &close, &V::Nil, &[n(1.0), n(1.1), n(0.5)], &[]).is_err());
    let p = &geo::close::DESC.params[2];
    assert_eq!((p.kind, p.default), (ParamKind::KwOnly, Some("1e-09")));

    assert!(matches!(m.get("sum"), V::Nil));
    let total = invoke(
        &mut ctx,
        &m.get("total"),
        &V::Nil,
        &[n(1.0), n(2.0)],
        &[("z", V::Int(4))],
    )
    .unwrap();
    assert_eq!(total.num(), 7.0);

    let greet = m.get("greet");
    assert_eq!(
        invoke(&mut ctx, &greet, &V::Nil, &[V::Str("bob".into())], &[])
            .unwrap()
            .s(),
        "hello, bob"
    );
    let hi = invoke(
        &mut ctx,
        &greet,
        &V::Nil,
        &[V::Str("bob".into())],
        &[("greeting", V::Str("hi".into()))],
    );
    assert_eq!(hi.unwrap().s(), "hi, bob");
    assert_eq!(geo::greet::DESC.min_pos, 1);
    assert_eq!(geo::greet::DESC.max_pos, 2);

    assert!(matches!(m.get("js_only"), V::Nil));
    assert!(matches!(
        invoke(&mut ctx, &m.get("fail"), &V::Nil, &[V::Int(2)], &[]).unwrap(),
        V::Int(4)
    ));
    assert_eq!(
        invoke(&mut ctx, &m.get("fail"), &V::Nil, &[V::Int(5)], &[]).unwrap_err(),
        "too big"
    );
    assert!(
        invoke(&mut ctx, &m.get("fail"), &V::Nil, &[V::Int(300)], &[])
            .unwrap_err()
            .contains("range")
    );
    assert!(matches!(
        invoke(
            &mut ctx,
            &m.get("logs"),
            &V::Nil,
            &[V::Str("a".into())],
            &[]
        )
        .unwrap(),
        V::Int(2)
    ));
    assert!(geo::logs::DESC.has(flags::CTX));
    let r = invoke(
        &mut ctx,
        &m.get("blen"),
        &V::Nil,
        &[V::Bytes(vec![1, 2, 3]), V::List(vec![V::Int(4), V::Int(5)])],
        &[],
    );
    let V::List(r) = r.unwrap() else { panic!() };
    assert!(matches!(r[..], [V::Int(3), V::Int(9)]));
    assert_eq!(m.get("TAU").num(), std::f64::consts::TAU);
}

#[test]
fn classes() {
    let mut ctx = MockCtx::default();
    let m = <Mock as Host>::module_object::<geo::Module>(&mut ctx).unwrap();
    let cls = m.get("Point");
    let p = invoke(
        &mut ctx,
        &cls.get("new"),
        &V::Nil,
        &[V::Num(1.0), V::Num(2.0)],
        &[],
    )
    .unwrap();
    invoke(&mut ctx, &cls.get("scale"), &p, &[V::Num(3.0)], &[]).unwrap();
    assert_eq!(
        invoke(&mut ctx, &cls.get("x"), &p, &[], &[]).unwrap().num(),
        3.0
    );
    invoke(&mut ctx, &cls.get("set_x"), &p, &[V::Num(10.0)], &[]).unwrap();
    assert_eq!(
        invoke(&mut ctx, &cls.get("repr"), &p, &[], &[])
            .unwrap()
            .s(),
        "Point(10, 6)"
    );
    let o = invoke(&mut ctx, &cls.get("origin"), &V::Nil, &[], &[]).unwrap();
    invoke(
        &mut ctx,
        &cls.get("add_from"),
        &o,
        std::slice::from_ref(&p),
        &[],
    )
    .unwrap();
    assert_eq!(
        invoke(&mut ctx, &cls.get("repr"), &o, &[], &[])
            .unwrap()
            .s(),
        "Point(10, 6)"
    );
    let err = invoke(
        &mut ctx,
        &cls.get("add_from"),
        &p,
        std::slice::from_ref(&p),
        &[],
    )
    .unwrap_err();
    assert!(err.contains("already borrowed"), "{err}");
    assert!(matches!(cls.get("hidden"), V::Nil));
    assert_eq!(<geo::Point as Class>::DESC.module, Some("geo"));
}
