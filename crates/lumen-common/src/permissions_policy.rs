//! Shared Permissions Policy declarations over maintained Structured Fields,
//! URL and CSP source-expression authorities. Host origins remain in adapters.
use crate::limits::{BudgetedString, BudgetedVec, ByteBudget};
use alloc::{string::String, sync::Arc};
use core::fmt;
use sfv::{
    visitor::{DictionaryVisitor, EntryVisitor, InnerListVisitor, ItemVisitor, ParameterVisitor},
    BareItemFromInput, KeyRef,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DefaultAllowlist {
    All,
    SelfOrigin,
}
#[derive(Clone, Copy, Debug)]
pub struct Feature {
    pub name: &'static str,
    pub default: DefaultAllowlist,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Capacity,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("permissions policy capacity exceeded")
    }
}
impl core::error::Error for Error {}

/// Permissions Policy §9.8: undeclared features have enabled parent values;
/// the default allowlist is applied separately when inheriting into a child.
pub fn feature_value(inherited: bool, declared_matches: Option<bool>) -> bool {
    inherited && declared_matches.unwrap_or(true)
}
/// Permissions Policy §9.9 used by an actual consumer and introspection.
pub fn check_policy(
    inherited: bool,
    declared_matches: Option<bool>,
    default: DefaultAllowlist,
    same_document_origin: bool,
) -> bool {
    inherited
        && declared_matches.unwrap_or(default == DefaultAllowlist::All || same_document_origin)
}
/// Permissions Policy §9.7: a container cannot re-enable an ancestor denial,
/// and a parent declaration independently restricts its own and target origins.
pub fn inherit_feature(
    parent_own_value: bool,
    parent_target_value: bool,
    container_matches: Option<bool>,
    default: DefaultAllowlist,
    same_parent_origin: bool,
) -> bool {
    parent_own_value
        && parent_target_value
        && container_matches.unwrap_or(default == DefaultAllowlist::All || same_parent_origin)
}

pub struct Allowlist {
    pub all: bool,
    pub self_origin: bool,
    pub src_origin: bool,
    expressions: BudgetedVec<BudgetedString>,
    report_to: Option<BudgetedString>,
}
impl Allowlist {
    fn empty(budget: &Arc<ByteBudget>) -> Self {
        Self {
            all: false,
            self_origin: false,
            src_origin: false,
            expressions: BudgetedVec::new(budget.clone(), crate::csp::MAX_POLICY_BYTES),
            report_to: None,
        }
    }
    pub fn expressions(&self) -> impl Iterator<Item = &str> {
        self.expressions
            .as_slice()
            .iter()
            .map(BudgetedString::as_str)
    }
    pub fn report_to(&self) -> Option<&str> {
        self.report_to.as_ref().map(BudgetedString::as_str)
    }
    fn expression(&mut self, value: &str, budget: &Arc<ByteBudget>) -> Result<(), Error> {
        if self.all || self.expressions().any(|previous| previous == value) {
            return Ok(());
        }
        let value = BudgetedString::copy(value, budget).map_err(|_| Error::Capacity)?;
        self.expressions.push(value).map_err(|_| Error::Capacity)
    }
    /// Self/src identity is supplied by the adapter from the origins captured
    /// when these declarations were parsed, including opaque origin identity.
    pub fn matches(&self, origin: &str, same_self_origin: bool, same_src_origin: bool) -> bool {
        if self.all
            || (self.self_origin && same_self_origin)
            || (self.src_origin && same_src_origin)
        {
            return true;
        }
        if origin == "null" || self.expressions.is_empty() {
            return false;
        }
        let Ok(url) = content_security_policy::Url::parse(origin) else {
            return false;
        };
        let origin = url.origin();
        self.expressions().any(|expression| {
            content_security_policy::matches_permissions_source_expression(
                &url, expression, &origin,
            )
        })
    }
}

