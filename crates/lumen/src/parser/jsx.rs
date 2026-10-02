use super::*;
use std::cell::Cell;
use std::cell::RefCell;

impl JsxOptions {
    pub(super) fn with_pragmas(&self, src: &str) -> Result<Self, ParseError> {
        let mut options = self.clone();
        let mut rest = src.trim_start_matches(['\u{feff}', ' ', '\t', '\r', '\n']);
        if rest.starts_with("#!") {
            rest = rest.split_once('\n').map_or("", |(_, s)| s);
        }
        loop {
            rest = rest.trim_start();
            let (comment, tail) = if let Some(s) = rest.strip_prefix("/*") {
                match s.split_once("*/") {
                    Some(pair) => pair,
                    None => break,
                }
            } else if let Some(s) = rest.strip_prefix("//") {
                s.split_once('\n').unwrap_or((s, ""))
            } else {
                break;
            };
            let mut words = comment.split_whitespace();
            while let Some(word) = words.next() {
                if !matches!(
                    word,
                    "@jsx" | "@jsxFrag" | "@jsxRuntime" | "@jsxImportSource"
                ) {
                    continue;
                }
                let value = words.next().unwrap_or("");
                let bad = || ParseError {
                    message: format!("invalid {word} pragma: {value}"),
                    line: src[..src.len() - rest.len()]
                        .bytes()
                        .filter(|b| *b == b'\n')
                        .count() as u32
                        + 1,
                    at_eof: false,
                };
                match word {
                    "@jsxRuntime" => {
                        options.runtime = match value {
                            "classic" => JsxRuntime::Classic,
                            "automatic" => JsxRuntime::Automatic,
                            _ => return Err(bad()),
                        }
                    }
                    "@jsxImportSource" if !value.is_empty() => options.import_source = value.into(),
                    "@jsx" | "@jsxFrag"
                        if value.split('.').all(|part| {
                            let mut chars = part.chars();
                            chars
                                .next()
                                .is_some_and(|c| c.is_alphabetic() || c == '$' || c == '_')
                                && chars.all(|c| c.is_alphanumeric() || c == '$' || c == '_')
                        }) =>
                    {
                        if word == "@jsx" {
                            options.factory = value.into();
                        } else {
                            options.fragment_factory = value.into();
                        }
                    }
                    _ => return Err(bad()),
                }
            }
            rest = tail;
        }
        Ok(options)
    }
}

pub(super) struct JsxState {
    options: JsxOptions,
    namespace: String,
    fallback: String,
    used: Cell<bool>,
    fallback_used: Cell<bool>,
    templates: RefCell<Vec<(String, String)>>,
    pub(super) elements: RefCell<Vec<Rc<JsxElement>>>,
    printing: RefCell<Option<(Rc<str>, Vec<(u32, u32, String)>)>>,
}

impl JsxState {
    pub(super) fn new(src: &str, options: &JsxOptions) -> Self {
        let mut namespace = "$lumen$jsx".to_string();
        while src.contains(&namespace) {
            namespace.push('$');
        }
        Self {
            fallback: format!("{namespace}$createElement"),
            namespace,
            options: options.clone(),
            used: Cell::new(false),
            fallback_used: Cell::new(false),
            templates: RefCell::new(Vec::new()),
            elements: RefCell::new(Vec::new()),
            printing: RefCell::new(None),
        }
    }

    pub(super) fn imports(&self, body: &mut Vec<Stmt>) {
        for (name, markup) in self.templates.borrow().iter().rev() {
            body.insert(
                0,
                Stmt::VarDecl {
                    kind: DeclKind::Const,
                    decls: vec![(
                        Pattern::Ident(name.clone()),
                        Some(native_call(
                            "template",
                            vec![Expr::Str(Rc::from(markup.as_str()))],
                        )),
                    )],
                },
            );
        }
        if self.used.get() {
            let suffix = if self.options.runtime == JsxRuntime::Development {
                "jsx-dev-runtime"
            } else {
                "jsx-runtime"
            };
            body.insert(
                0,
                Stmt::Import(ImportDecl {
                    source: Rc::from(format!("{}/{suffix}", self.options.import_source)),
                    specs: vec![ImportSpec::Namespace(self.namespace.clone())],
                    attr_type: None,
                }),
            );
        }
        if self.fallback_used.get() {
            body.insert(
                0,
                Stmt::Import(ImportDecl {
                    source: Rc::from(self.options.import_source.as_str()),
                    specs: vec![ImportSpec::Named {
                        imported: "createElement".into(),
                        local: self.fallback.clone(),
                    }],
                    attr_type: None,
                }),
            );
        }
    }

