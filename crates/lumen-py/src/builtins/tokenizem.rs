//! `_tokenize`: the tokenizer behind the `tokenize` module, on the engine's lexer
//! ([`crate::lexer::tokenize_extra`]).

#[lumen_bind::module(name = "_tokenize")]
pub mod _tokenize {
    #![allow(clippy::new_ret_no_self)]
    use crate::bind::NativeIter;
    use crate::lexer::{tokenize_extra, RawToken, Tok};
    use crate::object::*;
    use crate::parser::SyntaxError;
    use crate::vm::Interp;

    const ENDMARKER: i64 = 0;
    const NAME: i64 = 1;
    const NUMBER: i64 = 2;
    const STRING: i64 = 3;
    const NEWLINE: i64 = 4;
    const INDENT: i64 = 5;
    const DEDENT: i64 = 6;
    const OP: i64 = 55;
    const FSTRING_START: i64 = 59;
    const FSTRING_MIDDLE: i64 = 60;
    const FSTRING_END: i64 = 61;
    #[allow(dead_code)]
    const TSTRING_START: i64 = 62;
    #[allow(dead_code)]
    const TSTRING_MIDDLE: i64 = 63;
    #[allow(dead_code)]
    const TSTRING_END: i64 = 64;
    const COMMENT: i64 = 65;
    const NL: i64 = 66;
    const ERRORTOKEN: i64 = 67;

    /// `token.EXACT_TOKEN_TYPES`.
    const EXACT: &[(&str, i64)] = &[
        ("(", 7), (")", 8), ("[", 9), ("]", 10), (":", 11), (",", 12), (";", 13), ("+", 14), ("-", 15),
        ("*", 16), ("/", 17), ("|", 18), ("&", 19), ("<", 20), (">", 21), ("=", 22), (".", 23), ("%", 24),
        ("{", 25), ("}", 26), ("==", 27), ("!=", 28), ("<=", 29), (">=", 30), ("~", 31), ("^", 32),
        ("<<", 33), (">>", 34), ("**", 35), ("+=", 36), ("-=", 37), ("*=", 38), ("/=", 39), ("%=", 40),
        ("&=", 41), ("|=", 42), ("^=", 43), ("<<=", 44), (">>=", 45), ("**=", 46), ("//", 47),
        ("//=", 48), ("@", 49), ("@=", 50), ("->", 51), ("...", 52), (":=", 53), ("!", 54),
    ];

    fn token_type(t: &Tok, extra: bool) -> Option<i64> {
        Some(match t {
            Tok::Name(_) | Tok::Kw(_) => NAME,
            Tok::Op(_) if extra => OP,
            Tok::Op(o) => EXACT.iter().find(|(s, _)| s == o).map_or(OP, |&(_, n)| n),
            Tok::Int(_) | Tok::Float(_) | Tok::Imag(_) => NUMBER,
            Tok::Str(_) | Tok::Bytes(_) | Tok::FStr { .. } => STRING,
            Tok::Newline => NEWLINE,
            Tok::Indent => INDENT,
            Tok::Dedent => DEDENT,
            Tok::EndMarker => ENDMARKER,
            Tok::Comment if extra => COMMENT,
            Tok::Nl if extra => NL,
            Tok::Comment | Tok::Nl => return None,
            Tok::FStart => FSTRING_START,
            Tok::FMiddle => FSTRING_MIDDLE,
            Tok::FEnd => FSTRING_END,
            Tok::Error if extra => OP,
            Tok::Error => ERRORTOKEN,
        })
    }

    /// The physical lines `first..=last` (1-based) of `lines`, joined.
    fn lines_of(lines: &[&str], first: u32, last: u32) -> String {
        let (a, b) = (first as usize, (last as usize).min(lines.len()));
        if a == 0 || a > b {
            return String::new();
        }
        lines[a - 1..b].concat()
    }

