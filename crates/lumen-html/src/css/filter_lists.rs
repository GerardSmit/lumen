use super::*;

#[derive(Clone, Debug, PartialEq)]
enum Function {
    Amount { name: &'static str, value: f64 },
    DropShadow { color: Rgba, x: f64, y: f64, blur: f64 },
    Url(String),
}

const AMOUNTS: [(&str, f64); 9] = [
    ("blur", 0.0),
    ("brightness", 1.0),
    ("contrast", 1.0),
    ("grayscale", 0.0),
    ("hue-rotate", 0.0),
    ("invert", 0.0),
    ("opacity", 1.0),
    ("saturate", 1.0),
    ("sepia", 0.0),
];

fn split_functions(value: &str) -> Option<Vec<&str>> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = None;
    for (index, ch) in value.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth = depth.checked_sub(1)?,
            c if c.is_whitespace() && depth == 0 => {
                if let Some(begin) = start.take() {
                    parts.push(&value[begin..index]);
                }
                continue;
            }
            _ => {}
        }
        if start.is_none() {
            start = Some(index);
        }
    }
    if let Some(begin) = start {
        parts.push(&value[begin..]);
    }
    (depth == 0).then_some(parts)
}

fn parse_length(token: &str) -> Option<f64> {
    token.strip_suffix("px")?.parse().ok()
}

fn parse_function(part: &str) -> Option<Function> {
    let open = part.find('(')?;
    let name = &part[..open];
    let args = part[open + 1..].strip_suffix(')')?.trim();
    if name == "url" {
        return Some(Function::Url(part.to_owned()));
    }
    if name == "drop-shadow" {
        let tokens = split_functions(args)?;
        let [color, x, y, blur] = tokens.as_slice() else { return None };
        return Some(Function::DropShadow {
            color: super::color(color)?,
            x: parse_length(x)?,
            y: parse_length(y)?,
            blur: parse_length(blur)?,
        });
    }
    let (canonical, _) = AMOUNTS.iter().find(|(candidate, _)| *candidate == name)?;
    let number = match *canonical {
        "blur" => parse_length(args)?,
        "hue-rotate" => args.strip_suffix("deg")?.parse().ok()?,
        _ => args.parse().ok()?,
    };
    Some(Function::Amount { name: canonical, value: number })
}

fn parse_list(value: &str) -> Option<Vec<Function>> {
    let value = value.trim();
    if value == "none" {
        return Some(Vec::new());
    }
    split_functions(value)?.into_iter().map(parse_function).collect()
}

fn identity(function: &Function) -> Function {
    match function {
        Function::Amount { name, .. } => {
            let value = AMOUNTS.iter().find(|(candidate, _)| candidate == name).map_or(0.0, |(_, value)| *value);
            Function::Amount { name, value }
        }
        Function::DropShadow { .. } => Function::DropShadow { color: Rgba { r: 0, g: 0, b: 0, a: 0 }, x: 0.0, y: 0.0, blur: 0.0 },
        Function::Url(url) => Function::Url(url.clone()),
    }
}

fn clamp_amount(name: &str, value: f64) -> f64 {
    match name {
        "blur" | "brightness" | "contrast" | "saturate" => value.max(0.0),
        "grayscale" | "invert" | "opacity" | "sepia" => value.clamp(0.0, 1.0),
        _ => value,
    }
}

use lumen_common::numeric::lerp as mix;

fn mix_channel(a: u8, a_alpha: f64, b: u8, b_alpha: f64, alpha: f64, progress: f64) -> u8 {
    if alpha <= 0.0 {
        return 0;
    }
    let premultiplied = mix(f64::from(a) * a_alpha, f64::from(b) * b_alpha, progress);
    libm::floor(premultiplied / alpha + 0.5).clamp(0.0, 255.0) as u8
}

fn mix_color(a: Rgba, b: Rgba, progress: f64) -> Rgba {
    let (a_alpha, b_alpha) = (f64::from(a.a) / 255.0, f64::from(b.a) / 255.0);
    let alpha = mix(a_alpha, b_alpha, progress).clamp(0.0, 1.0);
    Rgba {
        r: mix_channel(a.r, a_alpha, b.r, b_alpha, alpha, progress),
        g: mix_channel(a.g, a_alpha, b.g, b_alpha, alpha, progress),
        b: mix_channel(a.b, a_alpha, b.b, b_alpha, alpha, progress),
        a: libm::floor(alpha * 255.0 + 0.5) as u8,
    }
}

fn serialize_function(function: &Function) -> String {
    match function {
        Function::Amount { name: "blur", value } => alloc::format!("blur({})", computed_values::px(*value as f32)),
        Function::Amount { name: "hue-rotate", value } => alloc::format!("hue-rotate({}deg)", computed_values::number(*value as f32)),
        Function::Amount { name, value } => alloc::format!("{name}({})", computed_values::number(*value as f32)),
        Function::DropShadow { color, x, y, blur } => alloc::format!(
            "drop-shadow({} {} {} {})",
            computed_values::color(*color),
            computed_values::px(*x as f32),
            computed_values::px(*y as f32),
            computed_values::px(*blur as f32)
        ),
        Function::Url(url) => url.clone(),
    }
}