    pub(super) fn requires(&self, body: &mut Vec<Stmt>) {
        let mut imports = Vec::new();
        self.imports(&mut imports);
        let mut decls = Vec::new();
        for import in imports {
            let Stmt::Import(import) = import else {
                if let Stmt::VarDecl {
                    decls: templates, ..
                } = import
                {
                    decls.extend(templates);
                }
                continue;
            };
            let value = Expr::Call {
                callee: Box::new(Expr::Ident("require".into())),
                args: vec![ArrayElem::Item(Expr::Str(import.source))],
                optional: false,
                pos: NO_POS,
            };
            for spec in import.specs {
                match spec {
                    ImportSpec::Namespace(name) => {
                        decls.push((Pattern::Ident(name), Some(value.clone())))
                    }
                    ImportSpec::Named { imported, local } => decls.push((
                        Pattern::Ident(local),
                        Some(Expr::Member {
                            obj: Box::new(value.clone()),
                            prop: imported,
                            optional: false,
                        }),
                    )),
                    _ => unreachable!(),
                }
            }
        }
        if !decls.is_empty() {
            body.insert(
                0,
                Stmt::VarDecl {
                    kind: DeclKind::Const,
                    decls,
                },
            );
        }
    }

    fn helper(&self, name: &str) -> Expr {
        self.used.set(true);
        Expr::Member {
            obj: Box::new(Expr::Ident(self.namespace.clone())),
            prop: name.into(),
            optional: false,
        }
    }
}

fn path(name: &str) -> Expr {
    let mut parts = name.split('.');
    let mut expr = Expr::Ident(parts.next().unwrap_or_default().into());
    for prop in parts {
        expr = Expr::Member {
            obj: Box::new(expr),
            prop: prop.into(),
            optional: false,
        };
    }
    expr
}

fn property(name: &str, value: Expr) -> PropDef {
    // A computed key makes __proto__ an ordinary own property.
    PropDef::KeyValue {
        key: PropKey::Computed(Expr::Str(Rc::from(name))),
        value,
    }
}

fn native_call(name: &str, args: Vec<Expr>) -> Expr {
    Expr::Call {
        callee: Box::new(path(&format!("__lumen.{name}"))),
        args: args.into_iter().map(ArrayElem::Item).collect(),
        optional: false,
        pos: NO_POS,
    }
}

fn jsx_arrow(body: Vec<Stmt>, strict: bool) -> Expr {
    Expr::Func(Rc::new(Function {
        name: None,
        params: Vec::new(),
        body: RefCell::new(Some(Rc::new(body))),
        lazy: RefCell::new(None),
        body_used: Cell::new(false),
        lazy_error: std::cell::OnceCell::new(),
        is_arrow: true,
        is_strict: strict,
        expr_body: false,
        is_generator: false,
        is_async: false,
        is_method: false,
        is_fn_expr: true,
        source: FnSource::None,
        scan: Cell::new(0),
        hoist: RefCell::new(None),
        calls: Cell::new(0),
        code: std::cell::OnceCell::new(),
        fn_maps: std::cell::OnceCell::new(),
    }))
}

enum SlotTarget {
    Text,
    Child,
    Attribute(String),
}

fn slot_markup(
    element: &JsxElement,
    path: Vec<usize>,
    slots: &mut Vec<(Vec<usize>, SlotTarget, SubToks)>,
) -> Option<String> {
    for tokens in element
        .attributes
        .iter()
        .filter_map(|attribute| match attribute {
            JsxAttribute::Named(_, Some(JsxChild::Expression(tokens))) => Some(tokens),
            _ => None,
        })
        .chain(element.children.iter().filter_map(|child| match child {
            JsxChild::Expression(tokens) => Some(tokens),
            _ => None,
        }))
    {
        if tokens.0.iter_from(0).any(|token| matches!(&token.kind, Tok::Ident(name) if matches!(name.as_str(), "await" | "yield"))) {
            return None;
        }
    }
    if !element.children.is_empty()
        && element.name.as_deref().is_some_and(|name| {
            matches!(
                name,
                "area"
                    | "base"
                    | "br"
                    | "col"
                    | "embed"
                    | "hr"
                    | "img"
                    | "input"
                    | "link"
                    | "meta"
                    | "param"
                    | "source"
                    | "track"
                    | "wbr"
            )
        })
    {
        return None;
    }
    let mut static_element = element.clone();
    static_element.children.clear();
    for attribute in &mut static_element.attributes {
        if let JsxAttribute::Named(name, Some(JsxChild::Expression(tokens))) = attribute {
            if name == "key" || name == "ref" || name.starts_with("on") {
                return None;
            }
            slots.push((
                path.clone(),
                SlotTarget::Attribute(name.clone()),
                tokens.clone(),
            ));
            *attribute = JsxAttribute::Named(name.clone(), Some(JsxChild::Text(String::new())));
        }
    }
    let mut markup = static_markup(&static_element)?;
    let close = markup.rfind("</").unwrap_or(markup.len());
    let mut children = String::new();
    let mut index = 0;
    for child in &element.children {
        let mut child_path = path.clone();
        child_path.push(index);
        match child {
            JsxChild::Element(child) => {
                children.push_str(&slot_markup(child, child_path, slots)?);
                index += 1;
            }
            JsxChild::Text(text) => {
                if let Some(Expr::Str(text)) = text_value(text) {
                    children.push_str(&text.replace('&', "&amp;").replace('<', "&lt;"));
                    index += 1;
                }
            }
            JsxChild::Expression(tokens)
                if element.children.len() == 1
                    && tokens.0.len() == 2
                    && matches!(
                        tokens.0.get(0).map(|t| &t.kind),
                        Some(Tok::Num(_) | Tok::Str(_))
                    ) =>
            {
                children.push(' ');
                slots.push((child_path, SlotTarget::Text, tokens.clone()));
                index += 1;
            }
            JsxChild::Expression(tokens)
                if matches!(tokens.0.get(0).map(|t| &t.kind), Some(Tok::Eof)) => {}
            JsxChild::Expression(tokens) => {
                children.push_str("<!--lumen-->");
                slots.push((child_path, SlotTarget::Child, tokens.clone()));
                index += 1;
            }
            _ => return None,
        }
    }
    markup.insert_str(close, &children);
    Some(markup)
}

