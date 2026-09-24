//! JSDoc tag extraction (docs/typed-tier.md §4.5, milestone F3).
//!
//! Supported tags: `@param`/`@arg`/`@argument`, `@returns`/`@return`, `@type`, `@typedef`
//! (with `@property`/`@prop`), `@callback`, `@template`, `@this`, and casts
//! `/** @type {T} */ (expr)`. Types use TypeScript syntax plus the Closure forms tsc accepts
//! (`?T`, `!T`, `T=`, `...T`, `*`, `?`, `function(T): U`, `Array.<T>`, `Object.<K, V>`).
//! Attachment follows tsc: the nearest `/** */` comment preceding the declaration statement.

use super::lexer::Comment;
use super::parser::parse_jsdoc_type;
use super::types::{FnType, ObjectType, Param, Property, Type, TypeParam};

#[derive(Debug, Clone, PartialEq)]
pub struct ParamTag {
    pub name: String,
    pub ty: Type,
    pub optional: bool,
    pub rest: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct JsDoc {
    pub params: Vec<ParamTag>,
    pub returns: Option<Type>,
    /// `@type {T}`.
    pub ty: Option<Type>,
    pub this: Option<Type>,
    pub templates: Vec<TypeParam>,
    /// `@typedef` and `@callback` aliases declared in this comment.
    pub typedefs: Vec<(String, Type)>,
    /// Type texts that did not parse (they make the annotated function not sound).
    pub errors: Vec<String>,
}

impl JsDoc {
    pub fn is_empty(&self) -> bool {
        self.params.is_empty()
            && self.returns.is_none()
            && self.ty.is_none()
            && self.this.is_none()
            && self.templates.is_empty()
            && self.typedefs.is_empty()
    }
}

/// The comment's body lines with the `/**`, `*/` and leading `*` decoration removed.
fn body_lines(text: &str) -> Vec<&str> {
    let inner = text
        .strip_prefix("/**")
        .unwrap_or(text)
        .strip_suffix("*/")
        .unwrap_or(text);
    inner
        .lines()
        .map(|l| {
            let l = l.trim_start();
            l.strip_prefix('*').unwrap_or(l)
        })
        .collect()
}

/// Splits `text` at the brace group starting at its first non-space char: `{T} rest`.
fn braced(text: &str) -> Option<(&str, &str)> {
    let t = text.trim_start();
    if !t.starts_with('{') {
        return None;
    }
    let mut depth = 0;
    for (i, c) in t.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some((&t[1..i], &t[i + 1..]));
                }
            }
            _ => {}
        }
    }
    None
}

fn first_word(text: &str) -> (&str, &str) {
    let t = text.trim_start();
    let end = t.find(|c: char| c.is_whitespace()).unwrap_or(t.len());
    (&t[..end], &t[end..])
}

enum Owner {
    Function,
    Typedef {
        name: String,
        base: Option<Type>,
        props: Vec<Property>,
    },
    Callback {
        name: String,
        params: Vec<Param>,
        ret: Option<Type>,
    },
}

