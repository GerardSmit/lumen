//! Bounded HTTP cookie storage. URLs, site boundaries and calendar arithmetic
//! are shared algorithms; clocks and persistence belong to the embedding host.
use crate::url::Url;

const MAX_COOKIE_BYTES: usize = 4096;
const MAX_COOKIE_FIELD_BYTES: usize = 64 * 1024;
const MAX_ATTRIBUTE_BYTES: usize = 1024;
const MAX_COOKIES: usize = 3000;
const MAX_SITE_COOKIES: usize = 180;
const MAX_JAR_BYTES: usize = 8 * 1024 * 1024;
const MAX_AGE: i64 = 400 * 86400;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SameSite {
    Default,
    Lax,
    Strict,
    None,
}

/// Trusted context for cookie retrieval/storage, never supplied by a page's
/// request headers. `site` is the schemeful top-level site serialized by site().
#[derive(Clone, Debug)]
pub struct Context {
    pub site: Option<String>,
    pub top_level_navigation: bool,
    pub safe_method: bool,
}
impl Context {
    pub fn document(url: &Url) -> Self {
        Self {
            site: site(url),
            top_level_navigation: false,
            safe_method: true,
        }
    }
}

#[derive(Clone, Debug)]
struct Cookie {
    name: String,
    value: String,
    domain: String,
    path: String,
    host_only: bool,
    secure: bool,
    http_only: bool,
    same_site: SameSite,
    partition: Option<String>,
    expires: Option<i64>,
    created: i64,
    sequence: u64,
    accessed: u64,
}
impl Cookie {
    fn matches_url(&self, url: &Url, context: &Context) -> bool {
        (if self.host_only { url.hostname() == self.domain }
         else { domain_match(url.hostname(), &self.domain) })
            && path_match(&url.path, &self.path)
            && (!self.secure || secure(url))
            && (self.partition.is_none() || self.partition == context.site)
    }
    fn bytes(&self) -> usize {
        self.name.len()
            + self.value.len()
            + self.domain.len()
            + self.path.len()
            + self.partition.as_ref().map_or(0, String::len)
    }
}

#[derive(Default)]
pub struct CookieJar {
    entries: Vec<Cookie>,
    sequence: u64,
}

pub fn site(url: &Url) -> Option<String> {
    if !matches!(url.scheme.as_str(), "http" | "https") {
        return None;
    }
    let host = url.hostname();
    if host.is_empty() {
        return None;
    }
    Some(format!("{}://{}", url.scheme, registrable_host(host)))
}
fn registrable_host(host: &str) -> &str {
    if host.parse::<core::net::IpAddr>().is_ok() || host.starts_with('[') {
        host
    } else {
        psl::domain_str(host).unwrap_or(host)
    }
}
fn secure(url: &Url) -> bool {
    url.scheme == "https" || url.hostname() == "localhost" || url.hostname().ends_with(".localhost")
}
fn domain_match(host: &str, domain: &str) -> bool {
    host == domain
        || (host.parse::<core::net::IpAddr>().is_err()
            && !host.starts_with('[')
            && host
                .strip_suffix(domain)
                .is_some_and(|prefix| prefix.ends_with('.')))
}
fn path_match(path: &str, scope: &str) -> bool {
    path == scope
        || path
            .strip_prefix(scope)
            .is_some_and(|tail| scope.ends_with('/') || tail.starts_with('/'))
}
fn default_path(url: &Url) -> String {
    match url.path.rfind('/') {
        Some(end) if end > 0 => url.path[..end].to_owned(),
        _ => "/".into(),
    }
}
fn canonical_domain(raw: &str) -> Option<String> {
    let raw = raw.strip_prefix('.').unwrap_or(raw);
    if raw.is_empty()
        || raw.ends_with('.')
        || raw
            .bytes()
            .any(|byte| byte <= 32 || matches!(byte, b'/' | b'\\' | b'@' | b':' | b'?' | b'#'))
    {
        return None;
    }
    let url = crate::url::parse(&format!("https://{raw}/"), None).ok()?;
    (!url.hostname().is_empty()).then(|| url.hostname().to_owned())
}
fn same_site_allowed(cookie: &Cookie, url: &Url, context: &Context, now: i64) -> bool {
    if context.site.is_some() && context.site == site(url) {
        return true;
    }
    match cookie.same_site {
        SameSite::None => true,
        SameSite::Strict => false,
        SameSite::Lax => context.top_level_navigation && context.safe_method,
        SameSite::Default => {
            context.top_level_navigation
                && (context.safe_method || now.saturating_sub(cookie.created) <= 120)
        }
    }
}