fn static_markup(element: &JsxElement) -> Option<String> {
    let name = element.name.as_deref()?;
    if !name.starts_with(|c: char| c.is_ascii_lowercase()) || name.contains(['.', ':']) {
        return None;
    }
    // HTML parsing changes these trees; keep them on the dynamic element path.
    if matches!(
        name,
        "table"
            | "thead"
            | "tbody"
            | "tfoot"
            | "tr"
            | "td"
            | "th"
            | "select"
            | "option"
            | "script"
            | "style"
            | "textarea"
            | "svg"
            | "math"
    ) {
        return None;
    }
    let escape = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('"', "&quot;")
    };
    let mut markup = format!("<{name}");
    for attribute in &element.attributes {
        let JsxAttribute::Named(key, value) = attribute else {
            return None;
        };
        if key == "key" || key == "ref" || key.starts_with("on") {
            return None;
        }
        let key = match key.as_str() {
            "className" => "class",
            "htmlFor" => "for",
            key => key,
        };
        markup.push(' ');
        markup.push_str(key);
        if let Some(value) = value {
            let JsxChild::Text(value) = value else {
                return None;
            };
            markup.push_str("=\"");
            markup.push_str(&escape(value));
            markup.push('"');
        }
    }
    markup.push('>');
    for child in &element.children {
        match child {
            JsxChild::Text(text) => {
                if let Some(Expr::Str(text)) = text_value(text) {
                    markup.push_str(&escape(&text));
                }
            }
            JsxChild::Element(child) => markup.push_str(&static_markup(child)?),
            _ => return None,
        }
    }
    if !matches!(
        name,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    ) {
        markup.push_str(&format!("</{name}>"));
    } else if !element.children.is_empty() {
        return None;
    }
    Some(markup)
}

/// Babel/TypeScript preserve spaces on a single line and trim indentation around newlines.
fn text_value(text: &str) -> Option<Expr> {
    let lines: Vec<String> = text
        .split(['\n', '\r'])
        .map(|s| s.replace('\t', " "))
        .collect();
    let last_nonempty = lines
        .iter()
        .rposition(|s| !s.trim_matches(' ').is_empty())
        .unwrap_or(0);
    let mut value = String::new();
    for (i, line) in lines.iter().enumerate() {
        let line = if i != 0 {
            line.trim_start_matches(' ')
        } else {
            line
        };
        let line = if i + 1 != lines.len() {
            line.trim_end_matches(' ')
        } else {
            line
        };
        if !line.is_empty() {
            value.push_str(line);
            if i != last_nonempty {
                value.push(' ');
            }
        }
    }
    (!value.is_empty()).then(|| Expr::Str(Rc::from(value)))
}

impl Parser {
    pub(super) fn print_jsx(&mut self, source: &str) -> Result<String, ParseError> {
        let state = self.jsx.clone().expect("JSX parser context");
        let mut elements = state.elements.borrow().clone();
        elements.sort_by_key(|element| element.end - element.start);
        *state.printing.borrow_mut() = Some((Rc::from(source), Vec::new()));
        for element in elements {
            let expr = self.build_jsx(&element)?;
            let text = print_expr(&expr);
            state.printing.borrow_mut().as_mut().unwrap().1.push((
                element.start,
                element.end,
                text,
            ));
        }
        let (_, edits) = state.printing.borrow_mut().take().unwrap();
        let mut out = String::new();
        let start = if source.starts_with("#!") {
            let end = source.find('\n').map_or(source.len(), |at| at + 1);
            out.push_str(&source[..end]);
            end as u32
        } else {
            0
        };
        let mut imports = Vec::new();
        state.imports(&mut imports);
        for import in imports {
            let Stmt::Import(import) = import else {
                if let Stmt::VarDecl { decls, .. } = import {
                    for (pattern, value) in decls {
                        let Pattern::Ident(name) = pattern else {
                            unreachable!()
                        };
                        out.push_str(&format!(
                            "const {name} = {};\n",
                            print_expr(&value.unwrap())
                        ));
                    }
                }
                continue;
            };
            for spec in import.specs {
                match spec {
                    ImportSpec::Namespace(name) => out.push_str(&format!(
                        "import * as {name} from {};\n",
                        lumen_common::json::json_string(&import.source)
                    )),
                    ImportSpec::Named { imported, local } => out.push_str(&format!(
                        "import {{ {imported} as {local} }} from {};\n",
                        lumen_common::json::json_string(&import.source)
                    )),
                    _ => unreachable!(),
                }
            }
        }
        out.push_str(&replace_range(source, start, source.len() as u32, &edits));
        Ok(out)
    }