fn serialize_list(list: &[Function]) -> String {
    if list.is_empty() {
        return String::from("none");
    }
    list.iter().map(serialize_function).collect::<Vec<_>>().join(" ")
}

fn same_kind(a: &Function, b: &Function) -> bool {
    match (a, b) {
        (Function::Amount { name: a, .. }, Function::Amount { name: b, .. }) => a == b,
        (Function::DropShadow { .. }, Function::DropShadow { .. }) => true,
        (Function::Url(a), Function::Url(b)) => a == b,
        _ => false,
    }
}

fn pad(list: &mut Vec<Function>, other: &[Function]) -> Option<()> {
    while list.len() < other.len() {
        if matches!(other[list.len()], Function::Url(_)) {
            return None;
        }
        let template = identity(&other[list.len()]);
        list.push(template);
    }
    Some(())
}

pub(crate) fn interpolate(from: &str, to: &str, progress: f64) -> Option<String> {
    if !progress.is_finite() {
        return None;
    }
    let mut a = parse_list(from)?;
    let mut b = parse_list(to)?;
    if a.iter().chain(b.iter()).any(|function| matches!(function, Function::Url(_))) {
        return None;
    }
    let common = a.len().min(b.len());
    if !a.iter().zip(b.iter()).take(common).all(|(x, y)| same_kind(x, y)) {
        return None;
    }
    if a.len() < b.len() {
        let template = b.clone();
        pad(&mut a, &template)?;
    } else {
        let template = a.clone();
        pad(&mut b, &template)?;
    }
    let mut out = Vec::with_capacity(a.len());
    for (x, y) in a.iter().zip(b.iter()) {
        out.push(match (x, y) {
            (Function::Amount { name, value: x }, Function::Amount { value: y, .. }) => {
                Function::Amount { name, value: clamp_amount(name, mix(*x, *y, progress)) }
            }
            (
                Function::DropShadow { color: ca, x: xa, y: ya, blur: ba },
                Function::DropShadow { color: cb, x: xb, y: yb, blur: bb },
            ) => Function::DropShadow {
                color: mix_color(*ca, *cb, progress),
                x: mix(*xa, *xb, progress),
                y: mix(*ya, *yb, progress),
                blur: mix(*ba, *bb, progress).max(0.0),
            },
            (Function::Url(url), _) => Function::Url(url.clone()),
            _ => return None,
        });
    }
    Some(serialize_list(&out))
}

fn bound_amounts(list: &mut [Function]) {
    for function in list {
        if let Function::Amount { name, value } = function {
            *value = clamp_amount(name, *value);
        }
    }
}

fn add_colors(a: Rgba, b: Rgba) -> Rgba {
    let (a_alpha, b_alpha) = (f64::from(a.a) / 255.0, f64::from(b.a) / 255.0);
    let alpha = (a_alpha + b_alpha).clamp(0.0, 1.0);
    if alpha <= 0.0 {
        return Rgba { r: 0, g: 0, b: 0, a: 0 };
    }
    let channel = |x: u8, y: u8| libm::floor((f64::from(x) * a_alpha + f64::from(y) * b_alpha) / alpha + 0.5).clamp(0.0, 255.0) as u8;
    Rgba { r: channel(a.r, b.r), g: channel(a.g, b.g), b: channel(a.b, b.b), a: libm::floor(alpha * 255.0 + 0.5) as u8 }
}

pub(crate) fn compose(base: &str, value: &str, accumulate: bool) -> Option<String> {
    let mut base = parse_list(base)?;
    let mut value = parse_list(value)?;
    bound_amounts(&mut base);
    bound_amounts(&mut value);
    if !accumulate {
        base.extend(value);
        return Some(serialize_list(&base));
    }
    let shared = base.len().min(value.len());
    let mismatched = base.iter().zip(value.iter()).take(shared).any(|(x, y)| !same_kind(x, y) || matches!(x, Function::Url(_)));
    if mismatched || base.iter().chain(value.iter()).any(|function| matches!(function, Function::Url(_))) {
        return Some(serialize_list(&value));
    }
    let mut out = Vec::with_capacity(base.len().max(value.len()));
    for (index, function) in base.iter().enumerate() {
        out.push(match value.get(index) {
            Some(Function::Amount { value: y, .. }) => match function {
                Function::Amount { name, value: x } => {
                    let identity = AMOUNTS.iter().find(|(candidate, _)| candidate == name).map_or(0.0, |(_, value)| *value);
                    Function::Amount { name, value: x + y - identity }
                }
                _ => function.clone(),
            },
            Some(Function::DropShadow { color: cb, x: xb, y: yb, blur: bb }) => match function {
                Function::DropShadow { color: ca, x: xa, y: ya, blur: ba } => Function::DropShadow {
                    color: add_colors(*ca, *cb),
                    x: xa + xb,
                    y: ya + yb,
                    blur: ba + bb,
                },
                _ => function.clone(),
            },
            _ => function.clone(),
        });
    }
    out.extend(value.into_iter().skip(shared));
    Some(serialize_list(&out))
}
