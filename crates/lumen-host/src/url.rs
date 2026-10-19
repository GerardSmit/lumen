//! The WHATWG URL interfaces (`URL`, `URLSearchParams`) as native classes over the
//! language-neutral parser in `lumen_common::url`, plus the string helpers Node's `url` module
//! and `fetch` call (`natives`).
//!
//! Install the classes with `lazy_globals::<bindings::Module>`: they are built when a program
//! first touches `URL` or `URLSearchParams`.
//!
//! A `URL` and its `searchParams` share one [`UrlState`]. The `URLSearchParams` holds the state
//! strongly (a mutation rewrites the URL's query) and the state holds the pair list weakly (a
//! new query reparses it in place), so no native edge keeps a JavaScript wrapper alive and no
//! cycle forms. The `[SameObject]` wrapper hangs off the `URL` object in a hidden slot.
use lumen_common::url;
use std::{
    cell::RefCell,
    rc::{Rc, Weak},
};

type Pair = (String, String);
type Pairs = Rc<RefCell<Vec<Pair>>>;

struct UrlState {
    url: url::Url,
    pairs: Weak<RefCell<Vec<Pair>>>,
}

impl UrlState {
    fn new(url: url::Url) -> Rc<RefCell<Self>> {
        Rc::new(RefCell::new(Self {
            url,
            pairs: Weak::new(),
        }))
    }

    fn query_pairs(&self) -> Vec<Pair> {
        url::form_urlencoded_parse(self.url.query.as_deref().unwrap_or(""))
    }

    fn resync_pairs(&self) {
        if let Some(pairs) = self.pairs.upgrade() {
            *pairs.borrow_mut() = self.query_pairs();
        }
    }

    /// Applies a setter to a fresh parse of the current href and keeps the result only when the
    /// setter accepts the value.
    fn edit(&mut self, change: impl FnOnce(&mut url::Url) -> bool) -> bool {
        let Some(mut next) = url::parse_url(&self.url.href(), None) else {
            return false;
        };
        if !change(&mut next) {
            return false;
        }
        self.url = next;
        true
    }

    fn set_href(&mut self, href: &str) -> bool {
        let Some(next) = url::parse_url(href, None) else {
            return false;
        };
        self.url = next;
        self.resync_pairs();
        true
    }
}

fn parse_with_base(input: &str, base: Option<&str>) -> Option<url::Url> {
    match base {
        Some(base) => {
            let base = url::parse_url(base, None)?;
            url::parse_url(input, Some(&base))
        }
        None => url::parse_url(input, None),
    }
}

fn serialize(pairs: &[Pair]) -> String {
    url::form_urlencoded_serialize(pairs.iter().map(|(name, value)| (name.as_str(), value.as_str())))
}

/// `Object.keys`-style copy of an inspect options object (`{ ...options }`, string keys).
fn spread_options(
    ctx: &mut lumen::embed::Ctx,
    options: &lumen::embed::Value,
) -> lumen::embed::OpResult<lumen::embed::Value> {
    use lumen::embed::{OpError, Value};
    let copy = ctx.plain_object(&[]);
    if !matches!(options, Value::Obj(_)) {
        return Ok(copy);
    }
    for key in ctx.reflect_own_keys(options).map_err(OpError::thrown)? {
        let Value::Str(name) = &key else { continue };
        let descriptor = ctx
            .reflect_get_own_property_descriptor(options, &key)
            .map_err(OpError::thrown)?;
        if !matches!(descriptor, Value::Obj(_)) {
            continue;
        }
        let enumerable = ctx
            .member_get(&descriptor, "enumerable")
            .map_err(OpError::thrown)?;
        if !ctx.to_boolean(&enumerable) {
            continue;
        }
        let value = ctx.member_get(options, name.as_str()).map_err(OpError::thrown)?;
        ctx.member_set(&copy, name.as_str(), value).map_err(OpError::thrown)?;
    }
    Ok(copy)
}

/// What `util.inspect` shows of a `URLSearchParams` iterator or list, with Node's own layout.
fn inspect_value(
    ctx: &mut lumen::embed::Ctx,
    inspect: &lumen::embed::Value,
    value: lumen::embed::Value,
    options: &lumen::embed::Value,
) -> lumen::embed::OpResult<String> {
    use lumen::embed::{OpError, Value};
    if !inspect.is_callable() {
        return Err(OpError::type_error("inspect must be a function"));
    }
    let text = ctx
        .invoke(inspect.clone(), Value::Undefined, &[value, options.clone()])
        .map_err(OpError::thrown)?;
    Ok(ctx.coerce_string(&text).map_err(OpError::thrown)?.to_string())
}