    fn jsx_expression(&mut self, tokens: &SubToks, spread: bool) -> Result<Expr, ParseError> {
        if let Some(state) = &self.jsx {
            if let Some((source, edits)) = state.printing.borrow().as_ref() {
                let start = tokens
                    .0
                    .get(usize::from(spread))
                    .expect("expression token")
                    .start;
                let end = tokens
                    .0
                    .get(tokens.0.len() - 1)
                    .expect("expression EOF")
                    .start;
                return Ok(Expr::Ident(format!(
                    "({})",
                    replace_range(source, start, end, edits)
                )));
            }
        }
        if spread {
            let mut inner = TokVec::with_capacity(tokens.0.len());
            inner.extend(tokens.0.iter_from(1).cloned());
            self.parse_template_sub(&SubToks(Rc::new(inner)))
        } else {
            self.parse_template_sub(tokens)
        }
    }

    fn jsx_value(&mut self, child: &JsxChild, attribute: bool) -> Result<Option<Expr>, ParseError> {
        Ok(match child {
            JsxChild::Text(text) if attribute => Some(Expr::Str(Rc::from(text.as_str()))),
            JsxChild::Text(text) => text_value(text),
            JsxChild::Element(element) => Some(self.build_jsx(element)?),
            JsxChild::Expression(tokens)
                if matches!(tokens.0.get(0).map(|t| &t.kind), Some(Tok::Eof)) =>
            {
                None
            }
            JsxChild::Expression(tokens) => Some(self.jsx_expression(tokens, false)?),
            JsxChild::Spread(tokens) => Some(self.jsx_expression(tokens, true)?),
        })
    }

    pub(super) fn build_jsx(&mut self, element: &JsxElement) -> Result<Expr, ParseError> {
        self.nested(|p| p.build_jsx_inner(element))
    }

