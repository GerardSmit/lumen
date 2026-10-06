//! `URLPattern` as a native class over the vendored `urlpattern` crate (see
//! `vendor/urlpattern/UPSTREAM.md`), which compiles each component to an ECMAScript regular
//! expression run by `lumen_common::regex` and canonicalizes through `lumen_common::url`, so a
//! pattern behaves like the `URL` class and a script `RegExp`. This module is the WebIDL layer:
//! overload resolution, dictionary conversion and result objects.
//!
//! Install with `lazy_globals::<bindings::Module>`.

use urlpattern::url::Url;
use urlpattern::{
    UrlPattern, UrlPatternComponentResult, UrlPatternInit, UrlPatternMatchInput,
    UrlPatternOptions, UrlPatternResult,
};

/// `URLPatternInit` members in WebIDL declaration order (the order of a result's `inputs`).
const INIT_MEMBERS: [&str; 9] = [
    "protocol", "username", "password", "hostname", "port", "pathname", "search", "hash",
    "baseURL",
];
/// The same members in the lexicographic order WebIDL reads a dictionary in.
const INIT_READ_ORDER: [&str; 9] = [
    "baseURL", "hash", "hostname", "password", "pathname", "port", "protocol", "search",
    "username",
];

/// A `URLPatternInit` as read from script: `baseURL` is still text.
#[derive(Default, Clone)]
struct RawInit {
    members: [Option<String>; 9],
}

impl RawInit {
    fn member(&self, name: &str) -> Option<&String> {
        INIT_MEMBERS
            .iter()
            .position(|member| *member == name)
            .and_then(|index| self.members[index].as_ref())
    }

    fn into_init(&self) -> Result<UrlPatternInit, urlpattern::url::ParseError> {
        let text = |name| self.member(name).cloned();
        Ok(UrlPatternInit {
            protocol: text("protocol"),
            username: text("username"),
            password: text("password"),
            hostname: text("hostname"),
            port: text("port"),
            pathname: text("pathname"),
            search: text("search"),
            hash: text("hash"),
            base_url: self.member("baseURL").map(|base| Url::parse(base)).transpose()?,
        })
    }
}

enum Input {
    Init(RawInit),
    Text(String),
}

#[lumen_bind::module(name = "webUrlPattern")]
pub mod bindings {
    use super::*;
    use crate::webidl::{usv_string, OptUsv};
    use lumen::embed::{Ctx, OpError, OpResult, Value};
    use lumen_bind::Passed;

    fn read_init(ctx: &mut Ctx, object: &Value) -> OpResult<RawInit> {
        let mut raw = RawInit::default();
        for name in INIT_READ_ORDER {
            let value = ctx.member_get(object, name).map_err(OpError::thrown)?;
            if matches!(value, Value::Undefined) {
                continue;
            }
            let text = usv_string(ctx, &value).map_err(OpError::thrown)?;
            let index = INIT_MEMBERS.iter().position(|member| *member == name);
            raw.members[index.unwrap()] = Some(text);
        }
        Ok(raw)
    }

    /// `(USVString or URLPatternInit)`: `undefined` and `null` select the dictionary, as does
    /// any object; everything else converts to a string.
    fn read_input(ctx: &mut Ctx, value: &Value) -> OpResult<Input> {
        match value {
            Value::Undefined | Value::Null => Ok(Input::Init(RawInit::default())),
            Value::Obj(_) => read_init(ctx, value).map(Input::Init),
            other => usv_string(ctx, other).map(Input::Text).map_err(OpError::thrown),
        }
    }

    /// `URLPatternOptions`: `undefined` / `null` give the defaults, other non-objects throw.
    fn read_options(ctx: &mut Ctx, value: &Value) -> OpResult<UrlPatternOptions> {
        match value {
            Value::Undefined | Value::Null => Ok(UrlPatternOptions::default()),
            Value::Obj(_) => {
                let ignore_case = ctx.member_get(value, "ignoreCase").map_err(OpError::thrown)?;
                Ok(UrlPatternOptions {
                    ignore_case: ctx.to_boolean(&ignore_case),
                })
            }
            _ => Err(OpError::type_error(
                "Failed to construct 'URLPattern': The options argument is not an object",
            )),
        }
    }

    fn construct_error(error: impl std::fmt::Display) -> OpError {
        OpError::type_error(format!("Failed to construct 'URLPattern': {error}"))
    }

    fn string(text: &str) -> Value {
        Value::from_string(text.to_owned())
    }

    fn init_object(ctx: &mut Ctx, raw: &RawInit) -> Value {
        let entries: Vec<(&str, Value)> = INIT_MEMBERS
            .iter()
            .zip(&raw.members)
            .filter_map(|(name, text)| text.as_deref().map(|text| (*name, string(text))))
            .collect();
        ctx.plain_object(&entries)
    }

    fn component_result(ctx: &mut Ctx, component: &UrlPatternComponentResult) -> Value {
        let groups: Vec<(&str, Value)> = component
            .groups
            .iter()
            .map(|(name, value)| {
                (name.as_str(), value.as_deref().map_or(Value::Undefined, string))
            })
            .collect();
        let groups = ctx.plain_object(&groups);
        ctx.plain_object(&[("input", string(&component.input)), ("groups", groups)])
    }