/// `ctx.stylize(text, style)` of a `util.inspect` options object.
fn stylize(
    ctx: &mut lumen::embed::Ctx,
    options: &lumen::embed::Value,
    text: &str,
    style: &str,
) -> lumen::embed::OpResult<lumen::embed::Value> {
    use lumen::embed::{OpError, Value};
    let stylize = ctx.member_get(options, "stylize").map_err(OpError::thrown)?;
    ctx.invoke(
        stylize,
        options.clone(),
        &[Value::from_string(text.into()), Value::from_string(style.into())],
    )
    .map_err(OpError::thrown)
}

/// The depth-limited options `util.inspect` hands nested values: `{ ...options, depth: depth - 1 }`.
fn nested_options(
    ctx: &mut lumen::embed::Ctx,
    options: &lumen::embed::Value,
    recurse_times: &lumen::embed::Value,
) -> lumen::embed::OpResult<lumen::embed::Value> {
    use lumen::embed::{OpError, Value};
    let nested = spread_options(ctx, options)?;
    if let Value::Num(depth) = recurse_times {
        ctx.member_set(&nested, "depth", Value::Num(depth - 1.0))
            .map_err(OpError::thrown)?;
    }
    Ok(nested)
}

fn strip_ansi_len(text: &str) -> usize {
    let bytes = text.as_bytes();
    let mut kept = 0;
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == 0x1b && bytes.get(at + 1) == Some(&b'[') {
            let digits = bytes[at + 2..]
                .iter()
                .take(2)
                .take_while(|byte| byte.is_ascii_digit())
                .count();
            if digits > 0 && bytes.get(at + 2 + digits) == Some(&b'm') {
                at += 3 + digits;
                continue;
            }
        }
        let width = text[at..].chars().next().map_or(1, char::len_utf16);
        kept += width;
        at += text[at..].chars().next().map_or(1, char::len_utf8);
    }
    kept
}

/// Hooks Node's `url` module and `fetch` call by name: the IDNA and formatting entry points of
/// the parser and an origin lookup. Installed as a namespace, not as globals.
#[lumen_bind::module(name = "__lumenUrl")]
pub mod natives {
    use super::*;
    use crate::webidl::Usv;

    /// The serialized origin of `input`, or `undefined` when it does not parse.
    #[op]
    pub fn origin(input: Usv) -> Option<String> {
        url::parse_url(&input.0, None).map(|url| url.origin())
    }

    #[op(rename(js = "domainToASCII"))]
    pub fn domain_to_ascii(input: Usv) -> String {
        url::domain_to_ascii(&input.0)
    }
    #[op(rename(js = "domainToUnicode"))]
    pub fn domain_to_unicode(input: Usv) -> String {
        url::domain_to_unicode(&input.0)
    }
    #[op(rename(js = "toASCII"))]
    pub fn to_ascii(input: Usv) -> String {
        url::idna_to_ascii(&input.0)
    }
    #[op(rename(js = "toUnicode"))]
    pub fn to_unicode(input: Usv) -> String {
        url::domain_to_unicode_raw(&input.0)
    }
    #[op]
    pub fn format(href: String, hash: bool, unicode: bool, search: bool, auth: bool) -> String {
        url::format(&href, hash, unicode, search, auth).unwrap_or(href)
    }
}

/// `URL`, `URLSearchParams` and the iterator class behind `URLSearchParams` iteration.
#[lumen_bind::module(name = "webUrl")]
pub mod bindings {
    use super::*;
    use crate::webidl::{
        coded, invalid_arg_type, invalid_this, usv_string, OptUsv, Usv,
    };
    use lumen::embed::{Ctx, OpError, OpResult, This, Value};
    use std::collections::HashMap;

    const SEARCH_PARAMS_SLOT: &str = "#\u{0}url_search_params";
    const ITERATOR_TAG: &str = "URLSearchParams Iterator";

    fn invalid_url(input: &str, base: Option<&str>) -> OpError {
        let error = OpError::type_error("Invalid URL")
            .with_code("ERR_INVALID_URL")
            .with_prop("input", input);
        match base {
            Some(base) => error.with_prop("base", base),
            None => error,
        }
    }

    fn tuple_error() -> OpError {
        coded(
            OpError::type_error("Each query pair must be an iterable [name, value] tuple"),
            "ERR_INVALID_TUPLE",
        )
    }

    fn string(text: &str) -> Value {
        Value::from_string(text.to_owned())
    }

    #[class(name = "URL", hint(js(webidl)))]
    pub struct WebUrl {
        state: Rc<RefCell<UrlState>>,
    }

    #[class(name = "URLSearchParams", hint(js(webidl)))]
    pub struct WebUrlSearchParams {
        pairs: Pairs,
        link: Option<Rc<RefCell<UrlState>>>,
    }

    #[derive(Clone, Copy)]
    enum IterKind {
        Keys,
        Values,
        Entries,
    }