    fn build_jsx_inner(&mut self, element: &JsxElement) -> Result<Expr, ParseError> {
        let state = self.jsx.clone().expect("JSX parser context");
        let options = &state.options;
        if options.runtime == JsxRuntime::Preserve {
            return self.err("JSX preserve mode cannot be evaluated");
        }
        if options.import_source == "lumen" && options.runtime == JsxRuntime::Automatic {
            let mut slots = Vec::new();
            if let Some(markup) = slot_markup(element, Vec::new(), &mut slots) {
                let mut templates = state.templates.borrow_mut();
                let name =
                    if let Some((name, _)) = templates.iter().find(|(_, text)| *text == markup) {
                        name.clone()
                    } else {
                        let name = format!("{}$template{}", state.namespace, templates.len());
                        templates.push((name.clone(), markup));
                        name
                    };
                let instantiate = native_call("instantiate", vec![Expr::Ident(name)]);
                if slots.is_empty() {
                    return Ok(instantiate);
                }
                drop(templates);
                let root = format!("{}$root", state.namespace);
                let mut body = vec![Stmt::VarDecl {
                    kind: DeclKind::Const,
                    decls: vec![(Pattern::Ident(root.clone()), Some(instantiate))],
                }];
                let mut bindings = Vec::new();
                for (slot_index, (path, target, tokens)) in slots.into_iter().enumerate() {
                    let value = self.jsx_expression(&tokens, false)?;
                    let callback = jsx_arrow(vec![Stmt::Return(Some(value))], self.strict);
                    let node = native_call(
                        "nodeAt",
                        vec![
                            Expr::Ident(root.clone()),
                            Expr::Array(
                                path.into_iter()
                                    .map(|i| ArrayElem::Item(Expr::Num(i as f64)))
                                    .collect(),
                            ),
                        ],
                    );
                    let slot_name = format!("{}$slot{slot_index}", state.namespace);
                    body.push(Stmt::VarDecl {
                        kind: DeclKind::Const,
                        decls: vec![(Pattern::Ident(slot_name.clone()), Some(node))],
                    });
                    let node = Expr::Ident(slot_name);
                    let call = match target {
                        SlotTarget::Attribute(attribute) => {
                            let attribute = match attribute.as_str() {
                                "className" => "class",
                                "htmlFor" => "for",
                                name => name,
                            };
                            native_call(
                                "bindAttribute",
                                vec![node, Expr::Str(Rc::from(attribute)), callback],
                            )
                        }
                        SlotTarget::Text => native_call("bindText", vec![node, callback]),
                        SlotTarget::Child => native_call("bindChild", vec![node, callback]),
                    };
                    bindings.push(Stmt::Expr(call));
                }
                body.extend(bindings);
                body.push(Stmt::Return(Some(Expr::Ident(root))));
                return Ok(Expr::Call {
                    callee: Box::new(jsx_arrow(body, self.strict)),
                    args: Vec::new(),
                    optional: false,
                    pos: NO_POS,
                });
            }
        }
        let classic = options.runtime == JsxRuntime::Classic;
        let tag = match element.name.as_deref() {
            None if classic => path(&options.fragment_factory),
            None => state.helper("Fragment"),
            Some(name)
                if !name.contains('.')
                    && (name.starts_with(|c: char| c.is_ascii_lowercase())
                        || name.contains('-')
                        || name.contains(':')) =>
            {
                Expr::Str(Rc::from(name))
            }
            Some(name) => path(name),
        };
        let mut spread_seen = false;
        let fallback = !classic
            && element.attributes.iter().any(|attribute| match attribute {
                JsxAttribute::Spread(_) => {
                    spread_seen = true;
                    false
                }
                JsxAttribute::Named(name, _) => name == "key" && spread_seen,
            });
        let mut props = Vec::new();
        let mut key = Expr::Undefined;
        for attribute in &element.attributes {
            match attribute {
                JsxAttribute::Named(name, value) => {
                    let value = match value {
                        Some(value) => self.jsx_value(value, true)?.unwrap_or(Expr::Undefined),
                        None => Expr::Bool(true),
                    };
                    if name == "key" && !classic && !fallback {
                        key = value;
                    } else {
                        props.push(property(name, value));
                    }
                }
                JsxAttribute::Spread(tokens) => {
                    props.push(PropDef::Spread(self.jsx_expression(tokens, true)?))
                }
            }
        }
        let mut children = Vec::new();
        for child in &element.children {
            if let Some(value) = self.jsx_value(child, false)? {
                children.push(if matches!(child, JsxChild::Spread(_)) {
                    ArrayElem::Spread(value)
                } else {
                    ArrayElem::Item(value)
                });
            }
        }
        let static_children = children.len() > 1;
        let mut args = vec![ArrayElem::Item(tag)];
        let callee = if classic || fallback {
            args.push(ArrayElem::Item(if props.is_empty() {
                Expr::Null
            } else {
                Expr::Object(props)
            }));
            args.extend(children);
            if classic {
                path(&options.factory)
            } else {
                state.fallback_used.set(true);
                Expr::Ident(state.fallback.clone())
            }
        } else {
            match children.len() {
                0 => {}
                1 if matches!(children.first(), Some(ArrayElem::Item(_))) => {
                    if let ArrayElem::Item(value) = children.remove(0) {
                        props.push(property("children", value));
                    }
                }
                _ => props.push(property("children", Expr::Array(children))),
            }
            args.extend([ArrayElem::Item(Expr::Object(props)), ArrayElem::Item(key)]);
            if options.runtime == JsxRuntime::Development {
                let start = self
                    .src
                    .byte_range(element.start, element.start)
                    .map_or(element.start, |(s, _)| s) as usize;
                let source = &self.src.src[..start.min(self.src.src.len())];
                let column = source
                    .rsplit('\n')
                    .next()
                    .unwrap_or("")
                    .encode_utf16()
                    .count()
                    + 1;
                args.extend([
                    ArrayElem::Item(Expr::Bool(static_children)),
                    ArrayElem::Item(Expr::Object(vec![
                        property("fileName", Expr::Str(Rc::from(options.filename.as_str()))),
                        property("lineNumber", Expr::Num(element.line as f64)),
                        property("columnNumber", Expr::Num(column as f64)),
                    ])),
                    ArrayElem::Item(Expr::This),
                ]);
                state.helper("jsxDEV")
            } else {
                state.helper(if static_children { "jsxs" } else { "jsx" })
            }
        };
        Ok(Expr::Call {
            callee: Box::new(callee),
            args,
            optional: false,
            pos: self
                .src
                .byte_range(element.start, element.start)
                .map_or(element.start, |(s, _)| s),
        })
    }
}

fn replace_range(source: &str, start: u32, end: u32, edits: &[(u32, u32, String)]) -> String {
    let mut edits: Vec<_> = edits
        .iter()
        .filter(|edit| edit.0 >= start && edit.1 <= end)
        .collect();
    edits.sort_by_key(|edit| (edit.0, std::cmp::Reverse(edit.1)));
    let mut out = String::new();
    let mut at = start as usize;
    for (begin, end, text) in edits {
        if (*begin as usize) < at {
            continue;
        }
        out.push_str(&source[at..*begin as usize]);
        out.push_str(text);
        at = *end as usize;
    }
    out.push_str(&source[at..end as usize]);
    out
}