pub struct Declarations {
    entries: BudgetedVec<Option<Allowlist>>,
    budget: Arc<ByteBudget>,
}
impl Declarations {
    pub fn empty(features: &[Feature], budget: Arc<ByteBudget>) -> Result<Self, Error> {
        let mut entries = BudgetedVec::new(budget.clone(), features.len());
        for _ in features {
            entries.push(None).map_err(|_| Error::Capacity)?;
        }
        Ok(Self { entries, budget })
    }
    pub fn get(&self, feature: usize) -> Option<&Allowlist> {
        self.entries
            .as_slice()
            .get(feature)
            .and_then(Option::as_ref)
    }
    pub fn has(&self, feature: usize) -> bool {
        self.get(feature).is_some()
    }
    fn clear(&mut self) {
        for entry in self.entries.as_mut_slice() {
            *entry = None;
        }
    }
    pub fn parse_header(
        source: &str,
        features: &[Feature],
        budget: Arc<ByteBudget>,
    ) -> Result<Self, Error> {
        if source.len() > crate::csp::MAX_POLICY_BYTES {
            return Err(Error::Capacity);
        }
        let mut result = Self::empty(features, budget.clone())?;
        // sfv's visitor parser retains no dictionary or inner-item vectors.
        // Its only transient buffers are the current decoded escaped string,
        // display string or byte sequence. Charge their bounded input-derived
        // growth/reallocation peak before entering that maintained parser.
        let decoder_bytes = source
            .len()
            .checked_mul(3)
            .and_then(|n| n.checked_add(64))
            .ok_or(Error::Capacity)?;
        let _decoder = budget.reserve(decoder_bytes).ok_or(Error::Capacity)?;
        let mut capacity_failed = false;
        let parsed = sfv::Parser::new(source).parse_dictionary_with_visitor(HeaderVisitor {
            features,
            result: &mut result,
            capacity_failed: &mut capacity_failed,
        });
        if parsed.is_err() {
            if capacity_failed {
                return Err(Error::Capacity);
            }
            // Structured Fields syntax failure discards the complete field,
            // including declarations already visited before the failure.
            result.clear();
        }
        Ok(result)
    }
    pub fn parse_response(
        headers: &[(String, String)],
        report_only: bool,
        features: &[Feature],
        budget: Arc<ByteBudget>,
    ) -> Result<Self, Error> {
        let name = if report_only {
            "Permissions-Policy-Report-Only"
        } else {
            "Permissions-Policy"
        };
        let fields = headers
            .iter()
            .filter(move |(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str());
        let mut first = fields.clone();
        let Some(value) = first.next() else {
            return Self::empty(features, budget);
        };
        if first.next().is_none() {
            return Self::parse_header(value, features, budget);
        }
        let field = BudgetedString::join(fields, ", ", &budget).map_err(|_| Error::Capacity)?;
        Self::parse_header(field.as_str(), features, budget)
    }
    /// Infra semicolon/ASCII-whitespace processing delegates origins to the
    /// existing URL parser. `src_origin` means the supplied declared origin,
    /// rather than the redirected child document's current URL.
    pub fn parse_attribute(
        source: &str,
        features: &[Feature],
        has_target_origin: bool,
        budget: Arc<ByteBudget>,
    ) -> Result<Self, Error> {
        if source.len() > crate::csp::MAX_POLICY_BYTES {
            return Err(Error::Capacity);
        }
        let mut result = Self::empty(features, budget.clone())?;
        for declaration in source.split(';') {
            let mut tokens = declaration.split_ascii_whitespace();
            let Some(name) = tokens.next() else {
                continue;
            };
            let Some(feature) = features.iter().position(|feature| feature.name == name) else {
                continue;
            };
            let mut list = Allowlist::empty(&budget);
            if tokens.clone().any(|token| token == "*") {
                list.all = true;
            } else {
                if tokens.clone().next().is_none() && has_target_origin {
                    list.src_origin = true;
                }
                for token in tokens {
                    if token.eq_ignore_ascii_case("'self'") {
                        list.self_origin = true;
                        continue;
                    }
                    if has_target_origin && token.eq_ignore_ascii_case("'src'") {
                        list.src_origin = true;
                        continue;
                    }
                    let Some(url) = crate::url::parse_url(token, None) else {
                        continue;
                    };
                    let origin = url.origin();
                    if origin == "null" {
                        continue;
                    }
                    list.expression(&origin, &budget)?;
                }
            }
            // The last declaration replaces an earlier declaration of the
            // same supported feature, as the normative ordered-map algorithm.
            result.entries.as_mut_slice()[feature] = Some(list);
        }
        Ok(result)
    }
    pub fn add_legacy_all(&mut self, feature: usize) {
        if feature < self.entries.len() && !self.has(feature) {
            let mut list = Allowlist::empty(&self.budget);
            list.all = true;
            self.entries.as_mut_slice()[feature] = Some(list);
        }
    }
}

struct HeaderVisitor<'a> {
    features: &'a [Feature],
    result: &'a mut Declarations,
    capacity_failed: &'a mut bool,
}
struct FeatureEntry<'a> {
    entry: Option<&'a mut Option<Allowlist>>,
    budget: Arc<ByteBudget>,
    capacity_failed: &'a mut bool,
}
struct FeatureItem<'a> {
    entry: Option<&'a mut Option<Allowlist>>,
    budget: Arc<ByteBudget>,
    capacity_failed: &'a mut bool,
    top_level: bool,
}
struct FeatureParams<'a> {
    entry: Option<&'a mut Option<Allowlist>>,
    budget: Arc<ByteBudget>,
    capacity_failed: &'a mut bool,
    reporting: bool,
}
impl<'de> DictionaryVisitor<'de> for HeaderVisitor<'_> {
    type Out = ();
    type Error = Error;
    fn entry(&mut self, key: &'de KeyRef) -> Result<impl EntryVisitor<'de>, Error> {
        let index = self
            .features
            .iter()
            .position(|feature| feature.name == key.as_str());
        let budget = self.result.budget.clone();
        let entry = index.map(|index| {
            let slot = &mut self.result.entries.as_mut_slice()[index];
            *slot = Some(Allowlist::empty(&budget));
            slot
        });
        Ok(FeatureEntry {
            entry,
            budget,
            capacity_failed: self.capacity_failed,
        })
    }
    fn finish(self) -> Result<(), Error> {
        Ok(())
    }
}
impl<'de> EntryVisitor<'de> for FeatureEntry<'_> {
    type Error = Error;
    fn item(self) -> Result<impl ItemVisitor<'de>, Error> {
        Ok(FeatureItem {
            entry: self.entry,
            budget: self.budget,
            capacity_failed: self.capacity_failed,
            top_level: true,
        })
    }
    fn inner_list(self) -> Result<impl InnerListVisitor<'de>, Error> {
        Ok(self)
    }
}
impl<'de> InnerListVisitor<'de> for FeatureEntry<'_> {
    type Error = Error;
    fn item(&mut self) -> Result<impl ItemVisitor<'de>, Error> {
        Ok(FeatureItem {
            entry: self.entry.as_deref_mut(),
            budget: self.budget.clone(),
            capacity_failed: self.capacity_failed,
            top_level: false,
        })
    }
    fn finish(self) -> Result<impl ParameterVisitor<'de>, Error> {
        Ok(FeatureParams {
            entry: self.entry,
            budget: self.budget,
            capacity_failed: self.capacity_failed,
            reporting: true,
        })
    }
}
impl<'de> ItemVisitor<'de> for FeatureItem<'_> {
    type Out = ();
    type Error = Error;
    fn bare_item(
        mut self,
        value: BareItemFromInput<'de>,
    ) -> Result<impl ParameterVisitor<'de, Out = ()>, Error> {
        if let Some(slot) = self.entry.as_deref_mut() {
            let mut valid = true;
            if let Some(list) = slot.as_mut() {
                match value {
                    BareItemFromInput::Token(token) if token.as_str() == "*" => {
                        *list = Allowlist::empty(&self.budget);
                        list.all = true;
                    }
                    BareItemFromInput::Token(token) if token.as_str() == "self" => {
                        list.self_origin = true
                    }
                    BareItemFromInput::String(value)
                        if content_security_policy::is_permissions_source_expression(
                            value.as_str(),
                        ) =>
                    {
                        if let Err(error) = list.expression(value.as_str(), &self.budget) {
                            *self.capacity_failed = true;
                            return Err(error);
                        }
                    }
                    _ => valid = false,
                }
            }
            if !valid && self.top_level {
                *slot = None;
            }
        }
        Ok(FeatureParams {
            entry: self.entry,
            budget: self.budget,
            capacity_failed: self.capacity_failed,
            reporting: self.top_level,
        })
    }
}
impl<'de> ParameterVisitor<'de> for FeatureParams<'_> {
    type Out = ();
    type Error = Error;
    fn parameter(&mut self, key: &'de KeyRef, value: BareItemFromInput<'de>) -> Result<(), Error> {
        if self.reporting && key.as_str() == "report-to" {
            if let Some(list) = self.entry.as_deref_mut().and_then(Option::as_mut) {
                list.report_to = None;
                if let BareItemFromInput::Token(value) = value {
                    match BudgetedString::copy(value.as_str(), &self.budget) {
                        Ok(value) => list.report_to = Some(value),
                        Err(_) => {
                            *self.capacity_failed = true;
                            return Err(Error::Capacity);
                        }
                    }
                }
            }
        }
        Ok(())
    }
    fn finish(self) -> Result<(), Error> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const FEATURES: [Feature; 2] = [
        Feature {
            name: "focus-without-user-activation",
            default: DefaultAllowlist::SelfOrigin,
        },
        Feature {
            name: "fullscreen",
            default: DefaultAllowlist::SelfOrigin,
        },
    ];
    fn budget() -> Arc<ByteBudget> {
        ByteBudget::new(crate::csp::MAX_POLICY_BYTES)
    }
    #[test]
    fn specification_permissions_policy_streamed_header_dictionary_and_origin_matching() {
        let policies=Declarations::parse_header(r#"focus-without-user-activation=(self "https://*.example.test" "https://example.test:*");report-to=focus, fullscreen=(), unknown=("https://unused.test")"#,&FEATURES,budget()).unwrap();
        let focus = policies.get(0).unwrap();
        assert!(focus.matches("https://owner.test", true, false));
        assert!(focus.matches("https://child.example.test", false, false));
        assert!(!focus.matches("https://unrelated.test", false, false));
        let subdomain = Declarations::parse_header(
            r#"fullscreen=("https://*.example.test")"#,
            &FEATURES,
            budget(),
        )
        .unwrap();
        assert!(!subdomain
            .get(1)
            .unwrap()
            .matches("https://example.test", false, false));
        assert!(focus.matches("https://example.test:8443", false, false));
        assert!(!focus.matches("null", false, false));
        assert_eq!(focus.report_to(), Some("focus"));
        assert!(!policies
            .get(1)
            .unwrap()
            .matches("https://owner.test", true, false));
        // §5.2 member semantics are distinct from Structured Fields syntax:
        // a source-expression String is permitted directly, whereas other
        // item kinds reject only that member, not the complete dictionary.
        let member_types = Declarations::parse_header(
            r#"fullscreen="https://direct.test", focus-without-user-activation=42"#,
            &FEATURES,
            budget(),
        )
        .unwrap();
        assert!(member_types
            .get(1)
            .unwrap()
            .matches("https://direct.test", false, false));
        assert!(!member_types.has(0));
        let inner = Declarations::parse_header(
            r#"fullscreen=("https://direct.test" "https://direct.test" ?1 42 @1 %"ignored")"#,
            &FEATURES,
            budget(),
        )
        .unwrap();
        assert_eq!(inner.get(1).unwrap().expressions().count(), 1);
        let duplicates = Declarations::parse_header(
            "fullscreen=*, fullscreen=?1, focus-without-user-activation=(?1 self 1)",
            &FEATURES,
            budget(),
        )
        .unwrap();
        assert!(!duplicates.has(1));
        assert!(duplicates
            .get(0)
            .unwrap()
            .matches("https://owner.test", true, false));
        let malformed = Declarations::parse_header(
            "fullscreen=*, focus-without-user-activation=(self",
            &FEATURES,
            budget(),
        )
        .unwrap();
        assert!(!malformed.has(0) && !malformed.has(1));
        let wildcard = Declarations::parse_header(
            r#"fullscreen=("https://before.test" * "https://after.test")"#,
            &FEATURES,
            budget(),
        )
        .unwrap();
        assert!(wildcard.get(1).unwrap().all);
        assert_eq!(wildcard.get(1).unwrap().expressions().count(), 0);
    }
    #[test]
    fn specification_permissions_policy_container_duplicates_defaults_and_inheritance() {
        let policies=Declarations::parse_attribute("fullscreen 'none'; fullscreen; focus-without-user-activation 'SELF' https://child.test/path",&FEATURES,true,budget()).unwrap();
        assert!(policies.get(1).unwrap().matches("null", false, true));
        assert!(policies
            .get(0)
            .unwrap()
            .matches("https://owner.test", true, false));
        assert!(policies
            .get(0)
            .unwrap()
            .matches("https://child.test", false, false));
        assert!(!policies
            .get(0)
            .unwrap()
            .matches("https://other.test", false, false));
        assert!(feature_value(true, None));
        assert!(!check_policy(
            true,
            None,
            DefaultAllowlist::SelfOrigin,
            false
        ));
        assert!(!inherit_feature(
            false,
            true,
            Some(true),
            DefaultAllowlist::SelfOrigin,
            true
        ));
        assert!(!inherit_feature(
            true,
            false,
            Some(true),
            DefaultAllowlist::SelfOrigin,
            true
        ));
        assert!(inherit_feature(
            true,
            true,
            Some(true),
            DefaultAllowlist::SelfOrigin,
            false
        ));
        assert!(!inherit_feature(
            true,
            true,
            None,
            DefaultAllowlist::SelfOrigin,
            false
        ));
    }
    #[test]
    fn specification_permissions_policy_failure_reclaims_real_owned_budget() {
        let storage = budget();
        let initial = storage.reserved();
        {
            let policies = Declarations::parse_header(
                r#"fullscreen=("https://one.test" "https://two.test")"#,
                &FEATURES,
                storage.clone(),
            )
            .unwrap();
            assert!(storage.reserved() > initial);
            assert_eq!(policies.get(1).unwrap().expressions().count(), 2);
        }
        assert_eq!(storage.reserved(), initial);
        assert!(matches!(
            Declarations::parse_header("fullscreen=*", &FEATURES, ByteBudget::new(1)),
            Err(Error::Capacity)
        ));
        assert!(matches!(
            Declarations::parse_attribute(
                "fullscreen https://one.test",
                &FEATURES,
                true,
                ByteBudget::new(1)
            ),
            Err(Error::Capacity)
        ));
    }
}
