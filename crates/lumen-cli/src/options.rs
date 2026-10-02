//! Node's command-line option grammar: the table of known options, `--opt=value` / `--opt value`
//! forms, `--no-opt` negation, `NODE_OPTIONS` tokenizing and the diagnostics (with Node's exit
//! code 9) for malformed command lines.

use std::collections::HashMap;
use std::sync::OnceLock;

const TABLE: &str = include_str!("node_options.txt");

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Bool,
    Value,
}

struct Spec {
    kind: Kind,
    in_node_options: bool,
}

fn table() -> &'static HashMap<&'static str, Spec> {
    static T: OnceLock<HashMap<&'static str, Spec>> = OnceLock::new();
    T.get_or_init(|| {
        TABLE
            .lines()
            .filter_map(|l| {
                let mut p = l.split(' ');
                let name = p.next()?;
                let kind = if p.next()? == "b" {
                    Kind::Bool
                } else {
                    Kind::Value
                };
                let in_node_options = p.next()? == "1";
                Some((
                    name,
                    Spec {
                        kind,
                        in_node_options,
                    },
                ))
            })
            .collect()
    })
}

/// A command-line diagnostic: the message and the exit status Node uses for it.
pub struct OptionError {
    pub message: String,
    pub status: i32,
}

impl OptionError {
    fn bad_usage(message: String) -> OptionError {
        OptionError { message, status: 9 }
    }
}

/// The value of an option as the JS side reads it back (`getOptionValue`).
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Bool(bool),
    Str(String),
    List(Vec<String>),
}

#[derive(Default)]
pub struct Parsed {
    /// Every runtime flag and its value before the script, as `process.execArgv` reports them.
    pub exec_argv: Vec<String>,
    /// The script (when no `--eval`) and its arguments, or the `process.argv` tail for eval.
    pub rest: Vec<String>,
    pub eval: Option<String>,
    pub print: bool,
    pub check: bool,
    pub interactive: bool,
    pub version: bool,
    pub help: bool,
    pub stdin_dash: bool,
    pub options: HashMap<String, Value>,
}

impl Parsed {
    pub fn flag(&self, name: &str) -> bool {
        matches!(self.options.get(name), Some(Value::Bool(true)))
    }

    pub fn string(&self, name: &str) -> Option<&str> {
        match self.options.get(name) {
            Some(Value::Str(s)) => Some(s),
            Some(Value::List(l)) => l.last().map(String::as_str),
            _ => None,
        }
    }

    pub fn list(&self, name: &str) -> &[String] {
        match self.options.get(name) {
            Some(Value::List(l)) => l,
            _ => &[],
        }
    }

    /// The options as a JSON object, for the runtime to answer `getOptionValue` from.
    /// Fold in options from `NODE_OPTIONS`: the command line wins, except lists, which
    /// concatenate with the environment's entries first.
    pub fn merge_env(&mut self, env: Parsed) {
        for (name, value) in env.options {
            match (self.options.get_mut(&name), value) {
                (None, v) => {
                    self.options.insert(name, v);
                }
                (Some(Value::List(cli)), Value::List(mut e)) => {
                    e.append(cli);
                    *cli = e;
                }
                _ => {}
            }
        }
    }

    pub fn options_json(&self) -> String {
        let mut keys: Vec<&String> = self.options.keys().collect();
        keys.sort();
        let mut out = String::from("{");
        for (i, k) in keys.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&lumen_common::json::json_string(k));
            out.push(':');
            match &self.options[*k] {
                Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
                Value::Str(s) => out.push_str(&lumen_common::json::json_string(s)),
                Value::List(l) => {
                    out.push('[');
                    for (j, s) in l.iter().enumerate() {
                        if j > 0 {
                            out.push(',');
                        }
                        out.push_str(&lumen_common::json::json_string(s));
                    }
                    out.push(']');
                }
            }
        }
        out.push('}');
        out
    }
}