    #[class(name = "URLSearchParams Iterator", skip(js), hint(js(webidl, iterator)))]
    pub struct WebUrlSearchParamsIterator {
        pairs: Pairs,
        kind: IterKind,
        index: usize,
    }

    fn state_of(ctx: &Ctx, this: &Value) -> OpResult<Rc<RefCell<UrlState>>> {
        ctx.instance_data::<WebUrl>(this)
            .map(|url| url.borrow().state.clone())
            .ok_or_else(|| invalid_this("URL"))
    }

    fn read<R>(ctx: &Ctx, this: &Value, read: impl FnOnce(&url::Url) -> R) -> OpResult<R> {
        let state = state_of(ctx, this)?;
        let state = state.borrow();
        Ok(read(&state.url))
    }

    fn edit(
        ctx: &Ctx,
        this: &Value,
        resync: bool,
        change: impl FnOnce(&mut url::Url) -> bool,
    ) -> OpResult<()> {
        let state = state_of(ctx, this)?;
        let mut state = state.borrow_mut();
        if state.edit(change) && resync {
            state.resync_pairs();
        }
        Ok(())
    }

    fn search_params_of(
        ctx: &Ctx,
        this: &Value,
    ) -> OpResult<Rc<RefCell<WebUrlSearchParams>>> {
        ctx.instance_data::<WebUrlSearchParams>(this)
            .ok_or_else(|| invalid_this("URLSearchParams"))
    }

    fn pairs_of(ctx: &Ctx, this: &Value) -> OpResult<(Pairs, Option<Rc<RefCell<UrlState>>>)> {
        let params = search_params_of(ctx, this)?;
        let params = params.borrow();
        Ok((params.pairs.clone(), params.link.clone()))
    }

    /// Writes the pair list back into the query of the URL it belongs to.
    fn flush(pairs: &Pairs, link: &Option<Rc<RefCell<UrlState>>>) {
        let Some(link) = link else { return };
        let query = serialize(&pairs.borrow());
        link.borrow_mut().edit(|url| {
            url.set_search(&query);
            true
        });
    }

    #[methods]
    impl WebUrl {
        #[constructor(hint(js(missing_message = "The \"url\" argument must be specified", missing_code = "ERR_MISSING_ARGS")))]
        fn new(
            ctx: &mut Ctx,
            url: Value,
            #[default(Value::Undefined)] base: Value,
        ) -> OpResult<Self> {
            let input = ctx.coerce_string(&url).map_err(OpError::thrown)?;
            let base = match &base {
                Value::Undefined => None,
                base => Some(ctx.coerce_string(base).map_err(OpError::thrown)?),
            };
            let parsed = parse_with_base(
                &lumen::well_formed_utf8(&input),
                base.as_deref().map(lumen::well_formed_utf8).as_deref(),
            )
            .ok_or_else(|| invalid_url(&input, base.as_deref()))?;
            Ok(Self {
                state: UrlState::new(parsed),
            })
        }

        #[method(hint(js(missing_message = "The \"url\" argument must be specified", missing_code = "ERR_MISSING_ARGS")))]
        fn parse(
            ctx: &mut Ctx,
            url: Usv,
            #[default(OptUsv::default())] base: OptUsv,
        ) -> Value {
            match parse_with_base(&url.0, base.0.as_deref()) {
                Some(parsed) => ctx.new_instance(WebUrl {
                    state: UrlState::new(parsed),
                }),
                None => Value::Null,
            }
        }

        #[method(hint(js(missing_message = "The \"url\" argument must be specified", missing_code = "ERR_MISSING_ARGS")))]
        fn can_parse(url: Usv, #[default(OptUsv::default())] base: OptUsv) -> bool {
            parse_with_base(&url.0, base.0.as_deref()).is_some()
        }

