//! DTD tables and the grammar of the markup declarations.

use super::chars::*;
use super::{err, model, Model, ModelNode};
use std::collections::HashMap;
use std::rc::Rc;

#[derive(Clone, Debug)]
pub struct Entity {
    pub name: String,
    pub text: Option<Rc<str>>,
    pub base: Option<String>,
    pub sysid: Option<String>,
    pub pubid: Option<String>,
    pub notation: Option<String>,
    pub open: bool,
}

#[derive(Clone, Debug)]
pub struct DefAtt {
    pub name: String,
    pub is_cdata: bool,
    /// The normalized default (or `#FIXED`) value.
    pub value: Option<String>,
}

#[derive(Default)]
pub struct Dtd {
    pub general: HashMap<String, Entity>,
    pub param: HashMap<String, Entity>,
    pub attlists: HashMap<String, Vec<DefAtt>>,
    pub standalone: bool,
    pub has_param_entity_refs: bool,
    pub keep_processing: bool,
    pub param_entity_read: bool,
}

impl Dtd {
    pub fn new() -> Dtd {
        Dtd { keep_processing: true, ..Dtd::default() }
    }
}

pub enum AttDefault {
    Required,
    Implied,
    Fixed(String),
    Value(String),
}

pub struct AttDecl {
    pub name: String,
    pub atype: String,
    pub default: AttDefault,
}

pub enum EntityDef {
    Internal(String),
    External { pubid: Option<String>, sysid: String, notation: Option<String> },
}

pub enum Decl {
    Element { name: String, model: Model },
    Attlist { element: String, atts: Vec<AttDecl> },
    Entity { param: bool, name: String, def: EntityDef },
    Notation { name: String, pubid: Option<String>, sysid: Option<String> },
}

#[derive(Debug, PartialEq)]
enum T<'a> {
    Word(&'a str),
    Hash(&'a str),
    Lit(&'a str),
    Open,
    Close,
    Bar,
    Comma,
    Quest,
    Star,
    Plus,
    Pct,
}

struct Lexer<'a> {
    s: &'a str,
    i: usize,
}

impl<'a> Lexer<'a> {
    fn next(&mut self) -> Result<Option<(T<'a>, usize, usize)>, u32> {
        let b = self.s.as_bytes();
        while self.i < b.len() && matches!(b[self.i], b' ' | b'\t' | b'\r' | b'\n') {
            self.i += 1;
        }
        if self.i >= b.len() {
            return Ok(None);
        }
        let start = self.i;
        let c = self.s[self.i..].chars().next().unwrap_or(' ');
        let tok = match c {
            '(' => (T::Open, 1),
            ')' => (T::Close, 1),
            '|' => (T::Bar, 1),
            ',' => (T::Comma, 1),
            '?' => (T::Quest, 1),
            '*' => (T::Star, 1),
            '+' => (T::Plus, 1),
            '%' => (T::Pct, 1),
            '"' | '\'' => match self.s[start + 1..].find(c) {
                Some(n) => (T::Lit(&self.s[start + 1..start + 1 + n]), n + 2),
                None => return Err(err::SYNTAX),
            },
            '#' => {
                let mut e = start + 1;
                for ch in self.s[e..].chars() {
                    if !is_name_char(ch) {
                        break;
                    }
                    e += ch.len_utf8();
                }
                if e == start + 1 {
                    return Err(err::SYNTAX);
                }
                (T::Hash(&self.s[start + 1..e]), e - start)
            }
            c if is_name_char(c) => {
                let mut e = start;
                for ch in self.s[e..].chars() {
                    if !is_name_char(ch) {
                        break;
                    }
                    e += ch.len_utf8();
                }
                (T::Word(&self.s[start..e]), e - start)
            }
            _ => return Err(err::SYNTAX),
        };
        self.i = start + tok.1;
        Ok(Some((tok.0, start, self.i)))
    }

    fn need(&mut self) -> Result<(T<'a>, usize, usize), u32> {
        self.next()?.ok_or(err::SYNTAX)
    }

    fn word(&mut self) -> Result<&'a str, u32> {
        match self.need()?.0 {
            T::Word(w) => Ok(w),
            _ => Err(err::SYNTAX),
        }
    }

    fn name(&mut self) -> Result<&'a str, u32> {
        let w = self.word()?;
        if is_name(w) {
            Ok(w)
        } else {
            Err(err::SYNTAX)
        }
    }

    fn literal(&mut self) -> Result<&'a str, u32> {
        match self.need()?.0 {
            T::Lit(l) => Ok(l),
            _ => Err(err::SYNTAX),
        }
    }

    fn end(&mut self) -> Result<(), u32> {
        match self.next()? {
            None => Ok(()),
            Some(_) => Err(err::SYNTAX),
        }
    }
}

/// Collapses runs of white space to one space and trims, as a public identifier is normalized.
pub fn normalize_pubid(s: &str) -> String {
    s.split([' ', '\t', '\r', '\n']).filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" ")
}

