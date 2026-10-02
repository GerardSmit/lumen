//! CSV reading and writing as CPython's `_csv` module does it: the record parser's state machine
//! and the writer's quoting rules, over Unicode scalar values.

use std::fmt;

/// When the writer quotes a field (`csv.QUOTE_*`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Quoting {
    Minimal = 0,
    All = 1,
    NonNumeric = 2,
    None = 3,
    Strings = 4,
    NotNull = 5,
}

impl Quoting {
    pub fn from_i64(n: i64) -> Option<Quoting> {
        Some(match n {
            0 => Quoting::Minimal,
            1 => Quoting::All,
            2 => Quoting::NonNumeric,
            3 => Quoting::None,
            4 => Quoting::Strings,
            5 => Quoting::NotNull,
            _ => return None,
        })
    }

    pub const NAMES: [(&'static str, Quoting); 6] = [
        ("QUOTE_MINIMAL", Quoting::Minimal),
        ("QUOTE_ALL", Quoting::All),
        ("QUOTE_NONNUMERIC", Quoting::NonNumeric),
        ("QUOTE_NONE", Quoting::None),
        ("QUOTE_STRINGS", Quoting::Strings),
        ("QUOTE_NOTNULL", Quoting::NotNull),
    ];
}

#[derive(Clone, Debug)]
pub struct Dialect {
    pub delimiter: char,
    pub quotechar: Option<char>,
    pub escapechar: Option<char>,
    pub doublequote: bool,
    pub skipinitialspace: bool,
    pub strict: bool,
    pub quoting: Quoting,
    pub lineterminator: String,
}

impl Default for Dialect {
    fn default() -> Self {
        Dialect {
            delimiter: ',',
            quotechar: Some('"'),
            escapechar: None,
            doublequote: true,
            skipinitialspace: false,
            strict: false,
            quoting: Quoting::Minimal,
            lineterminator: "\r\n".to_string(),
        }
    }
}

/// A malformed record or a field the writer cannot represent; `Display` gives CPython's message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    FieldLimit(i64),
    QuoteExpected { delimiter: char, quotechar: char },
    NewlineInUnquoted,
    UnexpectedEnd,
    NeedEscape,
    EmptyFieldWithSpace,
    SingleEmptyField,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::FieldLimit(n) => write!(f, "field larger than field limit ({n})"),
            Error::QuoteExpected { delimiter, quotechar } => write!(f, "'{delimiter}' expected after '{quotechar}'"),
            Error::NewlineInUnquoted => f.write_str("new-line character seen in unquoted field - do you need to open the file with newline=''?"),
            Error::UnexpectedEnd => f.write_str("unexpected end of data"),
            Error::NeedEscape => f.write_str("need to escape, but no escapechar set"),
            Error::EmptyFieldWithSpace => f.write_str("empty field must be quoted if delimiter is a space and skipinitialspace is true"),
            Error::SingleEmptyField => f.write_str("single empty field record must be quoted"),
        }
    }
}

/// One parsed field; `numeric` marks an unquoted field under `QUOTE_NONNUMERIC`, which the
/// reader converts to a float.
#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    pub text: String,
    pub numeric: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    StartRecord,
    StartField,
    EscapedChar,
    InField,
    InQuotedField,
    EscapeInQuotedField,
    QuoteInQuotedField,
    EatCrnl,
    AfterEscapedCrnl,
}

/// The end of an input line, fed after its characters.
const EOL: Option<char> = None;

/// Splits input lines into records; a quoted field may span lines.
#[derive(Debug)]
pub struct Parser {
    state: State,
    fields: Vec<Field>,
    field: String,
    field_len: i64,
    numeric: bool,
}

impl Default for Parser {
    fn default() -> Self {
        Parser { state: State::StartRecord, fields: Vec::new(), field: String::new(), field_len: 0, numeric: false }
    }
}

impl Parser {
    /// Starts a new record, dropping any partial one.
    pub fn reset(&mut self) {
        *self = Parser::default();
    }

    /// Whether the last line completed a record.
    pub fn record_done(&self) -> bool {
        self.state == State::StartRecord
    }

    /// Feeds one input line (with its line ending, if any).
    pub fn feed_line(&mut self, d: &Dialect, line: &str, field_limit: i64) -> Result<(), Error> {
        for c in line.chars() {
            self.process(d, Some(c), field_limit)?;
        }
        self.process(d, EOL, field_limit)
    }