        #[method(
            name = "createObjectURL",
            hint(js(
                missing_message = "The \"obj\" argument must be an instance of Blob. Received undefined",
                missing_code = "ERR_INVALID_ARG_TYPE"
            ))
        )]
        fn create_object_url(ctx: &mut Ctx, obj: Value) -> OpResult<String> {
            crate::blob::create_object_url(ctx, &obj)?
                .ok_or_else(|| invalid_arg_type(ctx, "obj", "an instance of Blob", &obj))
        }

        #[method(
            name = "revokeObjectURL",
            hint(js(missing_message = "The \"url\" argument must be specified", missing_code = "ERR_MISSING_ARGS"))
        )]
        fn revoke_object_url(ctx: &mut Ctx, url: Usv) -> OpResult<()> {
            let Some(parsed) = url::parse_url(&url.0, None) else {
                return Ok(());
            };
            if parsed.scheme != "blob" {
                return Ok(());
            }
            crate::blob::revoke_object_url(ctx, &parsed.href());
            Ok(())
        }

        #[method(hint(js(symbol_for = "nodejs.util.inspect.custom")))]
        fn inspect_custom(
            this: This<Value>,
            ctx: &mut Ctx,
            #[default(Value::Undefined)] depth: Value,
            #[default(Value::Undefined)] options: Value,
            #[default(Value::Undefined)] inspect: Value,
        ) -> OpResult<Value> {
            let url = read(ctx, &this, url::Url::clone)?;
            if matches!(depth, Value::Num(depth) if depth < 0.0) {
                return Ok(this.0.clone());
            }
            let search_params = Self::search_params(This(this.0.clone()), ctx)?;
            let fallback = ctx.class_constructor::<WebUrl>();
            let constructor = ctx
                .member_get(&this, "constructor")
                .map_err(OpError::thrown)?;
            let name = ctx
                .member_get(&constructor, "name")
                .ok()
                .and_then(|name| ctx.coerce_string(&name).ok())
                .filter(|name| constructor.is_callable() && !name.is_empty());
            let (constructor, name) = match name {
                Some(name) => (constructor, name.to_string()),
                None => (fallback, "URL".to_owned()),
            };
            let prototype = ctx.plain_object(&[("constructor", constructor)]);
            let shown = ctx.new_object_with_proto(&prototype);
            let host = match &url.host {
                None => String::new(),
                Some(host) => match url.port {
                    Some(port) => format!("{host}:{port}"),
                    None => host.clone(),
                },
            };
            let search = url
                .query
                .as_deref()
                .filter(|query| !query.is_empty())
                .map_or_else(String::new, |query| format!("?{query}"));
            let hash = url
                .fragment
                .as_deref()
                .filter(|fragment| !fragment.is_empty())
                .map_or_else(String::new, |fragment| format!("#{fragment}"));
            for (key, value) in [
                ("href", string(&url.href())),
                ("origin", string(&url.origin())),
                ("protocol", string(&format!("{}:", url.scheme))),
                ("username", string(&url.username)),
                ("password", string(&url.password)),
                ("host", string(&host)),
                ("hostname", string(url.hostname())),
                (
                    "port",
                    string(&url.port.map(|port| port.to_string()).unwrap_or_default()),
                ),
                ("pathname", string(&url.path)),
                ("search", string(&search)),
                ("searchParams", search_params),
                ("hash", string(&hash)),
            ] {
                ctx.member_set(&shown, key, value).map_err(OpError::thrown)?;
            }
            if inspect.is_callable() {
                let text = inspect_value(ctx, &inspect, shown, &options)?;
                return Ok(Value::from_string(format!("{name} {text}")));
            }
            let global = ctx.global_object();
            let json = ctx.member_get(&global, "JSON").map_err(OpError::thrown)?;
            let stringify = ctx.member_get(&json, "stringify").map_err(OpError::thrown)?;
            let text = ctx.invoke(stringify, json, &[shown]).map_err(OpError::thrown)?;
            let text = ctx.coerce_string(&text).map_err(OpError::thrown)?;
            Ok(Value::from_string(format!("{name} {text}")))
        }

        fn to_string(this: This<Value>, ctx: &mut Ctx) -> OpResult<String> {
            read(ctx, &this, url::Url::href)
        }

        #[method(name = "toJSON")]
        fn to_json(this: This<Value>, ctx: &mut Ctx) -> OpResult<String> {
            read(ctx, &this, url::Url::href)
        }

        #[getter]
        fn href(this: This<Value>, ctx: &mut Ctx) -> OpResult<String> {
            read(ctx, &this, url::Url::href)
        }

        #[setter]
        fn set_href(this: This<Value>, ctx: &mut Ctx, value: Usv) -> OpResult<()> {
            let state = state_of(ctx, &this)?;
            if state.borrow_mut().set_href(&value.0) {
                Ok(())
            } else {
                Err(invalid_url(&value.0, None))
            }
        }

        #[getter]
        fn origin(this: This<Value>, ctx: &mut Ctx) -> OpResult<String> {
            read(ctx, &this, url::Url::origin)
        }

        #[getter]
        fn protocol(this: This<Value>, ctx: &mut Ctx) -> OpResult<String> {
            read(ctx, &this, |url| format!("{}:", url.scheme))
        }

        #[setter]
        fn set_protocol(this: This<Value>, ctx: &mut Ctx, value: Usv) -> OpResult<()> {
            edit(ctx, &this, false, |url| url.set_protocol(&value.0))
        }

        #[getter]
        fn username(this: This<Value>, ctx: &mut Ctx) -> OpResult<String> {
            read(ctx, &this, |url| url.username.clone())
        }

        #[setter]
        fn set_username(this: This<Value>, ctx: &mut Ctx, value: Usv) -> OpResult<()> {
            edit(ctx, &this, false, |url| url.set_username(&value.0))
        }

        #[getter]
        fn password(this: This<Value>, ctx: &mut Ctx) -> OpResult<String> {
            read(ctx, &this, |url| url.password.clone())
        }

        #[setter]
        fn set_password(this: This<Value>, ctx: &mut Ctx, value: Usv) -> OpResult<()> {
            edit(ctx, &this, false, |url| url.set_password(&value.0))
        }

        #[getter]
        fn host(this: This<Value>, ctx: &mut Ctx) -> OpResult<String> {
            read(ctx, &this, |url| match &url.host {
                None => String::new(),
                Some(host) => match url.port {
                    Some(port) => format!("{host}:{port}"),
                    None => host.clone(),
                },
            })
        }

        #[setter]
        fn set_host(this: This<Value>, ctx: &mut Ctx, value: Usv) -> OpResult<()> {
            edit(ctx, &this, false, |url| url.set_host(&value.0))
        }

        #[getter]
        fn hostname(this: This<Value>, ctx: &mut Ctx) -> OpResult<String> {
            read(ctx, &this, |url| url.hostname().to_owned())
        }

        #[setter]
        fn set_hostname(this: This<Value>, ctx: &mut Ctx, value: Usv) -> OpResult<()> {
            edit(ctx, &this, false, |url| url.set_hostname(&value.0))
        }

        #[getter]
        fn port(this: This<Value>, ctx: &mut Ctx) -> OpResult<String> {
            read(ctx, &this, |url| {
                url.port.map(|port| port.to_string()).unwrap_or_default()
            })
        }

        #[setter]
        fn set_port(this: This<Value>, ctx: &mut Ctx, value: Usv) -> OpResult<()> {
            edit(ctx, &this, false, |url| url.set_port(&value.0))
        }

        #[getter]
        fn pathname(this: This<Value>, ctx: &mut Ctx) -> OpResult<String> {
            read(ctx, &this, |url| url.path.clone())
        }

        #[setter]
        fn set_pathname(this: This<Value>, ctx: &mut Ctx, value: Usv) -> OpResult<()> {
            edit(ctx, &this, false, |url| url.set_pathname(&value.0))
        }

        #[getter]
        fn search(this: This<Value>, ctx: &mut Ctx) -> OpResult<String> {
            read(ctx, &this, |url| match url.query.as_deref() {
                Some(query) if !query.is_empty() => format!("?{query}"),
                _ => String::new(),
            })
        }

        #[setter]
        fn set_search(this: This<Value>, ctx: &mut Ctx, value: Usv) -> OpResult<()> {
            edit(ctx, &this, true, |url| {
                url.set_search(&value.0);
                true
            })
        }

        /// The same `URLSearchParams` object on every read, linked to this URL's query.
        #[getter]
        fn search_params(this: This<Value>, ctx: &mut Ctx) -> OpResult<Value> {
            let state = state_of(ctx, &this)?;
            if let Some(existing) = ctx.native_private_value_slot(&this, SEARCH_PARAMS_SLOT) {
                return Ok(existing);
            }
            let pairs: Pairs = Rc::new(RefCell::new(state.borrow().query_pairs()));
            state.borrow_mut().pairs = Rc::downgrade(&pairs);
            let params = ctx.new_instance(WebUrlSearchParams {
                pairs,
                link: Some(state),
            });
            let _ = ctx.define_native_private_value_slot(&this, SEARCH_PARAMS_SLOT, params.clone());
            Ok(params)
        }

        #[getter]
        fn hash(this: This<Value>, ctx: &mut Ctx) -> OpResult<String> {
            read(ctx, &this, |url| match url.fragment.as_deref() {
                Some(fragment) if !fragment.is_empty() => format!("#{fragment}"),
                _ => String::new(),
            })
        }

        #[setter]
        fn set_hash(this: This<Value>, ctx: &mut Ctx, value: Usv) -> OpResult<()> {
            edit(ctx, &this, false, |url| {
                url.set_hash(&value.0);
                true
            })
        }
    }

    /// One `[name, value]` pair of a sequence initializer.
    fn query_pair(ctx: &mut Ctx, pair: Value) -> OpResult<Pair> {
        if matches!(pair, Value::Null | Value::Undefined) {
            return Err(tuple_error());
        }
        if ctx.is_array_value(&pair).map_err(OpError::thrown)? {
            let length = ctx.member_get(&pair, "length").map_err(OpError::thrown)?;
            if !matches!(length, Value::Num(length) if length == 2.0) {
                return Err(tuple_error());
            }
            let name = ctx.member_get(&pair, "0").map_err(OpError::thrown)?;
            let name = usv_string(ctx, &name).map_err(OpError::thrown)?;
            let value = ctx.member_get(&pair, "1").map_err(OpError::thrown)?;
            let value = usv_string(ctx, &value).map_err(OpError::thrown)?;
            return Ok((name, value));
        }
        if !matches!(pair, Value::Obj(_)) {
            return Err(tuple_error());
        }
        let iterator = ctx.well_known_symbol("iterator").unwrap_or(Value::Undefined);
        let method = ctx
            .reflect_get(&pair, &iterator, &pair)
            .map_err(OpError::thrown)?;
        if !method.is_callable() {
            return Err(tuple_error());
        }
        let mut items = ctx.convert_iterable(&pair, usize::MAX, |ctx, element| {
            usv_string(ctx, &element).map_err(OpError::thrown)
        })?;
        if items.len() != 2 {
            return Err(tuple_error());
        }
        let value = items.pop().unwrap_or_default();
        let name = items.pop().unwrap_or_default();
        Ok((name, value))
    }

    /// The pairs a `URLSearchParams` constructor argument stands for.
    fn init_pairs(ctx: &mut Ctx, init: &Value) -> OpResult<Vec<Pair>> {
        let Value::Obj(_) = init else {
            if matches!(init, Value::Undefined | Value::Null) {
                return Ok(Vec::new());
            }
            let text = usv_string(ctx, init).map_err(OpError::thrown)?;
            return Ok(url::form_urlencoded_parse(
                text.strip_prefix('?').unwrap_or(&text),
            ));
        };
        let iterator = ctx.well_known_symbol("iterator").unwrap_or(Value::Undefined);
        let method = ctx.reflect_get(init, &iterator, init).map_err(OpError::thrown)?;
        if let Some(other) = ctx.instance_data::<WebUrlSearchParams>(init) {
            let class = ctx.class_constructor::<WebUrlSearchParams>();
            let prototype = ctx.member_get(&class, "prototype").map_err(OpError::thrown)?;
            let default = ctx
                .reflect_get(&prototype, &iterator, &prototype)
                .map_err(OpError::thrown)?;
            if ctx.values_strict_equal(&method, &default) {
                return Ok(other.borrow().pairs.borrow().clone());
            }
        }
        if !matches!(method, Value::Undefined | Value::Null) {
            if !method.is_callable() {
                return Err(coded(
                    OpError::type_error("Query pairs must be iterable"),
                    "ERR_ARG_NOT_ITERABLE",
                ));
            }
            return ctx.convert_iterable(init, usize::MAX, query_pair);
        }
        let mut pairs: Vec<Pair> = Vec::new();
        let mut visited: HashMap<String, usize> = HashMap::new();
        for key in ctx.reflect_own_keys(init).map_err(OpError::thrown)? {
            let descriptor = ctx
                .reflect_get_own_property_descriptor(init, &key)
                .map_err(OpError::thrown)?;
            if !matches!(descriptor, Value::Obj(_)) {
                continue;
            }
            let enumerable = ctx
                .member_get(&descriptor, "enumerable")
                .map_err(OpError::thrown)?;
            if !ctx.to_boolean(&enumerable) {
                continue;
            }
            let name = usv_string(ctx, &key).map_err(OpError::thrown)?;
            let value = ctx.reflect_get(init, &key, init).map_err(OpError::thrown)?;
            let value = usv_string(ctx, &value).map_err(OpError::thrown)?;
            match visited.get(&name) {
                Some(&at) => pairs[at].1 = value,
                None => {
                    visited.insert(name.clone(), pairs.len());
                    pairs.push((name, value));
                }
            }
        }
        Ok(pairs)
    }

    #[methods]
    impl WebUrlSearchParams {
        #[constructor]
        fn new(ctx: &mut Ctx, #[default(Value::Undefined)] init: Value) -> OpResult<Self> {
            Ok(Self {
                pairs: Rc::new(RefCell::new(init_pairs(ctx, &init)?)),
                link: None,
            })
        }

        #[method(hint(js(symbol_for = "nodejs.util.inspect.custom")))]
        fn inspect_custom(
            this: This<Value>,
            ctx: &mut Ctx,
            #[default(Value::Undefined)] recurse_times: Value,
            #[default(Value::Undefined)] options: Value,
            #[default(Value::Undefined)] inspect: Value,
        ) -> OpResult<Value> {
            let (pairs, _) = pairs_of(ctx, &this)?;
            if matches!(recurse_times, Value::Num(depth) if depth < 0.0) {
                return stylize(ctx, &options, "[Object]", "special");
            }
            let nested = nested_options(ctx, &options, &recurse_times)?;
            let list = pairs.borrow().clone();
            let mut output = Vec::with_capacity(list.len());
            for (name, value) in list {
                let name = inspect_value(ctx, &inspect, string(&name), &nested)?;
                let value = inspect_value(ctx, &inspect, string(&value), &nested)?;
                output.push(format!("{name} => {value}"));
            }
            let separator = ", ";
            let length = output
                .iter()
                .map(|entry| strip_ansi_len(entry) + separator.len())
                .sum::<usize>()
                .saturating_sub(separator.len());
            let break_length = ctx
                .member_get(&options, "breakLength")
                .map_err(OpError::thrown)?;
            let constructor = ctx
                .member_get(&this, "constructor")
                .map_err(OpError::thrown)?;
            let name = ctx
                .member_get(&constructor, "name")
                .map_err(OpError::thrown)?;
            let name = ctx.coerce_string(&name).map_err(OpError::thrown)?;
            let too_long = matches!(break_length, Value::Num(limit) if length as f64 > limit);
            Ok(Value::from_string(if too_long {
                format!("{name} {{\n  {} }}", output.join(",\n  "))
            } else if output.is_empty() {
                format!("{name} {{}}")
            } else {
                format!("{name} {{ {} }}", output.join(separator))
            }))
        }

        #[getter]
        fn size(this: This<Value>, ctx: &mut Ctx) -> OpResult<f64> {
            let (pairs, _) = pairs_of(ctx, &this)?;
            let size = pairs.borrow().len();
            Ok(size as f64)
        }

        #[method(hint(js(missing_message = "The \"name\" and \"value\" arguments must be specified", missing_code = "ERR_MISSING_ARGS")))]
        fn append(this: This<Value>, ctx: &mut Ctx, name: Usv, value: Usv) -> OpResult<()> {
            let (pairs, link) = pairs_of(ctx, &this)?;
            pairs.borrow_mut().push((name.0, value.0));
            flush(&pairs, &link);
            Ok(())
        }

        #[method(hint(js(missing_message = "The \"name\" argument must be specified", missing_code = "ERR_MISSING_ARGS")))]
        fn delete(
            this: This<Value>,
            ctx: &mut Ctx,
            name: Usv,
            #[default(OptUsv::default())] value: OptUsv,
        ) -> OpResult<()> {
            let (pairs, link) = pairs_of(ctx, &this)?;
            pairs.borrow_mut().retain(|(candidate, current)| {
                !(*candidate == name.0 && value.0.as_ref().is_none_or(|value| current == value))
            });
            flush(&pairs, &link);
            Ok(())
        }

        #[method(hint(js(missing_message = "The \"name\" argument must be specified", missing_code = "ERR_MISSING_ARGS")))]
        fn get(this: This<Value>, ctx: &mut Ctx, name: Usv) -> OpResult<Value> {
            let (pairs, _) = pairs_of(ctx, &this)?;
            let pairs = pairs.borrow();
            Ok(pairs
                .iter()
                .find(|(candidate, _)| *candidate == name.0)
                .map_or(Value::Null, |(_, value)| string(value)))
        }

        #[method(hint(js(missing_message = "The \"name\" argument must be specified", missing_code = "ERR_MISSING_ARGS")))]
        fn get_all(this: This<Value>, ctx: &mut Ctx, name: Usv) -> OpResult<Vec<String>> {
            let (pairs, _) = pairs_of(ctx, &this)?;
            let pairs = pairs.borrow();
            Ok(pairs
                .iter()
                .filter(|(candidate, _)| *candidate == name.0)
                .map(|(_, value)| value.clone())
                .collect())
        }

        #[method(hint(js(missing_message = "The \"name\" argument must be specified", missing_code = "ERR_MISSING_ARGS")))]
        fn has(
            this: This<Value>,
            ctx: &mut Ctx,
            name: Usv,
            #[default(OptUsv::default())] value: OptUsv,
        ) -> OpResult<bool> {
            let (pairs, _) = pairs_of(ctx, &this)?;
            let pairs = pairs.borrow();
            Ok(pairs.iter().any(|(candidate, current)| {
                *candidate == name.0 && value.0.as_ref().is_none_or(|value| current == value)
            }))
        }

        #[method(hint(js(missing_message = "The \"name\" and \"value\" arguments must be specified", missing_code = "ERR_MISSING_ARGS")))]
        fn set(this: This<Value>, ctx: &mut Ctx, name: Usv, value: Usv) -> OpResult<()> {
            let (pairs, link) = pairs_of(ctx, &this)?;
            {
                let mut pairs = pairs.borrow_mut();
                let mut found = false;
                pairs.retain_mut(|(candidate, current)| {
                    if *candidate != name.0 {
                        return true;
                    }
                    if found {
                        return false;
                    }
                    found = true;
                    current.clone_from(&value.0);
                    true
                });
                if !found {
                    pairs.push((name.0, value.0));
                }
            }
            flush(&pairs, &link);
            Ok(())
        }

        /// Stable, by UTF-16 code units of the name.
        fn sort(this: This<Value>, ctx: &mut Ctx) -> OpResult<()> {
            let (pairs, link) = pairs_of(ctx, &this)?;
            pairs
                .borrow_mut()
                .sort_by(|(left, _), (right, _)| left.encode_utf16().cmp(right.encode_utf16()));
            flush(&pairs, &link);
            Ok(())
        }

        #[method(hint(js(also_iterator)))]
        fn entries(
            this: This<Value>,
            ctx: &mut Ctx,
        ) -> OpResult<WebUrlSearchParamsIterator> {
            iterator(ctx, &this, IterKind::Entries)
        }

        fn keys(this: This<Value>, ctx: &mut Ctx) -> OpResult<WebUrlSearchParamsIterator> {
            iterator(ctx, &this, IterKind::Keys)
        }

        fn values(this: This<Value>, ctx: &mut Ctx) -> OpResult<WebUrlSearchParamsIterator> {
            iterator(ctx, &this, IterKind::Values)
        }

        #[method(hint(js(
            missing_message = "The \"callback\" argument must be of type function. Received undefined",
            missing_code = "ERR_INVALID_ARG_TYPE"
        )))]
        fn for_each(
            this: This<Value>,
            ctx: &mut Ctx,
            callback: Value,
            #[default(Value::Undefined)] this_arg: Value,
        ) -> OpResult<()> {
            let (pairs, _) = pairs_of(ctx, &this)?;
            if !callback.is_callable() {
                return Err(invalid_arg_type(
                    ctx,
                    "callback",
                    "of type function",
                    &callback,
                ));
            }
            let mut index = 0;
            loop {
                let pair = pairs.borrow().get(index).cloned();
                let Some((name, value)) = pair else {
                    return Ok(());
                };
                ctx.invoke(
                    callback.clone(),
                    this_arg.clone(),
                    &[string(&value), string(&name), this.0.clone()],
                )
                .map_err(OpError::thrown)?;
                index += 1;
            }
        }

        fn to_string(this: This<Value>, ctx: &mut Ctx) -> OpResult<String> {
            let (pairs, _) = pairs_of(ctx, &this)?;
            let text = serialize(&pairs.borrow());
            Ok(text)
        }
    }

    fn iterator(
        ctx: &mut Ctx,
        this: &Value,
        kind: IterKind,
    ) -> OpResult<WebUrlSearchParamsIterator> {
        let (pairs, _) = pairs_of(ctx, this)?;
        Ok(WebUrlSearchParamsIterator {
            pairs,
            kind,
            index: 0,
        })
    }

    #[methods]
    impl WebUrlSearchParamsIterator {
        #[proto(next)]
        fn next(this: This<Value>, ctx: &mut Ctx) -> OpResult<Option<Value>> {
            let state = ctx
                .instance_data::<WebUrlSearchParamsIterator>(&this)
                .ok_or_else(|| invalid_this("URLSearchParamsIterator"))?;
            let mut state = state.borrow_mut();
            let pair = state.pairs.borrow().get(state.index).cloned();
            let Some((name, value)) = pair else {
                return Ok(None);
            };
            state.index += 1;
            Ok(Some(match state.kind {
                IterKind::Keys => string(&name),
                IterKind::Values => string(&value),
                IterKind::Entries => ctx.make_array(vec![string(&name), string(&value)]),
            }))
        }

        #[method(hint(js(symbol_for = "nodejs.util.inspect.custom")))]
        fn inspect_custom(
            this: This<Value>,
            ctx: &mut Ctx,
            #[default(Value::Undefined)] recurse_times: Value,
            #[default(Value::Undefined)] options: Value,
            #[default(Value::Undefined)] inspect: Value,
        ) -> OpResult<Value> {
            let state = ctx
                .instance_data::<WebUrlSearchParamsIterator>(&this)
                .ok_or_else(|| invalid_this("URLSearchParamsIterator"))?;
            if matches!(recurse_times, Value::Num(depth) if depth < 0.0) {
                return stylize(ctx, &options, "[Object]", "special");
            }
            let nested = nested_options(ctx, &options, &recurse_times)?;
            let (remaining, kind) = {
                let state = state.borrow();
                let pairs = state.pairs.borrow();
                (
                    pairs.get(state.index..).unwrap_or_default().to_vec(),
                    state.kind,
                )
            };
            let items: Vec<Value> = remaining
                .iter()
                .map(|(name, value)| match kind {
                    IterKind::Keys => string(name),
                    IterKind::Values => string(value),
                    IterKind::Entries => ctx.make_array(vec![string(name), string(value)]),
                })
                .collect();
            let as_list = ctx.make_array(items.clone());
            let multiline = inspect_value(ctx, &inspect, as_list, &nested)?.contains('\n');
            let mut shown = Vec::with_capacity(items.len());
            for item in items {
                shown.push(inspect_value(ctx, &inspect, item, &nested)?);
            }
            let body = if multiline {
                format!("\n  {}", shown.join(",\n  "))
            } else {
                format!(" {}", shown.join(", "))
            };
            Ok(Value::from_string(format!("{ITERATOR_TAG} {{{body} }}")))
        }
    }

}