pub fn check_pubid(s: &str) -> Result<String, u32> {
    if s.chars().all(is_pubid_char) {
        Ok(normalize_pubid(s))
    } else {
        Err(err::PUBLICID)
    }
}

struct Node {
    kind: u8,
    quant: u8,
    name: Option<String>,
    children: Vec<usize>,
}

fn quant_of(t: &T) -> Option<u8> {
    match t {
        T::Quest => Some(model::QUANT_OPT),
        T::Star => Some(model::QUANT_REP),
        T::Plus => Some(model::QUANT_PLUS),
        _ => None,
    }
}

fn element_decl(lx: &mut Lexer) -> Result<Decl, u32> {
    let name = lx.name()?.to_string();
    let (first, _, _) = lx.need()?;
    let mut arena: Vec<Node> = Vec::new();
    match first {
        T::Word("EMPTY") | T::Word("ANY") => {
            let kind = if first == T::Word("EMPTY") { model::EMPTY } else { model::ANY };
            lx.end()?;
            arena.push(Node { kind, quant: 0, name: None, children: Vec::new() });
        }
        T::Open => {
            arena.push(Node { kind: model::SEQ, quant: 0, name: None, children: Vec::new() });
            let mut stack: Vec<(usize, Option<char>)> = vec![(0, None)];
            let mut mixed = false;
            let mut want_item = true;
            let mut pending: Option<(T, usize, usize)> = None;
            loop {
                let (t, _, e) = match pending.take() {
                    Some(p) => p,
                    None => lx.need()?,
                };
                let depth = stack.len();
                let top = stack.len() - 1;
                if want_item {
                    match t {
                        T::Open => {
                            if mixed {
                                return Err(err::SYNTAX);
                            }
                            let idx = arena.len();
                            arena.push(Node { kind: model::SEQ, quant: 0, name: None, children: Vec::new() });
                            let parent = stack[top].0;
                            arena[parent].children.push(idx);
                            stack.push((idx, None));
                        }
                        T::Hash("PCDATA") => {
                            if depth != 1 || mixed || !arena[0].children.is_empty() {
                                return Err(err::SYNTAX);
                            }
                            mixed = true;
                            arena[0].kind = model::MIXED;
                            want_item = false;
                        }
                        T::Word(w) if is_name(w) => {
                            if mixed && stack[top].1.is_none() {
                                return Err(err::SYNTAX);
                            }
                            let idx = arena.len();
                            arena.push(Node { kind: model::NAME, quant: 0, name: Some(w.to_string()), children: Vec::new() });
                            let parent = stack[top].0;
                            arena[parent].children.push(idx);
                            if let Some((nt, ns, ne)) = lx.next()? {
                                match quant_of(&nt) {
                                    Some(q) if ns == e => {
                                        if mixed {
                                            return Err(err::SYNTAX);
                                        }
                                        arena[idx].quant = q;
                                    }
                                    _ => pending = Some((nt, ns, ne)),
                                }
                            }
                            want_item = false;
                        }
                        _ => return Err(err::SYNTAX),
                    }
                } else {
                    match t {
                        T::Bar | T::Comma => {
                            let c = if t == T::Bar { '|' } else { ',' };
                            let (gi, conn) = stack[top];
                            if conn.is_some_and(|x| x != c) || (mixed && depth == 1 && c == ',') {
                                return Err(err::SYNTAX);
                            }
                            stack[top].1 = Some(c);
                            arena[gi].kind = if mixed && depth == 1 {
                                model::MIXED
                            } else if c == '|' {
                                model::CHOICE
                            } else {
                                model::SEQ
                            };
                            want_item = true;
                        }
                        T::Close => {
                            let (gi, _) = stack.pop().ok_or(err::SYNTAX)?;
                            let mut quant = 0;
                            if let Some((nt, ns, ne)) = lx.next()? {
                                match quant_of(&nt) {
                                    Some(q) if ns == e => quant = q,
                                    _ => pending = Some((nt, ns, ne)),
                                }
                            }
                            if mixed && stack.is_empty() {
                                let has_names = !arena[gi].children.is_empty();
                                if (has_names && quant != model::QUANT_REP) || (!has_names && quant != 0 && quant != model::QUANT_REP) {
                                    return Err(err::SYNTAX);
                                }
                            }
                            arena[gi].quant = quant;
                            if stack.is_empty() {
                                if pending.is_some() {
                                    return Err(err::SYNTAX);
                                }
                                lx.end()?;
                                break;
                            }
                        }
                        _ => return Err(err::SYNTAX),
                    }
                }
            }
        }
        _ => return Err(err::SYNTAX),
    }
    let mut out: Vec<ModelNode> = Vec::with_capacity(arena.len());
    let mut jobs: Vec<usize> = vec![0];
    let mut k = 0;
    while k < jobs.len() {
        let n = &arena[jobs[k]];
        let first = jobs.len();
        jobs.extend(n.children.iter().copied());
        out.push(ModelNode {
            kind: n.kind,
            quant: n.quant,
            name: n.name.clone(),
            first: if n.children.is_empty() { 0 } else { first },
            count: n.children.len(),
        });
        k += 1;
    }
    Ok(Decl::Element { name, model: Model { nodes: out } })
}