    /// At the end of the input: whether a partial record remains to be returned.
    pub fn finish(&mut self, d: &Dialect) -> Result<bool, Error> {
        if self.field_len == 0 && self.state != State::InQuotedField {
            return Ok(false);
        }
        if d.strict {
            return Err(Error::UnexpectedEnd);
        }
        self.save_field();
        Ok(true)
    }

    pub fn take_fields(&mut self) -> Vec<Field> {
        std::mem::take(&mut self.fields)
    }

    fn save_field(&mut self) {
        let text = std::mem::take(&mut self.field);
        self.fields.push(Field { text, numeric: self.numeric });
        self.field_len = 0;
        self.numeric = false;
    }

    fn add_char(&mut self, c: char, limit: i64) -> Result<(), Error> {
        if self.field_len >= limit {
            return Err(Error::FieldLimit(limit));
        }
        self.field.push(c);
        self.field_len += 1;
        Ok(())
    }

    fn process(&mut self, d: &Dialect, c: Option<char>, limit: i64) -> Result<(), Error> {
        let is_nl = |c: Option<char>| matches!(c, Some('\n' | '\r'));
        let is_quote = |c: Option<char>| c.is_some() && c == d.quotechar && d.quoting != Quoting::None;
        let is_escape = |c: Option<char>| c.is_some() && c == d.escapechar;
        let is_delim = |c: Option<char>| c == Some(d.delimiter);
        let end_state = |c: Option<char>| if c == EOL { State::StartRecord } else { State::EatCrnl };
        let mut state = self.state;
        if state == State::StartRecord {
            if c == EOL {
                return Ok(());
            }
            if is_nl(c) {
                self.state = State::EatCrnl;
                return Ok(());
            }
            state = State::StartField;
            self.state = state;
        }
        if state == State::AfterEscapedCrnl {
            if c == EOL {
                return Ok(());
            }
            state = State::InField;
        }
        match state {
            State::StartField => {
                if is_nl(c) || c == EOL {
                    self.save_field();
                    self.state = end_state(c);
                } else if is_quote(c) {
                    self.state = State::InQuotedField;
                } else if is_escape(c) {
                    if d.quoting == Quoting::NonNumeric {
                        self.numeric = true;
                    }
                    self.state = State::EscapedChar;
                } else if c == Some(' ') && d.skipinitialspace {
                } else if is_delim(c) {
                    self.save_field();
                } else {
                    if d.quoting == Quoting::NonNumeric {
                        self.numeric = true;
                    }
                    self.add_char(c.unwrap_or('\n'), limit)?;
                    self.state = State::InField;
                }
            }
            State::EscapedChar => {
                if is_nl(c) {
                    self.add_char(c.unwrap_or('\n'), limit)?;
                    self.state = State::AfterEscapedCrnl;
                } else {
                    self.add_char(c.unwrap_or('\n'), limit)?;
                    self.state = State::InField;
                }
            }
            State::InField => {
                if is_nl(c) || c == EOL {
                    self.save_field();
                    self.state = end_state(c);
                } else if is_escape(c) {
                    self.state = State::EscapedChar;
                } else if is_delim(c) {
                    self.save_field();
                    self.state = State::StartField;
                } else {
                    self.add_char(c.unwrap_or('\n'), limit)?;
                    self.state = State::InField;
                }
            }
            State::InQuotedField => {
                if c == EOL {
                } else if is_escape(c) {
                    self.state = State::EscapeInQuotedField;
                } else if is_quote(c) {
                    self.state = if d.doublequote { State::QuoteInQuotedField } else { State::InField };
                } else {
                    self.add_char(c.unwrap_or('\n'), limit)?;
                }
            }
            State::EscapeInQuotedField => {
                self.add_char(c.unwrap_or('\n'), limit)?;
                self.state = State::InQuotedField;
            }
            State::QuoteInQuotedField => {
                if is_quote(c) {
                    self.add_char(c.unwrap_or('\n'), limit)?;
                    self.state = State::InQuotedField;
                } else if is_delim(c) {
                    self.save_field();
                    self.state = State::StartField;
                } else if is_nl(c) || c == EOL {
                    self.save_field();
                    self.state = end_state(c);
                } else if !d.strict {
                    self.add_char(c.unwrap_or('\n'), limit)?;
                    self.state = State::InField;
                } else {
                    return Err(Error::QuoteExpected { delimiter: d.delimiter, quotechar: d.quotechar.unwrap_or('"') });
                }
            }
            State::EatCrnl => {
                if is_nl(c) {
                } else if c == EOL {
                    self.state = State::StartRecord;
                } else {
                    return Err(Error::NewlineInUnquoted);
                }
            }
            State::StartRecord | State::AfterEscapedCrnl => unreachable!(),
        }
        Ok(())
    }
}

