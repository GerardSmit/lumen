use super::basic::Op;
use super::geom::*;
use super::*;

const MAX_COMMANDS: usize = 16384;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Origin {
    Origin,
    Start,
    End,
}

impl Origin {
    fn text(self) -> &'static str {
        match self {
            Origin::Origin => "origin",
            Origin::Start => "start",
            Origin::End => "end",
        }
    }

    fn default_for(by: bool) -> Origin {
        if by {
            Origin::Start
        } else {
            Origin::Origin
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Ctrl {
    pub pos: Pos,
    pub origin: Option<Origin>,
}

impl Ctrl {
    fn effective(&self, by: bool) -> Origin {
        self.origin.unwrap_or(Origin::default_for(by))
    }

    fn text(&self, by: bool, form: Form) -> String {
        let mut out = self.pos.text(form);
        let origin = self.effective(by);
        if origin != Origin::default_for(by) {
            out.push_str(" from ");
            out.push_str(origin.text());
        }
        out
    }

    fn computed(&self) -> Ctrl {
        Ctrl { pos: self.pos.computed(), origin: self.origin }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum HVal {
    Len(Len),
    Kw(&'static str),
}

impl HVal {
    fn len(&self) -> Len {
        match self {
            HVal::Len(len) => len.computed(),
            HVal::Kw(keyword) => match *keyword {
                "left" | "top" | "x-start" | "y-start" => Len::percent(0.0),
                "center" => Len::percent(50.0),
                _ => Len::percent(100.0),
            },
        }
    }

    fn text(&self, form: Form) -> String {
        match (self, form) {
            (HVal::Kw(keyword), Form::Specified) => String::from(*keyword),
            (HVal::Len(len), form) => len.text(form),
            (_, Form::Computed) => self.len().computed_text(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Cmd {
    Move { by: bool, to: Pos },
    Line { by: bool, to: Pos },
    HLine { by: bool, v: HVal },
    VLine { by: bool, v: HVal },
    Curve { by: bool, to: Pos, c1: Ctrl, c2: Option<Ctrl> },
    Smooth { by: bool, to: Pos, c: Option<Ctrl> },
    Arc { by: bool, to: Pos, r1: Len, r2: Option<Len>, cw: bool, large: bool, rot: Angle },
    Close,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct ShapeFn {
    pub evenodd: bool,
    pub from: Pos,
    pub cmds: Vec<Cmd>,
}

fn direction(by: bool) -> &'static str {
    if by {
        "by"
    } else {
        "to"
    }
}

fn parse_direction(token: &str) -> Option<bool> {
    match lower(token).as_ref() {
        "to" => Some(false),
        "by" => Some(true),
        _ => None,
    }
}

fn parse_target(tokens: &[&str], by: bool, env: Env) -> Option<Pos> {
    if by {
        let [x, y] = tokens else { return None };
        return Some(Pos::from_lens(parse_len(x, env, false)?, parse_len(y, env, false)?));
    }
    parse_position(tokens, env)
}

fn parse_origin(token: &str) -> Option<Origin> {
    match lower(token).as_ref() {
        "origin" => Some(Origin::Origin),
        "start" => Some(Origin::Start),
        "end" => Some(Origin::End),
        _ => None,
    }
}

fn parse_ctrl(text: &str, by: bool, env: Env) -> Option<Ctrl> {
    let tokens = components(text)?;
    let mut origin = None;
    let mut coordinates = &tokens[..];
    if tokens.len() >= 3 && tokens[tokens.len() - 2].eq_ignore_ascii_case("from") {
        origin = Some(parse_origin(tokens[tokens.len() - 1])?);
        coordinates = &tokens[..tokens.len() - 2];
    }
    let pos = if by || origin.is_some() {
        let [x, y] = coordinates else { return None };
        Pos::from_lens(parse_len(x, env, false)?, parse_len(y, env, false)?)
    } else {
        parse_position(coordinates, env)?
    };
    Some(Ctrl { pos, origin })
}

fn parse_hval(token: &str, by: bool, horizontal: bool, env: Env) -> Option<HVal> {
    let keyword = lower(token);
    let table: &[&'static str] = if horizontal {
        &["left", "center", "right", "x-start", "x-end"]
    } else {
        &["top", "center", "bottom", "y-start", "y-end"]
    };
    if let Some(found) = table.iter().find(|candidate| **candidate == keyword.as_ref()) {
        return if by { None } else { Some(HVal::Kw(*found)) };
    }
    Some(HVal::Len(parse_len(token, env, false)?))
}

fn parse_command(chunk: &str, env: Env) -> Option<Cmd> {
    let tokens = components(chunk)?;
    let name = lower(tokens.first()?);
    if name == "close" {
        return (tokens.len() == 1).then_some(Cmd::Close);
    }
    let by = parse_direction(tokens.get(1)?)?;
    match name.as_ref() {
        "move" | "line" => {
            let to = parse_target(&tokens[2..], by, env)?;
            Some(if name == "move" { Cmd::Move { by, to } } else { Cmd::Line { by, to } })
        }
        "hline" | "vline" => {
            let [_, _, value] = tokens.as_slice() else { return None };
            let horizontal = name == "hline";
            let v = parse_hval(value, by, horizontal, env)?;
            Some(if horizontal { Cmd::HLine { by, v } } else { Cmd::VLine { by, v } })
        }
        "curve" | "smooth" => {
            let with = tokens.iter().position(|token| token.eq_ignore_ascii_case("with"));
            let end = with.unwrap_or(tokens.len());
            if end < 2 {
                return None;
            }
            let to = parse_target(&tokens[2..end], by, env)?;
            let controls = match with {
                Some(index) => {
                    let text = tokens[index + 1..].join(" ");
                    Some(top_level_split(&text, b'/', 3)?.into_iter().map(|part| parse_ctrl(part, by, env)).collect::<Option<Vec<_>>>()?)
                }
                None => None,
            };
            if name == "curve" {
                let mut controls = controls?.into_iter();
                let c1 = controls.next()?;
                let c2 = controls.next();
                if controls.next().is_some() {
                    return None;
                }
                Some(Cmd::Curve { by, to, c1, c2 })
            } else {
                let c = match controls {
                    Some(mut controls) => {
                        if controls.len() != 1 {
                            return None;
                        }
                        controls.pop()
                    }
                    None => None,
                };
                Some(Cmd::Smooth { by, to, c })
            }
        }
        "arc" => {
            let of = tokens.iter().position(|token| token.eq_ignore_ascii_case("of"))?;
            if of < 2 {
                return None;
            }
            let to = parse_target(&tokens[2..of], by, env)?;
            let rest = &tokens[of + 1..];
            let r1 = parse_len(rest.first()?, env, false)?;
            let mut index = 1;
            let mut r2 = None;
            if let Some(token) = rest.get(1) {
                let keyword = lower(token);
                if !matches!(keyword.as_ref(), "cw" | "ccw" | "large" | "small" | "rotate") {
                    r2 = Some(parse_len(token, env, false)?);
                    index = 2;
                }
            }
            let mut sweep = None;
            let mut size = None;
            let mut rotation = None;
            while index < rest.len() {
                match lower(rest[index]).as_ref() {
                    "cw" | "ccw" if sweep.is_none() => sweep = Some(rest[index].eq_ignore_ascii_case("cw")),
                    "large" | "small" if size.is_none() => size = Some(rest[index].eq_ignore_ascii_case("large")),
                    "rotate" if rotation.is_none() => {
                        index += 1;
                        rotation = Some(parse_angle(rest.get(index)?)?);
                    }
                    _ => return None,
                }
                index += 1;
            }
            Some(Cmd::Arc {
                by,
                to,
                r1,
                r2,
                cw: sweep.unwrap_or(false),
                large: size.unwrap_or(false),
                rot: rotation.unwrap_or_else(|| Angle::degrees(0.0)),
            })
        }
        _ => None,
    }
}

pub(super) fn parse(inner: &str, env: Env) -> Option<ShapeFn> {
    let chunks = top_level_split(inner, b',', MAX_COMMANDS)?;
    let head = components(chunks[0])?;
    let mut evenodd = false;
    let mut index = 0;
    match head.first().map(|token| lower(token)).as_deref() {
        Some("nonzero") => index = 1,
        Some("evenodd") => {
            evenodd = true;
            index = 1;
        }
        _ => {}
    }
    if !head.get(index)?.eq_ignore_ascii_case("from") {
        return None;
    }
    let from = parse_position(&head[index + 1..], env)?;
    let mut cmds = Vec::new();
    for chunk in &chunks[1..] {
        cmds.push(parse_command(chunk, env)?);
    }
    if cmds.is_empty() {
        return None;
    }
    Some(ShapeFn { evenodd, from, cmds })
}

impl Cmd {
    fn text(&self, form: Form) -> String {
        match self {
            Cmd::Move { by, to } => alloc::format!("move {} {}", direction(*by), to.text(form)),
            Cmd::Line { by, to } => alloc::format!("line {} {}", direction(*by), to.text(form)),
            Cmd::HLine { by, v } => alloc::format!("hline {} {}", direction(*by), v.text(form)),
            Cmd::VLine { by, v } => alloc::format!("vline {} {}", direction(*by), v.text(form)),
            Cmd::Curve { by, to, c1, c2 } => {
                let mut out = alloc::format!("curve {} {} with {}", direction(*by), to.text(form), c1.text(*by, form));
                if let Some(c2) = c2 {
                    out.push_str(" / ");
                    out.push_str(&c2.text(*by, form));
                }
                out
            }
            Cmd::Smooth { by, to, c } => {
                let mut out = alloc::format!("smooth {} {}", direction(*by), to.text(form));
                if let Some(c) = c {
                    out.push_str(" with ");
                    out.push_str(&c.text(*by, form));
                }
                out
            }
            Cmd::Arc { by, to, r1, r2, cw, large, rot } => {
                let mut out = alloc::format!("arc {} {} of {}", direction(*by), to.text(form), r1.text(form));
                if let Some(r2) = r2 {
                    out.push(' ');
                    out.push_str(&r2.text(form));
                }
                if *cw {
                    out.push_str(" cw");
                }
                if *large {
                    out.push_str(" large");
                }
                if rot.deg != 0.0 {
                    out.push_str(" rotate ");
                    out.push_str(&rot.text(form));
                }
                out
            }
            Cmd::Close => String::from("close"),
        }
    }

    fn compute(&self) -> Cmd {
        match self {
            Cmd::Move { by, to } => Cmd::Move { by: *by, to: to.computed() },
            Cmd::Line { by, to } => Cmd::Line { by: *by, to: to.computed() },
            Cmd::HLine { by, v } => Cmd::HLine { by: *by, v: HVal::Len(v.len()) },
            Cmd::VLine { by, v } => Cmd::VLine { by: *by, v: HVal::Len(v.len()) },
            Cmd::Curve { by, to, c1, c2 } => {
                Cmd::Curve { by: *by, to: to.computed(), c1: c1.computed(), c2: c2.as_ref().map(Ctrl::computed) }
            }
            Cmd::Smooth { by, to, c } => Cmd::Smooth { by: *by, to: to.computed(), c: c.as_ref().map(Ctrl::computed) },
            Cmd::Arc { by, to, r1, r2, cw, large, rot } => Cmd::Arc {
                by: *by,
                to: to.computed(),
                r1: r1.computed(),
                r2: r2.as_ref().map(Len::computed),
                cw: *cw,
                large: *large,
                rot: Angle::degrees(rot.deg),
            },
            Cmd::Close => Cmd::Close,
        }
    }
}

impl ShapeFn {
    pub(super) fn text(&self, form: Form) -> String {
        let mut out = String::from("shape(");
        if self.evenodd {
            out.push_str("evenodd ");
        }
        out.push_str("from ");
        out.push_str(&self.from.text(form));
        for cmd in &self.cmds {
            out.push_str(", ");
            out.push_str(&cmd.text(form));
        }
        out.push(')');
        out
    }

    pub(super) fn compute(&self) -> ShapeFn {
        ShapeFn { evenodd: self.evenodd, from: self.from.computed(), cmds: self.cmds.iter().map(Cmd::compute).collect() }
    }
}

fn mix_len(a: &Len, b: &Len, op: Op) -> Len {
    a.map2(b, &|x, y| op.apply(x, y))
}

fn mix_pos(a: &Pos, b: &Pos, op: Op) -> Pos {
    let (ax, ay) = a.lens();
    let (bx, by) = b.lens();
    Pos::from_lens(mix_len(&ax, &bx, op), mix_len(&ay, &by, op))
}

fn mix_ctrl(a: &Ctrl, b: &Ctrl, by: bool, op: Op) -> Option<Ctrl> {
    if a.effective(by) != b.effective(by) {
        return None;
    }
    Some(Ctrl { pos: mix_pos(&a.pos, &b.pos, op), origin: a.origin })
}

fn mix_cmd(a: &Cmd, b: &Cmd, op: Op) -> Option<Cmd> {
    match (a, b) {
        (Cmd::Move { by: ba, to: ta }, Cmd::Move { by: bb, to: tb }) if ba == bb => {
            Some(Cmd::Move { by: *ba, to: mix_pos(ta, tb, op) })
        }
        (Cmd::Line { by: ba, to: ta }, Cmd::Line { by: bb, to: tb }) if ba == bb => {
            Some(Cmd::Line { by: *ba, to: mix_pos(ta, tb, op) })
        }
        (Cmd::HLine { by: ba, v: va }, Cmd::HLine { by: bb, v: vb }) if ba == bb => {
            Some(Cmd::HLine { by: *ba, v: HVal::Len(mix_len(&va.len(), &vb.len(), op)) })
        }
        (Cmd::VLine { by: ba, v: va }, Cmd::VLine { by: bb, v: vb }) if ba == bb => {
            Some(Cmd::VLine { by: *ba, v: HVal::Len(mix_len(&va.len(), &vb.len(), op)) })
        }
        (Cmd::Curve { by: ba, to: ta, c1: c1a, c2: c2a }, Cmd::Curve { by: bb, to: tb, c1: c1b, c2: c2b }) if ba == bb => {
            let c2 = match (c2a, c2b) {
                (Some(a), Some(b)) => Some(mix_ctrl(a, b, *ba, op)?),
                (None, None) => None,
                _ => return None,
            };
            Some(Cmd::Curve { by: *ba, to: mix_pos(ta, tb, op), c1: mix_ctrl(c1a, c1b, *ba, op)?, c2 })
        }
        (Cmd::Smooth { by: ba, to: ta, c: ca }, Cmd::Smooth { by: bb, to: tb, c: cb }) if ba == bb => {
            let c = match (ca, cb) {
                (Some(a), Some(b)) => Some(mix_ctrl(a, b, *ba, op)?),
                (None, None) => None,
                _ => return None,
            };
            Some(Cmd::Smooth { by: *ba, to: mix_pos(ta, tb, op), c })
        }
        (
            Cmd::Arc { by: ba, to: ta, r1: r1a, r2: r2a, cw: cwa, large: la, rot: rota },
            Cmd::Arc { by: bb, to: tb, r1: r1b, r2: r2b, cw: cwb, large: lb, rot: rotb },
        ) if ba == bb => {
            let r2 = match (r2a, r2b) {
                (None, None) => None,
                _ => Some(mix_len(r2a.as_ref().unwrap_or(r1a), r2b.as_ref().unwrap_or(r1b), op)),
            };
            let flag = |a: bool, b: bool| op.apply(f64::from(u8::from(a)), f64::from(u8::from(b))) > 0.0;
            Some(Cmd::Arc {
                by: *ba,
                to: mix_pos(ta, tb, op),
                r1: mix_len(r1a, r1b, op),
                r2,
                cw: flag(*cwa, *cwb),
                large: flag(*la, *lb),
                rot: Angle::degrees(op.apply(rota.deg, rotb.deg)),
            })
        }
        (Cmd::Close, Cmd::Close) => Some(Cmd::Close),
        _ => None,
    }
}

pub(super) fn mix(a: &ShapeFn, b: &ShapeFn, op: Op) -> Option<ShapeFn> {
    if a.evenodd != b.evenodd || a.cmds.len() != b.cmds.len() {
        return None;
    }
    let cmds = a.cmds.iter().zip(&b.cmds).map(|(x, y)| mix_cmd(x, y, op)).collect::<Option<Vec<_>>>()?;
    Some(ShapeFn { evenodd: a.evenodd, from: mix_pos(&a.from, &b.from, op), cmds })
}
