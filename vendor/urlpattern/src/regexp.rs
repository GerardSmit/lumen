use std::cell::OnceCell;

use lumen_common::regex::ExecOptions;
use lumen_common::regex::Regex;
use lumen_common::regex::js;

/// The regular expression a component compiles to.
pub trait RegExp: Sized {
  /// Generates a regexp pattern for the given string. If the pattern is
  /// invalid, the parse function should return an error. `force_eval` asks for
  /// the pattern to be validated now rather than on first match.
  #[allow(clippy::result_unit_err)]
  fn parse(pattern: &str, flags: &str, force_eval: bool) -> Result<Self, ()>;

  /// Matches the given text against the regular expression and returns the list
  /// of captures. The matches are returned in the order they appear in the
  /// regular expression. It is **not** prefixed with the full match. For groups
  /// that occur in the regular expression, but did not match, the corresponding
  /// capture should be `None`.
  ///
  /// Returns `None` if the text does not match the regular expression.
  fn matches<'a>(&self, text: &'a str) -> Option<Vec<Option<&'a str>>>;

  fn pattern_string(&self) -> &str;
}

/// An ECMAScript regular expression run by Lumen's own engine
/// (`lumen_common::regex`), so a pattern means what the same `RegExp` means in
/// script. Compilation is deferred until the first match unless forced:
/// components that need no regular expression never pay for one.
pub struct EcmaRegExp {
  pattern: String,
  flags: String,
  compiled: OnceCell<Option<Regex>>,
}

impl std::fmt::Debug for EcmaRegExp {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("EcmaRegExp")
      .field("pattern", &self.pattern)
      .field("flags", &self.flags)
      .finish()
  }
}

fn compile(pattern: &str, flags: &str) -> Result<Regex, ()> {
  let flags = js::Flags::parse(flags).map_err(|_| ())?;
  js::compile(pattern.chars().collect(), &flags).map_err(|_| ())
}

impl EcmaRegExp {
  fn regex(&self) -> Option<&Regex> {
    self
      .compiled
      .get_or_init(|| compile(&self.pattern, &self.flags).ok())
      .as_ref()
  }
}

impl RegExp for EcmaRegExp {
  fn parse(pattern: &str, flags: &str, force_eval: bool) -> Result<Self, ()> {
    let compiled = OnceCell::new();
    if force_eval {
      let _ = compiled.set(Some(compile(pattern, flags)?));
    }
    Ok(EcmaRegExp {
      pattern: pattern.to_owned(),
      flags: flags.to_owned(),
      compiled,
    })
  }

  fn matches<'a>(&self, text: &'a str) -> Option<Vec<Option<&'a str>>> {
    let regex = self.regex()?;
    let options = ExecOptions::search(0);
    if text.is_ascii() {
      let captures = regex.exec(text.as_bytes(), options).ok()??;
      return Some(
        captures[1..]
          .iter()
          .map(|span| span.map(|(start, end)| &text[start..end]))
          .collect(),
      );
    }
    let mut elements = Vec::new();
    let mut offsets = Vec::new();
    for (offset, c) in text.char_indices() {
      offsets.push(offset);
      elements.push(c as u32);
    }
    offsets.push(text.len());
    let captures = regex.exec(&elements[..], options).ok()??;
    Some(
      captures[1..]
        .iter()
        .map(|span| span.map(|(start, end)| &text[offsets[start]..offsets[end]]))
        .collect(),
    )
  }

  fn pattern_string(&self) -> &str {
    &self.pattern
  }
}