    fn match_result(ctx: &mut Ctx, inputs: Vec<Value>, result: &UrlPatternResult) -> Value {
        let inputs = ctx.make_array(inputs);
        let mut entries = vec![("inputs", inputs)];
        for (name, component) in [
            ("protocol", &result.protocol),
            ("username", &result.username),
            ("password", &result.password),
            ("hostname", &result.hostname),
            ("port", &result.port),
            ("pathname", &result.pathname),
            ("search", &result.search),
            ("hash", &result.hash),
        ] {
            entries.push((name, component_result(ctx, component)));
        }
        ctx.plain_object(&entries)
    }

    #[class(name = "URLPattern", hint(js(webidl)))]
    pub struct WebUrlPattern {
        pattern: UrlPattern,
    }

    impl WebUrlPattern {
        /// Spec "match": `None` is the `null` result. A string input resolves against the base
        /// URL; an init object with a base URL argument throws.
        fn run(
            &self,
            ctx: &mut Ctx,
            input: &Value,
            base: Option<String>,
        ) -> OpResult<Option<(Vec<Value>, UrlPatternResult)>> {
            let mut inputs = Vec::new();
            let target = match read_input(ctx, input)? {
                Input::Init(raw) => {
                    if base.is_some() {
                        return Err(OpError::type_error(
                            "Invalid arguments: a base URL cannot be combined with an init object",
                        ));
                    }
                    inputs.push(init_object(ctx, &raw));
                    match raw.into_init() {
                        Ok(init) => UrlPatternMatchInput::Init(init),
                        Err(_) => return Ok(None),
                    }
                }
                Input::Text(text) => {
                    inputs.push(string(&text));
                    let base_url = match &base {
                        Some(base) => {
                            inputs.push(string(base));
                            match Url::parse(base) {
                                Ok(url) => Some(url),
                                Err(_) => return Ok(None),
                            }
                        }
                        None => None,
                    };
                    match Url::parse_with_base(&text, base_url.as_ref()) {
                        Ok(url) => UrlPatternMatchInput::Url(url),
                        Err(_) => return Ok(None),
                    }
                }
            };
            Ok(self
                .pattern
                .exec(target)
                .ok()
                .flatten()
                .map(|result| (inputs, result)))
        }
    }

    #[methods]
    impl WebUrlPattern {
        /// `new URLPattern(input, baseURL, options)` or `new URLPattern(input, options)`: a
        /// second argument that is `undefined`, `null` or an object is the options, unless a
        /// third argument was passed.
        #[constructor]
        fn new(
            ctx: &mut Ctx,
            #[default(Value::Undefined)] input: Value,
            #[default(Value::Undefined)] second: Value,
            third: Passed<Value>,
        ) -> OpResult<Self> {
            let second_is_base = third.0.is_some()
                || !matches!(second, Value::Undefined | Value::Null | Value::Obj(_));
            let (base, options) = if second_is_base {
                let base = usv_string(ctx, &second).map_err(OpError::thrown)?;
                let options = third.0.unwrap_or(Value::Undefined);
                (Some(base), read_options(ctx, &options)?)
            } else {
                (None, read_options(ctx, &second)?)
            };
            let init = match read_input(ctx, &input)? {
                Input::Init(raw) => {
                    if base.is_some() {
                        return Err(construct_error(
                            "a base URL cannot be combined with an init object",
                        ));
                    }
                    raw.into_init().map_err(construct_error)?
                }
                Input::Text(text) => {
                    let base_url = base
                        .as_deref()
                        .map(Url::parse)
                        .transpose()
                        .map_err(construct_error)?;
                    UrlPatternInit::parse_constructor_string::<urlpattern::regexp::EcmaRegExp>(
                        &text, base_url,
                    )
                    .map_err(construct_error)?
                }
            };
            let pattern = UrlPattern::parse(init, options).map_err(construct_error)?;
            Ok(Self { pattern })
        }

        #[method]
        fn test(
            &self,
            ctx: &mut Ctx,
            #[default(Value::Undefined)] input: Value,
            #[default(OptUsv::default())] base_url: OptUsv,
        ) -> OpResult<bool> {
            Ok(self.run(ctx, &input, base_url.0)?.is_some())
        }

        #[method]
        fn exec(
            &self,
            ctx: &mut Ctx,
            #[default(Value::Undefined)] input: Value,
            #[default(OptUsv::default())] base_url: OptUsv,
        ) -> OpResult<Value> {
            Ok(match self.run(ctx, &input, base_url.0)? {
                Some((inputs, result)) => match_result(ctx, inputs, &result),
                None => Value::Null,
            })
        }

        #[getter]
        fn protocol(&self) -> String {
            self.pattern.protocol().to_owned()
        }

        #[getter]
        fn username(&self) -> String {
            self.pattern.username().to_owned()
        }

        #[getter]
        fn password(&self) -> String {
            self.pattern.password().to_owned()
        }

        #[getter]
        fn hostname(&self) -> String {
            self.pattern.hostname().to_owned()
        }

        #[getter]
        fn port(&self) -> String {
            self.pattern.port().to_owned()
        }

        #[getter]
        fn pathname(&self) -> String {
            self.pattern.pathname().to_owned()
        }

        #[getter]
        fn search(&self) -> String {
            self.pattern.search().to_owned()
        }

        #[getter]
        fn hash(&self) -> String {
            self.pattern.hash().to_owned()
        }

        #[getter]
        fn has_reg_exp_groups(&self) -> bool {
            self.pattern.has_regexp_groups()
        }
    }
}