/// Builds one output record field by field.
#[derive(Default, Debug)]
pub struct RowWriter {
    rec: String,
    num_fields: usize,
    last_null: bool,
}

impl RowWriter {
    /// Appends a field (`None` for a null field) that the caller decided to quote or not; the
    /// field is quoted anyway when it contains a special character.
    pub fn append(&mut self, d: &Dialect, field: Option<&str>, quoted: bool) -> Result<(), Error> {
        self.last_null = field.is_none();
        self.append_field(d, field, quoted)
    }

    fn append_field(&mut self, d: &Dialect, field: Option<&str>, mut quoted: bool) -> Result<(), Error> {
        let text = field.unwrap_or("");
        if text.is_empty() && d.delimiter == ' ' && d.skipinitialspace {
            if d.quoting == Quoting::None || (field.is_none() && matches!(d.quoting, Quoting::Strings | Quoting::NotNull)) {
                return Err(Error::EmptyFieldWithSpace);
            }
            quoted = true;
        }
        let mut body = String::with_capacity(text.len());
        for c in text.chars() {
            let special = c == d.delimiter
                || Some(c) == d.escapechar
                || Some(c) == d.quotechar
                || c == '\n'
                || c == '\r'
                || d.lineterminator.contains(c);
            if special {
                let mut want_escape = false;
                if d.quoting == Quoting::None {
                    want_escape = true;
                } else {
                    if Some(c) == d.quotechar {
                        if d.doublequote {
                            body.push(c);
                        } else {
                            want_escape = true;
                        }
                    } else if Some(c) == d.escapechar {
                        want_escape = true;
                    }
                    if !want_escape {
                        quoted = true;
                    }
                }
                if want_escape {
                    body.push(d.escapechar.ok_or(Error::NeedEscape)?);
                }
            }
            body.push(c);
        }
        if self.num_fields > 0 {
            self.rec.push(d.delimiter);
        }
        let quote = d.quotechar.filter(|_| quoted);
        self.rec.extend(quote);
        self.rec.push_str(&body);
        self.rec.extend(quote);
        self.num_fields += 1;
        Ok(())
    }

    /// The finished record with its line terminator; the writer is reset for the next one.
    pub fn finish(&mut self, d: &Dialect) -> Result<String, Error> {
        if self.num_fields > 0 && self.rec.is_empty() {
            if d.quoting == Quoting::None || (self.last_null && matches!(d.quoting, Quoting::Strings | Quoting::NotNull)) {
                return Err(Error::SingleEmptyField);
            }
            self.num_fields -= 1;
            self.append_field(d, None, true)?;
        }
        let mut line = std::mem::take(&mut self.rec);
        line.push_str(&d.lineterminator);
        self.num_fields = 0;
        self.last_null = false;
        Ok(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(d: &Dialect, lines: &[&str]) -> Vec<Vec<String>> {
        let mut p = Parser::default();
        let mut out = Vec::new();
        for l in lines {
            p.feed_line(d, l, 131072).unwrap();
            if p.record_done() {
                out.push(p.take_fields().into_iter().map(|f| f.text).collect());
                p.reset();
            }
        }
        out
    }

    #[test]
    fn parses_quoted_fields_across_lines() {
        let d = Dialect::default();
        assert_eq!(parse(&d, &["a,\"b\"\"c\",d\r\n", "\"x\n", "y\",z\n"]), vec![vec!["a", "b\"c", "d"], vec!["x\ny", "z"]]);
        assert_eq!(parse(&d, &["\n"]), vec![Vec::<String>::new()]);
    }

    #[test]
    fn writes_minimal_quoting() {
        let d = Dialect::default();
        let mut w = RowWriter::default();
        for f in ["a", "b,c", "q\"q", ""] {
            w.append(&d, Some(f), false).unwrap();
        }
        assert_eq!(w.finish(&d).unwrap(), "a,\"b,c\",\"q\"\"q\",\r\n");
        w.append(&d, Some(""), false).unwrap();
        assert_eq!(w.finish(&d).unwrap(), "\"\"\r\n");
    }
}