/// Options that accumulate into a list instead of the last one winning.
fn is_list_option(name: &str) -> bool {
    matches!(
        name,
        "--require"
            | "--import"
            | "--conditions"
            | "--env-file"
            | "--env-file-if-exists"
            | "--allow-fs-read"
            | "--allow-fs-write"
            | "--disable-warning"
            | "--loader"
            | "--experimental-loader"
            | "--watch-path"
            | "--test-name-pattern"
            | "--test-reporter"
            | "--test-reporter-destination"
    )
}

fn canonical_alias(name: &str) -> &str {
    match name {
        "-r" => "--require",
        "-C" => "--conditions",
        "-e" => "--eval",
        "-p" => "--print",
        "-c" => "--check",
        "-i" => "--interactive",
        "-h" => "--help",
        "-v" => "--version",
        "--debug-port" => "--inspect-port",
        "--loader" => "--experimental-loader",
        "--report-dir" => "--report-directory",
        other => other,
    }
}

/// `--foo_bar=x` → (`--foo-bar`, Some(`x`)): underscores in the name equal dashes.
fn split_name(arg: &str) -> (String, Option<String>) {
    let (name, value) = match arg.find('=') {
        Some(i) => (&arg[..i], Some(arg[i + 1..].to_string())),
        None => (arg, None),
    };
    let normalized = if name.starts_with("--") {
        name.replace('_', "-")
    } else {
        name.to_string()
    };
    (normalized, value)
}

/// `--experimental-*` flags track Node's moving feature set; ones this table predates are
/// accepted (as booleans) rather than failing a command line written for another Node version.
const EXPERIMENTAL: Spec = Spec {
    kind: Kind::Bool,
    in_node_options: true,
};

fn lookup(name: &str) -> Option<(&'static str, &'static Spec)> {
    let name = canonical_alias(name);
    if let Some((k, v)) = table().get_key_value(name) {
        return Some((*k, v));
    }
    if name.starts_with("--experimental-") {
        return Some((Box::leak(name.to_string().into_boxed_str()), &EXPERIMENTAL));
    }
    None
}

fn set(parsed: &mut Parsed, name: &str, value: Value) {
    if is_list_option(name) {
        if let Value::Str(s) = value {
            match parsed.options.get_mut(name) {
                Some(Value::List(l)) => l.push(s),
                _ => {
                    parsed.options.insert(name.to_string(), Value::List(vec![s]));
                }
            }
        }
    } else {
        parsed.options.insert(name.to_string(), value);
    }
}