impl CookieJar {
    fn purge(&mut self, now: i64) {
        self.entries
            .retain(|cookie| cookie.expires.is_none_or(|end| end > now));
    }

    /// Browser automation deletes associated HTTP cookies, including HttpOnly cookies.
    /// Preserve unrelated hosts, paths and partition keys in the same profile jar.
    pub fn delete_associated(&mut self, url: &Url, context: &Context, now: i64) {
        self.purge(now);
        if site(url).is_some() {
            self.entries.retain(|cookie| !cookie.matches_url(url, context));
        }
    }

    /// Store one Set-Cookie field or one non-HTTP document.cookie assignment.
    /// Invalid cookies are ignored, matching the cookie storage algorithm.
    pub fn store(
        &mut self,
        url: &Url,
        field: &str,
        from_http: bool,
        context: &Context,
        now: i64,
    ) -> bool {
        self.purge(now);
        if site(url).is_none()
            || field.len() > MAX_COOKIE_FIELD_BYTES
            || field
                .bytes()
                .any(|byte| (byte < 32 && byte != b'\t') || byte == 127)
        {
            return false;
        }
        let mut parts = field.split(';');
        let pair = parts.next().unwrap_or("").trim_matches([' ', '\t']);
        let (name, value) = pair.split_once('=').unwrap_or(("", pair));
        let name = name.trim_matches([' ', '\t']);
        let value = value.trim_matches([' ', '\t']);
        if (name.is_empty() && value.is_empty())
            || name.len().saturating_add(value.len()) > MAX_COOKIE_BYTES {
            return false;
        }
        let mut cookie = Cookie {
            name: name.into(),
            value: value.into(),
            domain: url.hostname().into(),
            path: default_path(url),
            host_only: true,
            secure: false,
            http_only: false,
            same_site: SameSite::Default,
            partition: None,
            expires: None,
            created: now,
            sequence: 0,
            accessed: 0,
        };
        let mut domain_attribute = None;
        let mut root_path_attribute = false;
        let mut partitioned = false;
        let mut max_age = None;
        for part in parts {
            let (attribute, value) = part
                .trim_matches([' ', '\t'])
                .split_once('=')
                .unwrap_or((part.trim_matches([' ', '\t']), ""));
            let value = value.trim_matches([' ', '\t']);
            if value.len() > MAX_ATTRIBUTE_BYTES { continue; }
            match attribute
                .trim_matches([' ', '\t'])
                .to_ascii_lowercase()
                .as_str()
            {
                "domain" => {
                    if value.is_empty() {
                        continue;
                    }
                    domain_attribute = Some(value);
                }
                "path" => {
                    cookie.path = if value.starts_with('/') {
                        value.into()
                    } else {
                        default_path(url)
                    };
                    root_path_attribute = value == "/";
                }
                "secure" => cookie.secure = true,
                "httponly" => cookie.http_only = true,
                "partitioned" => partitioned = true,
                "samesite" => {
                    cookie.same_site = match value.to_ascii_lowercase().as_str() {
                        "strict" => SameSite::Strict,
                        "lax" => SameSite::Lax,
                        "none" => SameSite::None,
                        _ => SameSite::Default,
                    }
                }
                "max-age" => {
                    let digits = value.strip_prefix('-').unwrap_or(value);
                    if !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()) {
                        max_age = Some(if value.starts_with('-') {
                            0
                        } else {
                            digits.parse::<i64>().unwrap_or(i64::MAX).min(MAX_AGE)
                        });
                    }
                }
                "expires" => {
                    if let Some(expiry) = cookie_date(value) {
                        cookie.expires = Some(expiry.min(now.saturating_add(MAX_AGE)));
                    }
                }
                _ => {}
            }
        }
        // Attributes are collected before storage validation: the last Domain
        // attribute determines scope even if an earlier one named another site.
        if let Some(raw) = domain_attribute {
            let Some(domain) = canonical_domain(raw) else {
                return false;
            };
            if !domain_match(url.hostname(), &domain) {
                return false;
            }
            let public_suffix = psl::suffix_str(&domain) == Some(domain.as_str());
            if public_suffix && domain != url.hostname() {
                return false;
            }
            cookie.host_only = public_suffix;
            cookie.domain = domain;
        }
        if let Some(age) = max_age {
            cookie.expires = Some(now.saturating_add(age));
        }
        if (cookie.secure && !secure(url))
            || (cookie.http_only && !from_http)
            || (cookie.same_site == SameSite::None && !cookie.secure)
        {
            return false;
        }
        if partitioned {
            if !cookie.secure {
                return false;
            }
            let Some(key) = context.site.clone() else {
                return false;
            };
            cookie.partition = Some(key);
        }
        let has_prefix = |value: &str, prefix: &str| {
            value.get(..prefix.len()).is_some_and(|head| head.eq_ignore_ascii_case(prefix))
        };
        if cookie.name.is_empty()
            && ["__Secure-", "__Host-", "__Http-"].iter().any(|prefix| has_prefix(&cookie.value, prefix))
        {
            return false;
        }
        if has_prefix(&cookie.name, "__Secure-") && !cookie.secure {
            return false;
        }
        if has_prefix(&cookie.name, "__Host-")
            && (!cookie.secure || domain_attribute.is_some() || !root_path_attribute)
        {
            return false;
        }
        if (has_prefix(&cookie.name, "__Http-") || has_prefix(&cookie.name, "__Host-Http-"))
            && (!cookie.secure || !cookie.http_only)
        {
            return false;
        }
        if cookie.same_site != SameSite::None
            && context.site != site(url)
            && !context.top_level_navigation
        {
            return false;
        }
        if !secure(url)
            && self.entries.iter().any(|old| {
                old.secure
                    && old.name == cookie.name
                    && old.partition == cookie.partition
                    && (domain_match(&old.domain, &cookie.domain)
                        || domain_match(&cookie.domain, &old.domain))
                    && path_match(&cookie.path, &old.path)
            })
        {
            return false;
        }
        let previous = self.entries.iter().position(|old| {
            old.name == cookie.name
                && old.domain == cookie.domain
                && old.path == cookie.path
                && old.partition == cookie.partition
        });
        if let Some(index) = previous {
            let old = &self.entries[index];
            if old.http_only && !from_http {
                return false;
            }
            cookie.created = old.created;
            cookie.sequence = old.sequence;
            self.entries.remove(index);
        } else {
            self.sequence = self.sequence.wrapping_add(1);
            cookie.sequence = self.sequence;
        }
        if cookie.expires.is_some_and(|end| end <= now) {
            return true;
        }
        self.sequence = self.sequence.wrapping_add(1);
        cookie.accessed = self.sequence;
        let scope = registrable_host(&cookie.domain).to_owned();
        self.entries.push(cookie);
        while self
            .entries
            .iter()
            .filter(|entry| registrable_host(&entry.domain) == scope)
            .count()
            > MAX_SITE_COOKIES
        {
            let index = self
                .entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| registrable_host(&entry.domain) == scope)
                .min_by_key(|(_, entry)| entry.accessed)
                .map(|(index, _)| index)
                .unwrap();
            self.entries.remove(index);
        }
        while self.entries.len() > MAX_COOKIES
            || self.entries.iter().map(Cookie::bytes).sum::<usize>() > MAX_JAR_BYTES
        {
            let index = self
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, entry)| entry.accessed)
                .map(|(index, _)| index)
                .unwrap();
            self.entries.remove(index);
        }
        true
    }

    pub fn header(&mut self, url: &Url, for_http: bool, context: &Context, now: i64) -> String {
        self.purge(now);
        if site(url).is_none() {
            return String::new();
        }
        self.sequence = self.sequence.wrapping_add(1);
        let access = self.sequence;
        let mut selected = self
            .entries
            .iter_mut()
            .filter(|cookie| {
                cookie.matches_url(url, context)
                    && (!cookie.http_only || for_http)
                    && (!for_http || same_site_allowed(cookie, url, context, now))
            })
            .collect::<Vec<_>>();
        selected.sort_by_key(|cookie| (std::cmp::Reverse(cookie.path.len()), cookie.sequence));
        for cookie in &mut selected {
            cookie.accessed = access;
        }
        selected
            .into_iter()
            .map(|cookie| if cookie.name.is_empty() { cookie.value.clone() }
                else { format!("{}={}", cookie.name, cookie.value) })
            .collect::<Vec<_>>()
            .join("; ")
    }
}

