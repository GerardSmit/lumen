use super::basic::{Op, Shape};
use super::geom::*;
use super::shape_fn::{Cmd, Ctrl, HVal, Origin, ShapeFn};
use super::*;
use lumen_common::svg_number::svg_number_token;

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Seg {
    pub cmd: u8,
    pub args: Vec<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct PathData {
    pub segs: Vec<Seg>,
}

const MAX_SEGMENTS: usize = 65536;

fn arity(command: u8) -> Option<usize> {
    Some(match command.to_ascii_uppercase() {
        b'M' | b'L' | b'T' => 2,
        b'H' | b'V' => 1,
        b'C' => 6,
        b'S' | b'Q' => 4,
        b'A' => 7,
        b'Z' => 0,
        _ => return None,
    })
}

fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0c)
}

fn number_start(byte: u8) -> bool {
    byte.is_ascii_digit() || matches!(byte, b'.' | b'+' | b'-')
}

struct Cursor<'a> {
    text: &'a str,
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn skip_space(&mut self) {
        while self.bytes.get(self.at).copied().is_some_and(is_space) {
            self.at += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn number(&mut self) -> Option<f64> {
        let end = svg_number_token(self.bytes, self.at).ok()?;
        let value: f64 = self.text[self.at..end].parse().ok()?;
        self.at = end;
        value.is_finite().then_some(value)
    }

    fn flag(&mut self) -> Option<f64> {
        let value = match self.peek()? {
            b'0' => 0.0,
            b'1' => 1.0,
            _ => return None,
        };
        self.at += 1;
        Some(value)
    }

    fn separator(&mut self) {
        self.skip_space();
        if self.peek() == Some(b',') {
            self.at += 1;
            self.skip_space();
        }
    }
}

pub(super) fn parse_data(text: &str) -> Option<PathData> {
    let mut cursor = Cursor { text, bytes: text.as_bytes(), at: 0 };
    cursor.skip_space();
    if !matches!(cursor.peek()?, b'M' | b'm') {
        return None;
    }
    let mut segs = Vec::new();
    loop {
        cursor.skip_space();
        let Some(command) = cursor.peek() else { break };
        let count = arity(command)?;
        cursor.at += 1;
        if count == 0 {
            segs.push(Seg { cmd: b'Z', args: Vec::new() });
            continue;
        }
        let mut first = true;
        loop {
            cursor.skip_space();
            if !first {
                if cursor.peek() == Some(b',') {
                    cursor.at += 1;
                    cursor.skip_space();
                    if !cursor.peek().is_some_and(number_start) {
                        return None;
                    }
                } else if !cursor.peek().is_some_and(number_start) {
                    break;
                }
            }
            let mut args = Vec::with_capacity(count);
            for index in 0..count {
                if index > 0 {
                    cursor.separator();
                }
                let is_flag = command.eq_ignore_ascii_case(&b'A') && (index == 3 || index == 4);
                args.push(if is_flag { cursor.flag()? } else { cursor.number()? });
            }
            let letter = if first {
                command
            } else {
                match command {
                    b'M' => b'L',
                    b'm' => b'l',
                    other => other,
                }
            };
            segs.push(Seg { cmd: letter, args });
            if segs.len() > MAX_SEGMENTS {
                return None;
            }
            first = false;
            cursor.skip_space();
            if cursor.peek() != Some(b',') && !cursor.peek().is_some_and(number_start) {
                break;
            }
        }
    }
    if segs.is_empty() {
        return None;
    }
    Some(PathData { segs })
}

pub(super) fn parse_function(inner: &str, allow_fill: bool) -> Option<Shape> {
    let parts = top_level_split(inner, b',', 3)?;
    let (evenodd, source) = match parts.as_slice() {
        [source] => (false, *source),
        [fill, source] if allow_fill => {
            let evenodd = match lower(fill).as_ref() {
                "nonzero" => false,
                "evenodd" => true,
                _ => return None,
            };
            (evenodd, *source)
        }
        _ => return None,
    };
    let text = css_string(source)?;
    Some(Shape::Path { evenodd, data: parse_data(&text)? })
}

impl PathData {
    pub(super) fn text(&self) -> String {
        let mut out = String::new();
        for (index, seg) in self.segs.iter().enumerate() {
            if index > 0 {
                out.push(' ');
            }
            out.push(char::from(seg.cmd));
            for arg in &seg.args {
                out.push(' ');
                out.push_str(&num(*arg));
            }
        }
        out
    }

    pub(super) fn absolute(&self) -> PathData {
        let mut current = (0.0f64, 0.0f64);
        let mut start = (0.0f64, 0.0f64);
        let mut segs = Vec::with_capacity(self.segs.len());
        for seg in &self.segs {
            let relative = seg.cmd.is_ascii_lowercase();
            let upper = seg.cmd.to_ascii_uppercase();
            let mut args = seg.args.clone();
            let (dx, dy) = if relative { current } else { (0.0, 0.0) };
            match upper {
                b'H' => {
                    args[0] += dx;
                    current.0 = args[0];
                }
                b'V' => {
                    args[0] += dy;
                    current.1 = args[0];
                }
                b'A' => {
                    args[5] += dx;
                    args[6] += dy;
                    current = (args[5], args[6]);
                }
                b'Z' => current = start,
                _ => {
                    for pair in args.chunks_mut(2) {
                        pair[0] += dx;
                        pair[1] += dy;
                    }
                    let last = args.len();
                    current = (args[last - 2], args[last - 1]);
                    if upper == b'M' {
                        start = current;
                    }
                }
            }
            segs.push(Seg { cmd: upper, args });
        }
        PathData { segs }
    }

    pub(super) fn to_shape_fn(&self, evenodd: bool) -> Option<ShapeFn> {
        let mut iterator = self.segs.iter();
        let first = iterator.next()?;
        if first.cmd.to_ascii_uppercase() != b'M' {
            return None;
        }
        let point = |x: f64, y: f64| Pos::from_lens(Len::pixels(x), Len::pixels(y));
        let mut cmds = Vec::new();
        for seg in iterator {
            let a = &seg.args;
            let by = seg.cmd.is_ascii_lowercase();
            let control = |x: f64, y: f64| Ctrl { pos: point(x, y), origin: Some(if by { Origin::Start } else { Origin::Origin }) };
            cmds.push(match seg.cmd.to_ascii_uppercase() {
                b'M' => Cmd::Move { by, to: point(a[0], a[1]) },
                b'L' => Cmd::Line { by, to: point(a[0], a[1]) },
                b'H' => Cmd::HLine { by, v: HVal::Len(Len::pixels(a[0])) },
                b'V' => Cmd::VLine { by, v: HVal::Len(Len::pixels(a[0])) },
                b'C' => Cmd::Curve {
                    by,
                    to: point(a[4], a[5]),
                    c1: control(a[0], a[1]),
                    c2: Some(control(a[2], a[3])),
                },
                b'Q' => Cmd::Curve { by, to: point(a[2], a[3]), c1: control(a[0], a[1]), c2: None },
                b'S' => Cmd::Smooth { by, to: point(a[2], a[3]), c: Some(control(a[0], a[1])) },
                b'T' => Cmd::Smooth { by, to: point(a[0], a[1]), c: None },
                b'A' => Cmd::Arc {
                    by,
                    to: point(a[5], a[6]),
                    r1: Len::pixels(a[0]),
                    r2: Some(Len::pixels(a[1])),
                    cw: a[4] != 0.0,
                    large: a[3] != 0.0,
                    rot: Angle::degrees(a[2]),
                },
                b'Z' => Cmd::Close,
                _ => return None,
            });
        }
        Some(ShapeFn { evenodd, from: point(first.args[0], first.args[1]), cmds })
    }
}

pub(super) fn mix(a: &PathData, b: &PathData, op: Op) -> Option<PathData> {
    if !matches!(op, Op::Interpolate(_)) {
        return None;
    }
    let a = a.absolute();
    let b = b.absolute();
    if a.segs.len() != b.segs.len() {
        return None;
    }
    let mut segs = Vec::with_capacity(a.segs.len());
    for (left, right) in a.segs.iter().zip(&b.segs) {
        if left.cmd != right.cmd {
            return None;
        }
        let mut args = Vec::with_capacity(left.args.len());
        for (index, (x, y)) in left.args.iter().zip(&right.args).enumerate() {
            let mixed = op.apply(*x, *y);
            let flag = left.cmd == b'A' && (index == 3 || index == 4);
            args.push(if flag { f64::from(u8::from(mixed > 0.0)) } else { mixed });
        }
        segs.push(Seg { cmd: left.cmd, args });
    }
    Some(PathData { segs })
}