/// Parse `args` (the command line after the executable). `argv0` prefixes diagnostics. With
/// `from_env` the arguments come from `NODE_OPTIONS`: only options allowed there are accepted
/// and no script or eval source is.
pub fn parse(
    argv0: &str,
    args: &[String],
    from_env: bool,
    parsed: &mut Parsed,
) -> Result<(), OptionError> {
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        i += 1;
        if arg == "--" {
            if from_env {
                return Err(OptionError::bad_usage(format!(
                    "{argv0}: -- is not allowed in NODE_OPTIONS"
                )));
            }
            parsed.exec_argv.push(arg.clone());
            parsed.rest.extend(args[i..].iter().cloned());
            return Ok(());
        }
        if arg == "-" {
            if from_env {
                return Err(OptionError::bad_usage(format!(
                    "{argv0}: - is not allowed in NODE_OPTIONS"
                )));
            }
            parsed.stdin_dash = true;
            parsed.rest.extend(args[i..].iter().cloned());
            return Ok(());
        }
        if !arg.starts_with('-') {
            parsed.rest.push(arg.clone());
            parsed.rest.extend(args[i..].iter().cloned());
            return Ok(());
        }
        if !from_env {
            parsed.exec_argv.push(arg.clone());
        }

        let (name, inline) = if arg == "-pe" || arg == "-ep" {
            ("--print".to_string(), None)
        } else {
            split_name(arg)
        };

        let (resolved, spec, negated) = match lookup(&name) {
            Some((n, s)) => (n.to_string(), s, false),
            None => {
                let base = name
                    .strip_prefix("--no-")
                    .map(|b| format!("--{b}"));
                match base.as_deref().and_then(lookup) {
                    Some((n, s)) => {
                        if s.kind != Kind::Bool {
                            return Err(OptionError::bad_usage(format!(
                                "{argv0}: {name} is an invalid negation because it is not a boolean option"
                            )));
                        }
                        (n.to_string(), s, true)
                    }
                    None => {
                        return Err(OptionError::bad_usage(format!(
                            "{argv0}: bad option: {arg}"
                        )));
                    }
                }
            }
        };

        if from_env && !spec.in_node_options {
            return Err(OptionError::bad_usage(format!(
                "{argv0}: {arg} is not allowed in NODE_OPTIONS"
            )));
        }

        match spec.kind {
            Kind::Bool => {
                if inline.is_some() {
                    return Err(OptionError::bad_usage(format!(
                        "{argv0}: {name} does not take an argument"
                    )));
                }
                if resolved == "--print" && !negated {
                    // `-p code` is `-p -e code` when the next argument is not an option.
                    parsed.print = true;
                    set(parsed, &resolved, Value::Bool(true));
                    if i < args.len() && !args[i].starts_with('-') {
                        let code = args[i].clone();
                        i += 1;
                        if !from_env {
                            parsed.exec_argv.push(code.clone());
                        }
                        parsed.eval = Some(code.strip_prefix('\\').filter(|r| r.starts_with('-')).unwrap_or(&code).to_string());
                        set(parsed, "--eval", Value::Str(parsed.eval.clone().unwrap()));
                    }
                    continue;
                }
                match resolved.as_str() {
                    "--check" => parsed.check = !negated,
                    "--interactive" => parsed.interactive = !negated,
                    "--version" => parsed.version = !negated,
                    "--help" => parsed.help = !negated,
                    _ => {}
                }
                set(parsed, &resolved, Value::Bool(!negated));
            }
            Kind::Value => {
                let value = match inline {
                    Some(v) if v.is_empty() => {
                        return Err(OptionError::bad_usage(format!(
                            "{argv0}: {arg} requires an argument"
                        )));
                    }
                    Some(v) => v,
                    None => {
                        let missing = || {
                            OptionError::bad_usage(format!("{argv0}: {arg} requires an argument"))
                        };
                        let v = args.get(i).ok_or_else(missing)?.clone();
                        if v.starts_with('-') {
                            return Err(missing());
                        }
                        i += 1;
                        if !from_env {
                            parsed.exec_argv.push(v.clone());
                        }
                        v
                    }
                };
                if resolved == "--eval" {
                    parsed.eval = Some(value.strip_prefix('\\').filter(|r| r.starts_with('-')).unwrap_or(&value).to_string());
                    set(parsed, &resolved, Value::Str(parsed.eval.clone().unwrap()));
                } else {
                    set(parsed, &resolved, Value::Str(value));
                }
            }
        }
    }
    Ok(())
}

/// `NODE_OPTIONS` split into arguments: whitespace separated, a double-quoted run keeps its
/// spaces and `\` escapes the next character inside it.
pub fn split_node_options(s: &str) -> Result<Vec<String>, OptionError> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_token = false;
    let mut chars = s.chars().peekable();
    let mut quoted = false;
    while let Some(c) = chars.next() {
        if quoted {
            match c {
                '"' => quoted = false,
                '\\' => {
                    if let Some(n) = chars.next() {
                        cur.push(n);
                    }
                }
                c => cur.push(c),
            }
        } else if c == '"' {
            quoted = true;
            in_token = true;
        } else if c == ' ' || c == '\t' {
            if in_token {
                out.push(std::mem::take(&mut cur));
                in_token = false;
            }
        } else {
            cur.push(c);
            in_token = true;
        }
    }
    if quoted {
        return Err(OptionError::bad_usage(
            "invalid value for NODE_OPTIONS (unterminated string)".to_string(),
        ));
    }
    if in_token {
        out.push(cur);
    }
    Ok(out)
}