/// RFC cookie dates are tokenized independently of HTTP's strict IMF-fixdate.
/// The shared Gregorian implementation supplies epoch arithmetic.
fn cookie_date(value: &str) -> Option<i64> {
    let tokens = value.split(|ch: char| {
        ch == '\t' || matches!(ch as u32, 0x20..=0x2f | 0x3b..=0x40 | 0x5b..=0x60 | 0x7b..=0x7e)
    });
    let (mut time, mut day, mut month, mut year) = (None, None, None, None);
    for token in tokens.filter(|token| !token.is_empty()) {
        if time.is_none() {
            if let Some(parsed) = cookie_time(token) {
                time = Some(parsed);
                continue;
            }
        }
        if day.is_none() {
            if let Some(parsed) = date_number(token, 1, 2) {
                day = Some(parsed);
                continue;
            }
        }
        if month.is_none() && token.len() >= 3 {
            month = [
                "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
            ]
            .iter()
            .position(|month| token.as_bytes()[..3].eq_ignore_ascii_case(month.as_bytes()))
            .map(|month| month as i64 + 1);
            if month.is_some() {
                continue;
            }
        }
        if year.is_none() {
            year = date_number(token, 2, 4);
        }
    }
    let (time, day, month, mut year) = (time?, day?, month?, year?);
    if year <= 69 {
        year += 2000;
    } else if year <= 99 {
        year += 1900;
    }
    if year < 1601
        || day < 1
        || day > i64::from(crate::civil::days_in_month(year, month as u8))
        || time[0] > 23
        || time[1] > 59
        || time[2] > 59
    {
        return None;
    }
    Some(
        crate::civil::days_from_civil(year, month, day) * 86400
            + time[0] * 3600
            + time[1] * 60
            + time[2],
    )
}

