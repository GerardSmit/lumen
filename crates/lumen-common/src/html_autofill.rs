//! HTML §4.10.19.7.2 autofill token processing, independent of a host language.
use alloc::string::String;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Category { Off, Automatic, Normal, Contact, Credential }

fn category(token: &str) -> Option<Category> {
    if token.eq_ignore_ascii_case("off") { return Some(Category::Off); }
    if token.eq_ignore_ascii_case("on") { return Some(Category::Automatic); }
    if token.eq_ignore_ascii_case("webauthn") { return Some(Category::Credential); }
    if CONTACT_FIELDS.iter().any(|field|token.eq_ignore_ascii_case(field)) { return Some(Category::Contact); }
    NORMAL_FIELDS.iter().any(|field|token.eq_ignore_ascii_case(field)).then_some(Category::Normal)
}
const NORMAL_FIELDS: &[&str] = &[
    "name", "honorific-prefix", "given-name", "additional-name", "family-name", "honorific-suffix",
    "nickname", "username", "new-password", "current-password", "one-time-code", "organization-title",
    "organization", "street-address", "address-line1", "address-line2", "address-line3", "address-level4",
    "address-level3", "address-level2", "address-level1", "country", "country-name", "postal-code",
    "cc-name", "cc-given-name", "cc-additional-name", "cc-family-name", "cc-number", "cc-exp",
    "cc-exp-month", "cc-exp-year", "cc-csc", "cc-type", "transaction-currency", "transaction-amount",
    "language", "bday", "bday-day", "bday-month", "bday-year", "sex", "url", "photo",
];
const CONTACT_FIELDS: &[&str] = &[
    "tel", "tel-country-code", "tel-national", "tel-area-code", "tel-local", "tel-local-prefix",
    "tel-local-suffix", "tel-extension", "email", "impp",
];
fn keyword(token: &str, values: &'static [&'static str]) -> Option<&'static str> {
    values.iter().copied().find(|value|token.eq_ignore_ascii_case(value))
}

/// A borrowed processing result. Parsing allocates nothing and retains no DOM
/// state. Field and credential spelling follow the processing algorithm; only
/// section, contact and address prefixes are normalized in exposed strings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Autofill<'a> {
    pub field_name: &'a str,
    pub address_hint: Option<&'static str>,
    pub contact_hint: Option<&'static str>,
    pub credential_type: Option<&'static str>,
    section: Option<&'a str>,
    idl_field: Option<&'a str>,
    idl_credential: Option<&'a str>,
}
impl Autofill<'_> {
    pub fn idl_value(&self) -> String {
        self.serialize(true)
    }
    pub fn scope(&self) -> String {
        self.serialize(false)
    }
    fn serialize(&self, idl: bool) -> String {
        let tokens=[self.section,self.address_hint,self.contact_hint,
            if idl {self.idl_field} else {None},if idl {self.idl_credential} else {None}];
        let count=tokens.iter().flatten().count();
        let bytes=tokens.iter().flatten().map(|token|token.len()).sum::<usize>()+count.saturating_sub(1);
        let mut value=String::with_capacity(bytes);
        for (index,token) in tokens.into_iter().enumerate() {
            let Some(token)=token else {continue};
            if !value.is_empty(){value.push(' ');}
            if index==0 {value.extend(token.chars().map(|character|character.to_ascii_lowercase()));}
            else {value.push_str(token);}
        }
        value
    }
}

/// Apply the processing model, rather than the separate author-conformance
/// restrictions on which field names are appropriate for a control's type.
pub fn parse(raw: Option<&str>, anchor: bool, form_off: bool) -> Autofill<'_> {
    let default=Autofill {field_name:if anchor {""} else if form_off {"off"} else {"on"},
        address_hint:None,contact_hint:None,credential_type:None,section:None,idl_field:None,idl_credential:None};
    let Some(raw)=raw else{return default};
    let mut tokens=raw.split_ascii_whitespace();
    let Some(field)=tokens.next_back() else{return default};
    let Some(mut category)=category(field) else{return default};
    if matches!(category,Category::Off|Category::Automatic) {
        if anchor||tokens.next_back().is_some(){return default;}
        let field=if category==Category::Off {"off"} else {"on"};
        return Autofill {field_name:field,idl_field:Some(field),..default};
    }
    let mut result=Autofill {field_name:field,idl_field:Some(field),..default};
    if category==Category::Credential {
        result.credential_type=Some("webauthn");
        result.idl_credential=Some(field);
        result.idl_field=None;
        let Some(field)=tokens.next_back() else{return result};
        let Some(previous_category)=self::category(field) else{return default};
        if !matches!(previous_category,Category::Normal|Category::Contact){return default;}
        category=previous_category;
        result.idl_field=Some(field);
    }
    let Some(mut prefix)=tokens.next_back() else{return result};
    if category==Category::Contact {
        if let Some(contact)=keyword(prefix,&["home","work","mobile","fax","pager"]) {
            result.contact_hint=Some(contact);
            let Some(next)=tokens.next_back() else{return result};prefix=next;
        }
    }
    if let Some(address)=keyword(prefix,&["shipping","billing"]) {
        result.address_hint=Some(address);
        let Some(next)=tokens.next_back() else{return result};prefix=next;
    }
    if tokens.next_back().is_some()||!prefix.get(..8).is_some_and(|start|start.eq_ignore_ascii_case("section-")) {
        return default;
    }
    result.section=Some(prefix);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn specification_autofill_order_case_mantles_and_default_fields() {
        let result=parse(Some("  SECTION-ÄBC\tBILLING\nWORK\remail\x0cWebAuthn "),false,false);
        assert_eq!(result.idl_value(),"section-Äbc billing work email WebAuthn");
        assert_eq!(result.scope(),"section-Äbc billing work");
        assert_eq!(result.field_name,"WebAuthn");
        assert_eq!(result.credential_type,Some("webauthn"));
        assert_eq!(parse(Some("NaMe"),false,false).idl_value(),"NaMe");
        assert_eq!(parse(Some("OFF"),false,false).idl_value(),"off");
        assert_eq!(parse(Some("WebAuthn"),true,true).idl_value(),"WebAuthn");
        for raw in ["home name","shipping section-a email","section-a home shipping tel",
            "section-a billing work home tel","name off","on webauthn","home webauthn",
            "section-a email webauthn webauthn","section-a section-b email","email\u{a0}","email\x0b"] {
            let result=parse(Some(raw),false,true);
            assert_eq!(result.idl_value(),"", "{raw}");
            assert_eq!(result.field_name,"off", "{raw}");
            assert_eq!(result.scope(),"", "{raw}");
            assert_eq!(result.credential_type,None,"{raw}");
        }
        for raw in [None,Some(""),Some("on"),Some("off"),Some("bad")] {
            let result=parse(raw,true,true);assert_eq!(result.field_name,"");assert_eq!(result.idl_value(),"");
        }
        assert_eq!(parse(None,false,false).field_name,"on");
        assert_eq!(parse(Some("section- photo"),true,false).scope(),"section-");
        for field in NORMAL_FIELDS.iter().chain(CONTACT_FIELDS) {
            assert_eq!(parse(Some(field),false,false).idl_value(),*field);
        }
    }
}
