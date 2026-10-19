//! The slice of `rust-url` the pattern code uses, over Lumen's WHATWG parser in
//! `lumen_common::url` (so patterns canonicalize exactly as the `URL` class parses).

use core::fmt;
use core::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
  InvalidDomainCharacter,
  InvalidIpv6Address,
  InvalidPort,
  InvalidUrl,
}

impl fmt::Display for ParseError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(match self {
      ParseError::InvalidDomainCharacter => "invalid domain character",
      ParseError::InvalidIpv6Address => "invalid IPv6 address",
      ParseError::InvalidPort => "invalid port number",
      ParseError::InvalidUrl => "invalid URL",
    })
  }
}

impl std::error::Error for ParseError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url(lumen_common::url::Url);

impl Url {
  pub fn parse(input: &str) -> Result<Url, ParseError> {
    Url::parse_with_base(input, None)
  }

  pub fn parse_with_base(
    input: &str,
    base: Option<&Url>,
  ) -> Result<Url, ParseError> {
    lumen_common::url::parse_url(input, base.map(|base| &base.0))
      .map(Url)
      .ok_or(ParseError::InvalidUrl)
  }

  pub fn scheme(&self) -> &str {
    &self.0.scheme
  }

  pub fn username(&self) -> &str {
    &self.0.username
  }

  pub fn password(&self) -> Option<&str> {
    Some(self.0.password.as_str()).filter(|password| !password.is_empty())
  }

  pub fn host_str(&self) -> Option<&str> {
    self.0.host.as_deref()
  }

  pub fn query(&self) -> Option<&str> {
    self.0.query.as_deref()
  }

  pub fn fragment(&self) -> Option<&str> {
    self.0.fragment.as_deref()
  }

  pub fn cannot_be_a_base(&self) -> bool {
    self.0.opaque
  }

  pub fn set_username(&mut self, username: &str) -> Result<(), ()> {
    self.0.set_username(username).then_some(()).ok_or(())
  }

  pub fn set_password(&mut self, password: Option<&str>) -> Result<(), ()> {
    self
      .0
      .set_password(password.unwrap_or_default())
      .then_some(())
      .ok_or(())
  }

  /// Sets the path. An opaque path is the input percent-encoded with the C0
  /// control set, as the spec's "canonicalize an opaque pathname" does.
  pub fn set_path(&mut self, path: &str) {
    if !self.0.opaque {
      self.0.set_pathname(path);
      return;
    }
    let mut encoded = String::with_capacity(path.len());
    for byte in path.bytes() {
      if byte < 0x20 || byte > 0x7e {
        encoded.push_str(&format!("%{byte:02X}"));
      } else {
        encoded.push(byte as char);
      }
    }
    self.0.path = encoded;
  }

  /// Sets the query without treating a leading `?` as the delimiter.
  pub fn set_query(&mut self, query: Option<&str>) {
    match query {
      Some(query) => self.0.set_search(&format!("?{query}")),
      None => self.0.set_search(""),
    }
  }

  /// Sets the fragment without treating a leading `#` as the delimiter.
  pub fn set_fragment(&mut self, fragment: Option<&str>) {
    match fragment {
      Some(fragment) => self.0.set_hash(&format!("#{fragment}")),
      None => self.0.set_hash(""),
    }
  }
}

impl FromStr for Url {
  type Err = ParseError;

  fn from_str(input: &str) -> Result<Url, ParseError> {
    Url::parse(input)
  }
}

/// The URL API's string views of a URL.
pub mod quirks {
  use super::Url;

  pub fn hostname(url: &Url) -> &str {
    url.0.hostname()
  }

  pub fn port(url: &Url) -> String {
    url.0.port.map(|port| port.to_string()).unwrap_or_default()
  }

  pub fn pathname(url: &Url) -> &str {
    &url.0.path
  }

  pub fn set_hostname(url: &mut Url, hostname: &str) -> Result<(), ()> {
    url.0.set_hostname(hostname).then_some(()).ok_or(())
  }

  pub fn set_port(url: &mut Url, port: &str) -> Result<(), ()> {
    url.0.set_port(port).then_some(()).ok_or(())
  }
}