// Cookie-date fields may have a trailing non-digit suffix. Count the entire
// initial digit run so an overlong field cannot be accepted by truncation.
fn date_number(token: &str, min: usize, max: usize) -> Option<i64> {
    let count = token.bytes().take_while(u8::is_ascii_digit).count();
    (min..=max)
        .contains(&count)
        .then(|| token[..count].parse().ok())
        .flatten()
}

fn cookie_time(token: &str) -> Option<[i64; 3]> {
    let mut remaining = token;
    let mut fields = [0; 3];
    for (index, field) in fields.iter_mut().enumerate() {
        let count = remaining.bytes().take_while(u8::is_ascii_digit).count();
        if !(1..=2).contains(&count) {
            return None;
        }
        *field = remaining[..count].parse().ok()?;
        remaining = &remaining[count..];
        if index < 2 {
            remaining = remaining.strip_prefix(':')?;
        }
    }
    Some(fields)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn url(value: &str) -> Url {
        crate::url::parse(value, None).unwrap()
    }

    #[test]
    fn cookie_loose_pairs_and_attribute_limits_preserve_storage_security() {
        let origin = url("https://app.example.com/account/page");
        let context = Context::document(&origin);
        let mut jar = CookieJar::default();
        assert!(jar.store(&origin, "test9; max-age=2.63,", true, &context, 100));
        assert_eq!(jar.header(&origin, false, &context, 100), "test9");
        assert!(jar.store(&origin, "test test=8", false, &context, 100));
        assert!(jar.store(&origin, "тест=2", false, &context, 100));
        assert!(!jar.store(&origin, "bad=\u{7f}", false, &context, 100));
        assert!(!jar.store(&origin, "=", false, &context, 100));
        let value = "1".repeat(4095);
        assert!(jar.store(&origin, &format!("t={value}; Path=/"), false, &context, 100));
        assert!(!jar.store(&origin, &format!("tt={value}; Path=/"), false, &context, 100));
        let invalid_path = format!("/{}", "a".repeat(1024));
        assert!(jar.store(&origin, &format!("path=1; Path=/account; Path={invalid_path}"), false, &context, 100));
        assert!(jar.header(&origin, false, &context, 100).contains("path=1"));
        assert!(jar.store(&origin, &format!("age=1; Max-Age=5; Max-Age=-{}", "1".repeat(1024)), false, &context, 100));
        assert!(jar.header(&origin, false, &context, 100).contains("age=1"));
        assert!(!jar.store(&origin, "__Host-secret=1; Domain=example.com; Secure; Path=/", true, &context, 100));
        for value in [
            "__secure-secret=1", "__hOsT-secret=1; Secure; Path=/; Domain=example.com",
            "__http-secret=1; Secure", "=__Secure-secret=1; Secure",
            "__HOST-secret; Secure; Path=/", "=__Http-secret=1; Secure; HttpOnly",
        ] {
            assert!(!jar.store(&origin, value, true, &context, 100), "{value}");
        }
        assert!(jar.store(&origin, "__hOsT-valid=1; Secure; Path=/", true, &context, 100));
    }

    #[test]
    fn automation_deletes_associated_http_cookies_without_clearing_other_scopes() {
        let current = url("https://app.example.com/account/page");
        let context = Context::document(&current);
        let other = url("https://other.test/account/page");
        let other_context = Context::document(&other);
        let mut jar = CookieJar::default();
        assert!(jar.store(&current, "session=secret; HttpOnly; Secure; Path=/", true, &context, 100));
        assert!(jar.store(&current, "theme=dark; Domain=example.com; Path=/account", false, &context, 100));
        assert!(jar.store(&current, "elsewhere=1; Path=/different", false, &context, 100));
        assert!(jar.store(&other, "other=1; Path=/", true, &other_context, 100));
        jar.delete_associated(&current, &context, 101);
        assert_eq!(jar.header(&current, true, &context, 101), "");
        assert_eq!(jar.header(&url("https://app.example.com/different/"), true, &context, 101), "elsewhere=1");
        assert_eq!(jar.header(&other, true, &other_context, 101), "other=1");
    }

    #[test]
    fn cookie_domain_path_expiry_and_script_protection() {
        let origin = url("https://app.example.com/account/page");
        let context = Context::document(&origin);
        let mut jar = CookieJar::default();
        assert!(jar.store(
            &origin,
            "session=secret; HttpOnly; Secure; Path=/",
            true,
            &context,
            100
        ));
        assert!(jar.store(
            &origin,
            "theme=dark; Domain=example.com; Max-Age=10",
            false,
            &context,
            100
        ));
        assert_eq!(jar.header(&origin, false, &context, 101), "theme=dark");
        assert_eq!(
            jar.header(&origin, true, &context, 101),
            "theme=dark; session=secret"
        );
        assert!(!jar.store(&origin, "session=forged; Path=/", false, &context, 102));
        assert!(!jar.store(&origin, "session=; Path=/; Max-Age=0", false, &context, 102));
        assert_eq!(
            jar.header(
                &url("https://other.example.com/account/next"),
                true,
                &context,
                102
            ),
            "theme=dark"
        );
        assert_eq!(
            jar.header(
                &url("https://app.example.com/accountant"),
                true,
                &context,
                102
            ),
            "session=secret"
        );
        assert_eq!(jar.header(&origin, true, &context, 111), "session=secret");
        let insecure = url("http://app.example.com/account/page");
        assert!(!jar.store(
            &insecure,
            "session=forged; Path=/",
            true,
            &Context::document(&insecure),
            112
        ));
        assert_eq!(
            jar.header(&insecure, true, &Context::document(&insecure), 112),
            ""
        );
    }

    #[test]
    fn cookie_public_suffix_prefixes_and_date_precedence() {
        let origin = url("https://app.example.co.uk/");
        let context = Context::document(&origin);
        let mut jar = CookieJar::default();
        for invalid in [
            "a=1; Domain=co.uk",
            "a=1; Domain=other.co.uk",
            "__Secure-a=1",
            "__Host-a=1; Secure",
            "__Host-a=1; Secure; Path=/; Domain=example.co.uk",
            "__Http-a=1; Secure",
        ] {
            assert!(
                !jar.store(&origin, invalid, true, &context, 100),
                "{invalid}"
            );
        }
        assert!(jar.store(&origin, "__Host-a=1; Secure; Path=/", false, &context, 100));
        assert!(jar.store(
            &origin,
            "a=1; Expires=Thu, 01 Jan 1970 00:00:00 GMT; Max-Age=10",
            true,
            &context,
            100
        ));
        assert!(jar.header(&origin, true, &context, 105).contains("a=1"));
        assert_eq!(jar.header(&origin, true, &context, 110), "__Host-a=1");
        assert_eq!(
            cookie_date("Wed, 09 Jun 2021 10:18:14 GMT"),
            Some(1623233894)
        );
        assert_eq!(cookie_date("Wed, 09-Jun-21 10:18:14 GMT"), Some(1623233894));
        assert_eq!(
            cookie_date("Wed, 09th June 2021year 10:18:14GMT"),
            Some(1623233894)
        );
        assert_eq!(cookie_date("Wed, 09 Jun 2021 010:18:14 GMT"), None);
        assert_eq!(cookie_date("Wed, 09 Jun 2021 10:18:014 GMT"), None);
        assert_eq!(cookie_date("Wed, 09 Jun 2021 24:18:14 10:18:14 GMT"), None);
        assert_eq!(cookie_date("Wed, 31 Feb 2021 10:18:14 GMT"), None);
        let private = url("https://one.github.io/");
        assert!(!jar.store(
            &private,
            "a=1; Domain=github.io",
            true,
            &Context::document(&private),
            100
        ));
        assert_ne!(site(&private), site(&url("https://two.github.io/")));
        assert_ne!(
            site(&url("https://192.168.0.1/")),
            site(&url("https://10.0.0.1/"))
        );
    }

    #[test]
    fn cookie_invalid_duplicate_attributes_preserve_last_valid_value() {
        let origin = url("https://app.example.com/");
        let context = Context::document(&origin);
        let mut jar = CookieJar::default();
        assert!(jar.store(
            &origin,
            "expired=1; Expires=Thu, 01 Jan 1970 00:00:00 GMT; Expires=invalid",
            true,
            &context,
            100
        ));
        assert_eq!(jar.header(&origin, true, &context, 100), "");
        assert!(jar.store(
            &origin,
            "domain=1; Domain=example.com; Domain=",
            true,
            &context,
            100
        ));
        assert_eq!(
            jar.header(&url("https://other.example.com/"), true, &context, 100),
            "domain=1"
        );
        assert!(jar.store(&origin, "host=2; Domain=", true, &context, 100));
        assert_eq!(
            jar.header(&url("https://other.example.com/"), true, &context, 100),
            "domain=1"
        );
        assert!(jar.store(
            &origin,
            "__Host-prefix=3; Secure; Path=/; Domain=",
            true,
            &context,
            100
        ));
        assert!(jar
            .header(&origin, true, &context, 100)
            .contains("__Host-prefix=3"));
        assert!(jar.store(
            &origin,
            "last=4; Domain=unrelated.test; Domain=example.com",
            true,
            &context,
            100
        ));
        assert!(jar
            .header(&url("https://other.example.com/"), true, &context, 100)
            .contains("last=4"));
        assert!(!jar.store(
            &origin,
            "rejected=5; Domain=example.com; Domain=unrelated.test",
            true,
            &context,
            100
        ));
    }

    #[test]
    fn cookie_same_site_and_partitioned_requests() {
        let origin = url("https://service.example.com/");
        let own = Context::document(&origin);
        let other = Context::document(&url("https://unrelated.test/"));
        let mut jar = CookieJar::default();
        assert!(jar.store(
            &origin,
            "strict=1; SameSite=Strict; Secure",
            true,
            &own,
            100
        ));
        assert!(jar.store(&origin, "lax=1; SameSite=Lax; Secure", true, &own, 100));
        assert!(jar.store(&origin, "default=1; Secure", true, &own, 100));
        assert!(jar.store(
            &origin,
            "third=1; SameSite=None; Secure; Partitioned",
            true,
            &other,
            100
        ));
        assert!(!jar.store(&origin, "blocked=1; SameSite=Lax", true, &other, 100));
        assert_eq!(jar.header(&origin, true, &other, 300), "third=1");
        let navigation = Context {
            top_level_navigation: true,
            ..other.clone()
        };
        assert_eq!(
            jar.header(&origin, true, &navigation, 300),
            "lax=1; default=1; third=1"
        );
        let post = Context {
            safe_method: false,
            ..navigation
        };
        assert_eq!(jar.header(&origin, true, &post, 300), "third=1");
        assert_eq!(jar.header(&origin, true, &post, 150), "default=1; third=1");
        assert!(!jar.header(&origin, true, &own, 150).contains("third="));
        assert!(jar
            .header(
                &origin,
                true,
                &Context::document(&url("https://other.example.com/")),
                150
            )
            .contains("strict=1"));
        assert!(!jar
            .header(
                &origin,
                true,
                &Context::document(&url("http://other.example.com/")),
                150
            )
            .contains("strict=1"));
    }

    #[test]
    fn cookie_storage_budget_and_replacement_order() {
        let origin = url("https://example.test/");
        let context = Context::document(&origin);
        let mut jar = CookieJar::default();
        for index in 0..200 {
            assert!(jar.store(&origin, &format!("cookie{index}="), false, &context, 100));
        }
        assert_eq!(jar.entries.len(), MAX_SITE_COOKIES);
        assert!(jar.entries.iter().all(|cookie| cookie.name != "cookie0"));
        let first = jar.entries[0].name.clone();
        assert!(jar.store(&origin, &format!("{first}=new"), false, &context, 101));
        assert!(jar
            .header(&origin, false, &context, 102)
            .starts_with(&format!("{first}=new;")));
        assert!(!jar.store(
            &origin,
            &format!("oversized={}", "x".repeat(MAX_COOKIE_BYTES)),
            true,
            &context,
            102
        ));
        assert!(!jar.store(&origin, "inject=1\r\nCookie: stolen", true, &context, 102));
    }
}