fn attlist_decl(lx: &mut Lexer) -> Result<Decl, u32> {
    let element = lx.name()?.to_string();
    let mut atts = Vec::new();
    while let Some((t, _, _)) = lx.next()? {
        let name = match t {
            T::Word(w) if is_name(w) => w.to_string(),
            _ => return Err(err::SYNTAX),
        };
        let atype = match lx.need()?.0 {
            T::Word(w @ ("CDATA" | "ID" | "IDREF" | "IDREFS" | "ENTITY" | "ENTITIES" | "NMTOKEN" | "NMTOKENS")) => w.to_string(),
            T::Word("NOTATION") => {
                if lx.need()?.0 != T::Open {
                    return Err(err::SYNTAX);
                }
                format!("NOTATION{}", enumeration(lx, true)?)
            }
            T::Open => enumeration(lx, false)?,
            _ => return Err(err::SYNTAX),
        };
        let default = match lx.need()?.0 {
            T::Hash("REQUIRED") => AttDefault::Required,
            T::Hash("IMPLIED") => AttDefault::Implied,
            T::Hash("FIXED") => AttDefault::Fixed(lx.literal()?.to_string()),
            T::Lit(l) => AttDefault::Value(l.to_string()),
            _ => return Err(err::SYNTAX),
        };
        atts.push(AttDecl { name, atype, default });
    }
    Ok(Decl::Attlist { element, atts })
}

/// The part of an enumerated type after `(`, through `)`: `(a|b)`.
fn enumeration(lx: &mut Lexer, names: bool) -> Result<String, u32> {
    let mut out = String::from("(");
    loop {
        let w = lx.word()?;
        if names && !is_name(w) {
            return Err(err::SYNTAX);
        }
        out.push_str(w);
        match lx.need()?.0 {
            T::Bar => out.push('|'),
            T::Close => {
                out.push(')');
                return Ok(out);
            }
            _ => return Err(err::SYNTAX),
        }
    }
}

fn entity_decl(lx: &mut Lexer) -> Result<Decl, u32> {
    let mut param = false;
    let mut t = lx.need()?.0;
    if t == T::Pct {
        param = true;
        t = lx.need()?.0;
    }
    let name = match t {
        T::Word(w) if is_name(w) => w.to_string(),
        _ => return Err(err::SYNTAX),
    };
    let def = match lx.need()?.0 {
        T::Lit(l) => EntityDef::Internal(l.to_string()),
        T::Word("SYSTEM") => EntityDef::External { pubid: None, sysid: lx.literal()?.to_string(), notation: None },
        T::Word("PUBLIC") => {
            let p = check_pubid(lx.literal()?)?;
            EntityDef::External { pubid: Some(p), sysid: lx.literal()?.to_string(), notation: None }
        }
        _ => return Err(err::SYNTAX),
    };
    let def = match def {
        EntityDef::External { pubid, sysid, .. } => {
            let notation = match lx.next()? {
                None => None,
                Some((T::Word("NDATA"), _, _)) if !param => Some(lx.name()?.to_string()),
                Some(_) => return Err(err::SYNTAX),
            };
            EntityDef::External { pubid, sysid, notation }
        }
        d => d,
    };
    lx.end()?;
    Ok(Decl::Entity { param, name, def })
}

fn notation_decl(lx: &mut Lexer) -> Result<Decl, u32> {
    let name = lx.name()?.to_string();
    let (pubid, sysid) = match lx.need()?.0 {
        T::Word("SYSTEM") => (None, Some(lx.literal()?.to_string())),
        T::Word("PUBLIC") => {
            let p = check_pubid(lx.literal()?)?;
            let sys = match lx.next()? {
                None => None,
                Some((T::Lit(l), _, _)) => Some(l.to_string()),
                Some(_) => return Err(err::SYNTAX),
            };
            (Some(p), sys)
        }
        _ => return Err(err::SYNTAX),
    };
    lx.end()?;
    Ok(Decl::Notation { name, pubid, sysid })
}

/// Parses `<!KEYWORD ... >` (the whole declaration including both delimiters).
pub fn parse_decl(text: &str) -> Result<Decl, u32> {
    let body = text.strip_prefix("<!").and_then(|t| t.strip_suffix('>')).ok_or(err::SYNTAX)?;
    let kw_end = body.find(|c: char| !c.is_ascii_uppercase()).unwrap_or(body.len());
    let kw = &body[..kw_end];
    let mut lx = Lexer { s: &body[kw_end..], i: 0 };
    if kw_end < body.len() && !is_space(body[kw_end..].chars().next().unwrap_or('x')) {
        return Err(err::SYNTAX);
    }
    match kw {
        "ELEMENT" => element_decl(&mut lx),
        "ATTLIST" => attlist_decl(&mut lx),
        "ENTITY" => entity_decl(&mut lx),
        "NOTATION" => notation_decl(&mut lx),
        _ => Err(err::SYNTAX),
    }
}