/// Only prints the expression forms JSX lowering emits; expression containers retain source.
fn print_expr(expr: &Expr) -> String {
    match expr {
        Expr::Ident(name) => name.clone(),
        Expr::Str(value) => lumen_common::json::quote(
            value,
            &lumen_common::json::Quote {
                spelling: lumen_common::json::Spelling::Utf16,
                lone_surrogates: true,
                ..lumen_common::json::Quote::JS_SOURCE
            },
        ),
        Expr::Num(value) => value.to_string(),
        Expr::Bool(value) => value.to_string(),
        Expr::Null => "null".into(),
        Expr::Undefined => "void 0".into(),
        Expr::This => "this".into(),
        Expr::Member { obj, prop, .. } => format!("{}.{prop}", print_expr(obj)),
        Expr::Call { callee, args, .. } => format!("{}({})", print_expr(callee), print_items(args)),
        Expr::Array(items) => format!("[{}]", print_items(items)),
        Expr::Object(props) => format!(
            "{{{}}}",
            props
                .iter()
                .map(|prop| match prop {
                    PropDef::KeyValue {
                        key: PropKey::Computed(key),
                        value,
                    } => format!("[{}]: {}", print_expr(key), print_expr(value)),
                    PropDef::Spread(value) => format!("...{}", print_expr(value)),
                    _ => unreachable!("JSX property form"),
                })
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Expr::Func(function) => format!(
            "(() => {{ {} }})",
            function
                .body()
                .iter()
                .map(|statement| match statement {
                    Stmt::Return(Some(value)) => format!("return {};", print_expr(value)),
                    Stmt::Expr(value) => format!("{};", print_expr(value)),
                    Stmt::VarDecl { decls, .. } => decls
                        .iter()
                        .map(|(name, value)| {
                            let Pattern::Ident(name) = name else {
                                unreachable!()
                            };
                            format!("const {name} = {};", print_expr(value.as_ref().unwrap()))
                        })
                        .collect::<Vec<_>>()
                        .join(" "),
                    _ => unreachable!("JSX closure statement"),
                })
                .collect::<Vec<_>>()
                .join(" ")
        ),
        _ => unreachable!("JSX lowering expression form"),
    }
}

fn print_items(items: &[ArrayElem]) -> String {
    items
        .iter()
        .map(|item| match item {
            ArrayElem::Item(value) => print_expr(value),
            ArrayElem::Spread(value) => format!("...{}", print_expr(value)),
            ArrayElem::Hole => String::new(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Completion, Engine};

    const RUNTIME: &str = "export const Fragment='fragment'; export function jsx(type, props, key) { return {type, props, key}; } export const jsxs=jsx; export function jsxDEV(type,props,key,staticChildren,source,self) { return {type,props,key,staticChildren,source}; } export function createElement(type,props,...children) { return {type,props,children,mode:'classic'}; }";

    #[test]
    fn native_lumen_static_templates_are_hoisted_and_reused() {
        let source = "/** @jsxImportSource lumen */ const a=<div className='a'><span>Hello &amp;</span><img src='x.png'/></div>; const b=<div className='a'><span>Hello &amp;</span><img src='x.png'/></div>;";
        let printed = transpile_jsx(source, false, &JsxOptions::default()).unwrap();
        assert_eq!(printed.matches("__lumen.template(").count(), 1, "{printed}");
        assert_eq!(
            printed.matches("__lumen.instantiate(").count(),
            2,
            "{printed}"
        );
        assert!(
            printed.contains(
                "<div class=\\\"a\\\"><span>Hello &amp;</span><img src=\\\"x.png\\\"></div>"
            ),
            "{printed}"
        );
        let mut engine = Engine::new();
        engine
            .eval(
                "globalThis.__lumen={template:s=>s, instantiate:s=>s};",
                false,
            )
            .unwrap();
        let result = engine
            .eval_module_jsx(source, "app.tsx", true, |_, _| None)
            .unwrap();
        assert!(!matches!(result, Completion::Throw { .. }));
    }

    #[test]
    fn native_lumen_slots_capture_lexical_values() {
        let source = "/** @jsxImportSource lumen */ const count=()=>7; globalThis.result=<div title={count()}><span>{count()}</span></div>;";
        let printed = transpile_jsx(source, true, &JsxOptions::default()).unwrap();
        assert!(printed.contains("__lumen.bindAttribute("), "{printed}");
        assert!(printed.contains("__lumen.bindChild("), "{printed}");
        let mut engine = Engine::new();
        engine.eval("globalThis.slots=[]; globalThis.__lumen={template:s=>s, instantiate:s=>({markup:s}), nodeAt:(root,path)=>path, bindAttribute:(node,name,fn)=>slots.push([node,name,fn()]), bindChild:(node,fn)=>slots.push([node,fn()])};", false).unwrap();
        let result = engine
            .eval_module_jsx(source, "app.tsx", true, |_, _| None)
            .unwrap();
        assert!(!matches!(result, Completion::Throw { .. }));
        match engine.eval("JSON.stringify(slots)", false).unwrap() {
            Completion::Value(value) => assert_eq!(value, "[[[],\"title\",7],[[0,0],7]]"),
            Completion::Throw { message, .. } => panic!("{message}"),
        }
    }

    #[test]
    fn native_jsx_numeric_entities_preserve_surrogates_and_validate_range() {
        let result = evaluate(
            "globalThis.result=<div title='&#xD800;' />;",
            false,
            JsxOptions::default(),
        );
        assert!(result.contains("\\ud800"), "{result}");
        let printed = transpile_jsx(
            "const node=<div title='&#xD800;' />;",
            false,
            &JsxOptions::default(),
        )
        .unwrap();
        assert!(printed.contains("\\ud800"), "{printed}");
        assert!(parse_module_jsx(
            "const node=<div>&#x110000;</div>;",
            false,
            &JsxOptions::default()
        )
        .is_err());
    }

    #[test]
    fn native_lumen_mixed_child_slots_capture_nodes_before_binding() {
        let source = "/** @jsxImportSource lumen */ const value=2; globalThis.result=<div>before {value}<span>{value}</span>{[value,3]}</div>;";
        let printed = transpile_jsx(source, false, &JsxOptions::default()).unwrap();
        assert_eq!(
            printed.matches("__lumen.bindChild(").count(),
            3,
            "{printed}"
        );
        assert!(
            printed.rfind("__lumen.nodeAt(").unwrap() < printed.find("__lumen.bindChild(").unwrap(),
            "{printed}"
        );
        let mut engine = Engine::new();
        engine.eval("globalThis.slots=[]; globalThis.__lumen={template:s=>s, instantiate:s=>s, nodeAt:(root,path)=>path, bindChild:(node,fn)=>slots.push([node,fn()])};", false).unwrap();
        let result = engine
            .eval_module_jsx(source, "mixed.jsx", false, |_, _| None)
            .unwrap();
        assert!(!matches!(result, Completion::Throw { .. }));
        match engine.eval("JSON.stringify(slots)", false).unwrap() {
            Completion::Value(value) => assert_eq!(value, "[[[1],2],[[2,0],2],[[3],[2,3]]]"),
            Completion::Throw { message, .. } => panic!("{message}"),
        }
    }

    fn evaluate(src: &str, ts: bool, options: JsxOptions) -> String {
        let mut engine = Engine::new();
        engine.set_jsx_options(options);
        let result = engine
            .eval_module_jsx(src, "app", ts, |specifier, _| {
                Some((specifier.into(), RUNTIME.into()))
            })
            .unwrap();
        if let Completion::Throw { message, .. } = result {
            panic!("{message}");
        }
        match engine
            .eval("JSON.stringify(globalThis.result)", false)
            .unwrap()
        {
            Completion::Value(value) => value,
            Completion::Throw { message, .. } => panic!("{message}"),
        }
    }

    #[test]
    fn native_jsx_automatic_elements_fragments_entities_and_text() {
        let result = evaluate("const UI={Button:'button'}; globalThis.result=<><div data-x='a\\b' title='&copy; &Omega;' flag>\n hello\n world {1+2}<UI.Button />{ /* empty */ }</div></>;", false, JsxOptions::default());
        // The component is resolved as a member expression, never a tag string.
        assert!(result.contains("fragment"), "{result}");
        assert!(result.contains("hello world "), "{result}");
        assert!(result.contains("© Ω"), "{result}");
        assert!(result.contains("button"), "{result}");
    }

    #[test]
    fn native_jsx_key_spread_fallback_and_significant_spaces() {
        let result = evaluate("const props={key:'spread',a:1}; globalThis.result=[<a key='early' {...props}/>, <a {...props} key='late'/>, <a> <b/> </a>];", false, JsxOptions::default());
        assert_eq!(
            result,
            r#"[{"type":"a","props":{"key":"spread","a":1},"key":"early"},{"type":"a","props":{"key":"late","a":1},"children":[],"mode":"classic"},{"type":"a","props":{"children":[" ",{"type":"b","props":{}}," "]}}]"#
        );
    }

    #[test]
    fn native_jsx_babel_evaluated_fixtures() {
        use lumen_common::json::{parse, Value};
        let corpus = parse(include_str!("../../tests/fixtures/jsx/babel.json")).unwrap();
        let Value::Arr(fixtures) = corpus.get("fixtures").unwrap() else {
            panic!("fixtures array");
        };
        for fixture in fixtures {
            let text = |name| fixture.get(name).unwrap().as_str().unwrap();
            let ts = matches!(fixture.get("typescript"), Some(Value::Bool(true)));
            let options = JsxOptions {
                runtime: if text("runtime") == "classic" {
                    JsxRuntime::Classic
                } else {
                    JsxRuntime::Automatic
                },
                import_source: "fixture".into(),
                ..Default::default()
            };
            assert_eq!(
                evaluate(text("source"), ts, options),
                text("expected"),
                "{}",
                text("name")
            );
        }
    }

    #[test]
    fn native_tsx_generic_arrows_and_annotations() {
        let result = evaluate("const id = <T,>(x: T): T => x; const constrained = <T extends number>(x: T): T => x; const count: number = 3; globalThis.result=<div n={id<number>(count)}>{constrained(4)}</div>;", true, JsxOptions::default());
        assert_eq!(result, r#"{"type":"div","props":{"n":3,"children":4}}"#);
    }

    #[test]
    fn native_jsx_classic_spread_and_pragma() {
        let options = JsxOptions {
            runtime: JsxRuntime::Classic,
            ..Default::default()
        };
        let result = evaluate("/** @jsx make @jsxFrag Frag */ function make(type,props,...children) { return {type,props,children}; } const Frag='F'; const values=[1,2]; globalThis.result=<><x {...{a:3}}>{...values}</x></>;", false, options);
        assert_eq!(
            result,
            r#"{"type":"F","props":null,"children":[{"type":"x","props":{"a":3},"children":[1,2]}]}"#
        );
    }

    #[test]
    fn native_jsx_dev_source_and_invalid_markup() {
        let options = JsxOptions {
            runtime: JsxRuntime::Development,
            filename: "source.tsx".into(),
            ..Default::default()
        };
        let result = evaluate("\nglobalThis.result = <a>{1}{2}</a>;", true, options);
        assert!(
            result.contains(r#""lineNumber":2,"columnNumber":21"#),
            "{result}"
        );
        for src in [
            "const x=<a></b>;",
            "const x=<a p={}/>;",
            "const x=<a>{x;}</a>;",
            "const x=<T>value;",
            "const x=<a>;",
            "const x=<a>{...}</a>;",
        ] {
            assert!(
                parse_module_jsx(src, true, &JsxOptions::default()).is_err(),
                "accepted {src}"
            );
        }
        assert!(parse_module_jsx(
            "const x=<a/>;",
            false,
            &JsxOptions {
                runtime: JsxRuntime::Preserve,
                ..Default::default()
            }
        )
        .is_err());
    }

    #[test]
    fn native_jsx_keeps_js_less_than_and_regex_containers() {
        let result = evaluate("const small=1<2; globalThis.result=<a yes={small}>{/x/.test('x') ? <b/> : null}{`x${1}`}</a>;", false, JsxOptions::default());
        assert_eq!(
            result,
            r#"{"type":"a","props":{"yes":true,"children":[{"type":"b","props":{}},"x1"]}}"#
        );
    }

    #[test]
    fn native_tsx_precompiled_matches_interpreter() {
        use crate::precompiled::{CompileOptions, CompiledUnit, PrecompileBundle, SourceKind};
        let source = "const id=<T,>(x:T):T=>x; function component(n:number) { return <div n={id(n)}>{n+1}</div>; } globalThis.result=component(4);";
        let expected = evaluate(source, true, JsxOptions::default());
        let unit = CompiledUnit::compile_with_jsx_options(
            source,
            SourceKind::Module,
            CompileOptions::default(),
            Some((true, &JsxOptions::default())),
        )
        .unwrap();
        let runtime = CompiledUnit::compile(RUNTIME, SourceKind::Module).unwrap();
        let mut bundle = PrecompileBundle::new();
        let entry = bundle.add_compiled("app.tsx", unit).unwrap();
        let runtime = bundle.add_compiled("runtime.js", runtime).unwrap();
        bundle.link(entry, "react/jsx-runtime", runtime).unwrap();
        bundle.set_entry("app.tsx").unwrap();
        let mut engine = Engine::new();
        if let Completion::Throw { message, .. } = engine
            .load_precompiled_owned(bundle.finish().into())
            .unwrap()
        {
            panic!("{message}");
        }
        match engine
            .eval("JSON.stringify(globalThis.result)", false)
            .unwrap()
        {
            Completion::Value(value) => assert_eq!(value, expected),
            Completion::Throw { message, .. } => panic!("{message}"),
        }
    }

    #[test]
    fn native_tsx_ast_printing_preserves_expressions_and_nested_jsx() {
        let source = "const id=<T,>(x:T):T=>x; function view(n:number) { return <div n={id<number>(n)}>{(() => <span>{n+1}</span>)()}</div>; } globalThis.result=view(4);";
        let options = JsxOptions {
            runtime: JsxRuntime::Classic,
            ..Default::default()
        };
        let printed = transpile_jsx(source, true, &options).unwrap();
        assert!(!printed.contains("<T,"), "{printed}");
        assert!(!printed.contains(":number"), "{printed}");
        assert!(!printed.contains("<span>"), "{printed}");
        let mut engine = Engine::new();
        engine.eval("globalThis.React={createElement(type,props,...children){ return {type,props,children}; }};", false).unwrap();
        if let Completion::Throw { message, .. } = engine.eval(&printed, false).unwrap() {
            panic!("{message}\n{printed}");
        }
        match engine.eval("JSON.stringify(result)", false).unwrap() {
            Completion::Value(value) => assert_eq!(
                value,
                r#"{"type":"div","props":{"n":4},"children":[{"type":"span","props":null,"children":[5]}]}"#
            ),
            Completion::Throw { message, .. } => panic!("{message}"),
        }
    }
}