    fn tuple(rt: &RawToken, ty: i64, lines: &[&str]) -> Value {
        let end = rt.end;
        let pos = |(l, c): (u32, u32)| Value::tuple(vec![Value::Int(l as i64), Value::Int(c as i64)]);
        let line = match rt.tok {
            Tok::EndMarker => String::new(),
            _ => lines_of(lines, rt.start.0, end.0),
        };
        let (mut text, mut end) = (rt.text.clone(), end);
        // The lexer sees `\r\n` as `\n`; `tokenize` reports the newline as written.
        if ty == NEWLINE && text == "\n" && line.ends_with("\r\n") {
            text = "\r\n".into();
            end.1 += 1;
        }
        Value::tuple(vec![Value::Int(ty), Value::string(text), pos(rt.start), pos(end), Value::string(line)])
    }

    fn syntax_error(it: &mut Interp, e: &SyntaxError, lines: &[&str]) -> Obj {
        if e.msg == "unexpected EOF in multi-line statement" && e.col == 0 {
            let exc = it.new_exc_str("SyntaxError", &e.msg);
            it.set_exc_attr(&exc, "msg", Value::str(&e.msg));
            it.set_exc_attr(&exc, "lineno", Value::Int(e.line as i64));
            it.set_exc_attr(&exc, "offset", Value::Int(0));
            return exc;
        }
        let kind = crate::import::syntax_error_kind(&e.msg);
        let text = lines_of(lines, e.line, e.line);
        let detail = Value::tuple(vec![
            Value::str("<string>"),
            Value::Int(e.line as i64),
            Value::Int(e.col as i64 + 1),
            Value::string(text.trim_end_matches('\n').to_string()),
            Value::None,
            Value::None,
        ]);
        let cls = Value::Obj(it.exc_type(kind));
        match it.call(&cls, vec![Value::str(&e.msg), detail], Vec::new()) {
            Ok(Value::Obj(o)) => o,
            Ok(_) => it.new_exc_str(kind, &e.msg),
            Err(err) => err,
        }
    }

    /// The source `readline` returns, line by line until it is exhausted.
    fn read_source(it: &mut Interp, readline: &Value, encoding: Option<&str>) -> R<String> {
        let mut src = String::new();
        loop {
            let line = match it.call(readline, Vec::new(), Vec::new()) {
                Ok(l) => l,
                Err(e) if it.exc_is(&e, "StopIteration") => break,
                Err(e) => return Err(e),
            };
            let line = match encoding {
                Some(enc) => {
                    let decode = it.get_attr_str(&line, "decode")?;
                    it.call(&decode, vec![Value::str(enc)], Vec::new())?
                }
                None => line,
            };
            let Some(s) = line.as_str() else {
                return Err(it.type_error("readline() returned a non-string object"));
            };
            if s.is_empty() {
                break;
            }
            src.push_str(s);
        }
        Ok(src)
    }

    #[class(name = "TokenizerIter", hint(py(native_iter)))]
    pub struct TokenizerIter;

    #[methods]
    impl TokenizerIter {
        #[constructor]
        fn new(it: &mut Interp, readline: &Value, #[kwonly] extra_tokens: bool, #[kwonly] encoding: Option<&str>) -> R<NativeIter> {
            let src = read_source(it, readline, encoding)?;
            let (toks, err) = tokenize_extra(&src);
            let lines: Vec<String> = src.split_inclusive('\n').map(str::to_string).collect();
            let mut toks = toks.into_iter();
            let mut err = err;
            Ok(NativeIter::new(move |it| {
                let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
                for rt in toks.by_ref() {
                    if let Some(ty) = token_type(&rt.tok, extra_tokens) {
                        return Ok(Some(tuple(&rt, ty, &refs)));
                    }
                }
                match err.take() {
                    Some(e) => Err(syntax_error(it, &e, &refs)),
                    None => Ok(None),
                }
            }))
        }
    }
}