/// Parses the tags of one `/** … */` comment.
pub fn parse_comment(text: &str) -> JsDoc {
    let joined = body_lines(text).join("\n");
    // Tags start at `@` preceded by start or whitespace.
    let mut tags: Vec<(&str, &str)> = Vec::new();
    let bytes = joined.as_bytes();
    let mut starts = Vec::new();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'@' && (i == 0 || bytes[i - 1].is_ascii_whitespace()) {
            starts.push(i);
        }
    }
    for (k, &s) in starts.iter().enumerate() {
        let end = starts.get(k + 1).copied().unwrap_or(joined.len());
        let chunk = &joined[s + 1..end];
        let name_end = chunk
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
            .unwrap_or(chunk.len());
        tags.push((&chunk[..name_end], &chunk[name_end..]));
    }

    let mut doc = JsDoc::default();
    let mut owner = Owner::Function;
    let parse = |doc: &mut JsDoc, text: &str| -> Option<(Type, bool, bool)> {
        match parse_jsdoc_type(text.trim()) {
            Ok(t) => Some(t),
            Err(e) => {
                doc.errors
                    .push(format!("{{{}}}: {}", text.trim(), e.message));
                None
            }
        }
    };
    let finish = |doc: &mut JsDoc, owner: Owner| match owner {
        Owner::Function => {}
        Owner::Typedef { name, base, props } => {
            let ty = if props.is_empty() {
                base.unwrap_or(Type::Any)
            } else {
                Type::Object(ObjectType {
                    props,
                    ..ObjectType::default()
                })
            };
            doc.typedefs.push((name, ty));
        }
        Owner::Callback { name, params, ret } => {
            doc.typedefs.push((
                name,
                Type::Function(Box::new(FnType {
                    type_params: Vec::new(),
                    this: None,
                    params,
                    ret: ret.unwrap_or(Type::Void),
                    predicate: None,
                    construct: false,
                })),
            ));
        }
    };
    for (tag, rest) in tags {
        match tag {
            "param" | "arg" | "argument" | "property" | "prop" => {
                let Some((ty_text, after)) = braced(rest) else {
                    continue;
                };
                let Some((ty, eq_optional, is_rest)) = parse(&mut doc, ty_text) else {
                    continue;
                };
                let (word, _) = first_word(after);
                let (name, bracket_optional) = if let Some(inner) = word.strip_prefix('[') {
                    let inner = inner.split(['=', ']']).next().unwrap_or("");
                    (inner.to_string(), true)
                } else {
                    (word.to_string(), false)
                };
                if name.is_empty() || name.contains('.') {
                    // `@param {T} opts.x` documents a property of a destructured parameter.
                    continue;
                }
                let optional = eq_optional || bracket_optional;
                let is_prop = matches!(tag, "property" | "prop");
                match &mut owner {
                    Owner::Typedef { props, .. } if is_prop => props.push(Property {
                        name,
                        optional,
                        readonly: false,
                        method: false,
                        ty,
                    }),
                    Owner::Callback { params, .. } if !is_prop => params.push(Param {
                        name,
                        ty: if is_rest {
                            Type::Array(Box::new(ty))
                        } else {
                            ty
                        },
                        optional,
                        rest: is_rest,
                    }),
                    Owner::Function if !is_prop => doc.params.push(ParamTag {
                        name,
                        ty,
                        optional,
                        rest: is_rest,
                    }),
                    _ => {}
                }
            }
            "returns" | "return" => {
                let Some((ty_text, _)) = braced(rest) else {
                    continue;
                };
                let Some((ty, _, _)) = parse(&mut doc, ty_text) else {
                    continue;
                };
                match &mut owner {
                    Owner::Callback { ret, .. } => *ret = Some(ty),
                    _ => doc.returns = Some(ty),
                }
            }
            "type" => {
                if let Some((ty_text, _)) = braced(rest) {
                    if let Some((ty, _, _)) = parse(&mut doc, ty_text) {
                        doc.ty = Some(ty);
                    }
                }
            }
            "this" => {
                if let Some((ty_text, _)) = braced(rest) {
                    if let Some((ty, _, _)) = parse(&mut doc, ty_text) {
                        doc.this = Some(ty);
                    }
                }
            }
            "template" => {
                let (constraint, names) = match braced(rest) {
                    Some((t, after)) => (parse(&mut doc, t).map(|p| p.0), after),
                    None => (None, rest),
                };
                let (list, _) = first_word(names);
                let mut list = list.to_string();
                // `@template T, U` (the comma may be followed by a space).
                let mut tail = names.trim_start()[list.len()..].trim_start();
                while list.ends_with(',') {
                    let (w, r) = first_word(tail);
                    if w.is_empty() {
                        break;
                    }
                    list.push_str(w);
                    tail = r.trim_start();
                }
                for n in list.split(',').filter(|n| !n.is_empty()) {
                    doc.templates.push(TypeParam {
                        name: n.to_string(),
                        constraint: constraint.clone(),
                        default: None,
                    });
                }
            }
            "typedef" => {
                let prev = std::mem::replace(&mut owner, Owner::Function);
                finish(&mut doc, prev);
                let (base, after) = match braced(rest) {
                    Some((t, after)) => (parse(&mut doc, t).map(|p| p.0), after),
                    None => (None, rest),
                };
                let (name, _) = first_word(after);
                if !name.is_empty() {
                    owner = Owner::Typedef {
                        name: name.to_string(),
                        base,
                        props: Vec::new(),
                    };
                }
            }
            "callback" => {
                let prev = std::mem::replace(&mut owner, Owner::Function);
                finish(&mut doc, prev);
                let (name, _) = first_word(rest);
                if !name.is_empty() {
                    owner = Owner::Callback {
                        name: name.to_string(),
                        params: Vec::new(),
                        ret: None,
                    };
                }
            }
            _ => {}
        }
    }
    finish(&mut doc, owner);
    doc
}

/// The comment's source text.
pub fn comment_text(src: &str, c: Comment) -> &str {
    &src[c.start as usize..c.end as usize]
}

/// The `@type` of a JSDoc cast comment (`/** @type {T} */ (expr)`).
pub fn cast_type(src: &str, c: Comment) -> Option<Type> {
    let text = comment_text(src, c);
    if !text.contains("@type") {
        return None;
    }
    parse_comment(text).ty
}
