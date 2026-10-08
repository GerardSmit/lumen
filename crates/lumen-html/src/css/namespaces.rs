//! Stylesheet-local CSS namespace names; URI values are literal strings.
use super::*;
use alloc::format;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NamespaceRule {
    pub prefix: Option<Arc<str>>,
    pub uri: Arc<str>,
}
impl NamespaceRule {
    pub fn serialize(&self) -> String {
        let mut text=String::from("@namespace ");
        if let Some(prefix)=&self.prefix {text.push_str(&serialize_identifier(prefix));text.push(' ');}
        text.push_str(&serialize_url(&self.uri));text.push(';');text
    }
}

pub(super) fn parse_prelude(raw: &str) -> Option<NamespaceRule> {
    let completed=syntax::complete(raw).ok()??;
    let input=completed.as_ref();let mut position=0;
    skip_css_space_comments(input,&mut position)?;
    if input.as_bytes().get(position)!=Some(&b'@') {return None;}
    position+=1;
    if !consume_selector_identifier(input,&mut position)?.eq_ignore_ascii_case("namespace") {return None;}
    skip_css_space_comments(input,&mut position)?;
    let start=position;
    let mut prefix=None;
    if !matches!(input.as_bytes().get(position),Some(b'\''|b'"')) {
        let name=consume_selector_identifier(input,&mut position)?;
        if !name.eq_ignore_ascii_case("url") || input.as_bytes().get(position)!=Some(&b'(') {
            prefix=Some(Arc::from(name));skip_css_space_comments(input,&mut position)?;
        } else {position=start;}
    }
    let uri=if matches!(input.as_bytes().get(position),Some(b'\''|b'"')) {
        let end=quoted_css_end(input,position)?;
        let uri=Arc::from(css_string(&input[position..end])?);position=end;uri
    } else {
        let start=position;
        if !consume_selector_identifier(input,&mut position)?.eq_ignore_ascii_case("url")
            || input.as_bytes().get(position)!=Some(&b'(') {return None;}
        let end=matching_css_block(input,position)?;
        let uri=if input[start..position].eq_ignore_ascii_case("url") {
            background_url(&input[start..end])?
        } else {background_url(&format!("url{}",&input[position..end]))?};
        position=end;uri
    };
    skip_css_space_comments(input,&mut position)?;
    if position!=input.len() {return None;}
    Some(NamespaceRule{prefix,uri})
}
pub fn parse_rule(input:&str)->Result<NamespaceRule,CssError> {
    let source=nesting::parse_one_source_rule(input,&[])?;
    if source.kind!=nesting::SourceRuleKind::Statement {
        return Err(selector_error(0,"not a namespace rule"));
    }
    parse_prelude(&input[source.prelude]).ok_or_else(||selector_error(0,"invalid namespace rule"))
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NamespaceMap { entries: Vec<NamespaceRule> }
impl NamespaceMap {
    pub fn resolve(&self,prefix:Option<&str>)->Option<Arc<str>> {
        self.entries.iter().rev().find(|rule|rule.prefix.as_deref()==prefix).map(|rule|rule.uri.clone())
    }
    pub fn from_stylesheet(input:&str)->Result<Self,CssError> {
        let mut map=Self::default();let mut phase=0u8;
        for (start,end,open,_) in nesting::source_rule_ranges(input,false)? {
            let prelude=input[start..open.unwrap_or(end)].trim().trim_end_matches(';');
            if open.is_none() {
                if let Some(rule)=parse_prelude(prelude) {
                    if phase<3 {map.entries.push(rule);phase=2;}
                    continue;
                }
                if nesting::at_rule(prelude,"@import") && parse_import_prelude(prelude,start..end).is_some() {
                    if phase<2 {phase=1;}continue;
                }
                if nesting::at_rule(prelude,"@layer") && comma_components(&prelude[6..],128).is_some_and(|names|!names.is_empty()&&names.iter().all(|name|valid_import_layer_name(name.trim()))) {
                    if phase!=0 {phase=3;}continue;
                }
                // Other invalid/unknown statements are ignored.
                continue;
            }
            if !prelude.starts_with('@') || ["@media","@supports","@layer","@scope","@font-face","@keyframes","@starting-style"].iter().any(|name|nesting::at_rule(prelude,name)) {
                phase=3;
            }
        }
        Ok(map)
    }
}

#[derive(Clone,Debug,Eq,PartialEq)]
pub(super) enum NamespaceConstraint { Any, None, Uri(Arc<str>) }
impl NamespaceConstraint {
    pub fn matches(&self,uri:&str)->bool {match self {Self::Any=>true,Self::None=>uri.is_empty(),Self::Uri(expected)=>expected.as_ref()==uri}}
}

#[cfg(test)]
mod tests {
    use super::*;
    fn kind(uri:&str)->NodeKind {NodeKind::Element {namespace:Namespace::Other(alloc::rc::Rc::from(uri)),name:"item".into(),attributes:vec![("class".into(),"x".into()),("flag".into(),"yes".into())]}}
    fn matches(input:&str,map:&NamespaceMap,uri:&str)->bool {parse_selector_depth(input,0,0,false,map).unwrap().matches(&kind(uri))}
    #[test]
    fn specification_css_namespaces_literal_grammar_scope_and_default_subjects() {
        for source in [r#"@namespace "";"#,r#"@namespace P "../literal#name";"#,r#"@namespace p url(urn:test);"#,r#"@namespace p\31  "urn:test";"#,r#"@namesp\61 ce p u\72l("urn:test");"#] {
            let rule=parse_rule(source).unwrap();
            assert_eq!(parse_rule(&rule.serialize()).unwrap(),rule,"literal URI/prefix canonical roundtrip");
        }
        for source in ["@namespace;","@namespace a b;","@namespace p url(x) extra;","@namespace p {}"] {assert!(parse_rule(source).is_err(),"{source}");}
        let map=NamespaceMap::from_stylesheet(r#"@namespace "urn:default";@namespace P "urn:first";@namespace P "urn:last";@namespace p "urn:lower";"#).unwrap();
        assert!(matches("P|item",&map,"urn:last"));assert!(!matches("P|item",&map,"urn:first"));
        assert!(matches("p|item",&map,"urn:lower"));assert!(matches(".x",&map,"urn:default"));assert!(!matches(".x",&map,"urn:last"));
        assert!(matches("*|item",&map,""));assert!(matches("|item",&map,""));assert!(!matches("|item",&map,"urn:last"));
        assert!(matches("*|*:is(.x)",&map,"urn:last"));assert!(!matches("*|*:is(*.x)",&map,"urn:last"));
        assert!(matches("*|*:not(*.x)",&map,"urn:last"));assert!(!matches("*|*:not(.x)",&map,"urn:last"));
        assert!(matches("*|*:where(missing|item,.x)",&map,"urn:last"));
        assert!(parse_selector_depth("missing|item",0,0,false,&map).is_err());
        assert!(parse_selector_depth("P|item",0,0,false,&NamespaceMap::default()).is_err(),"DOM selector context never consults stylesheet prefixes");
        let index=StyleIndex::new(parse("@namespace P 'urn:last';@supports selector(P|item){*|item{width:13px}}@supports selector(missing|item){*|item{height:99px}}").unwrap());
        let supported=compute(&kind("urn:last"),None,&index).unwrap();assert_eq!(supported.width,Some(13.));assert_eq!(supported.height,None);
        assert!(!supports_condition("selector(P|item)"),"CSS.supports uses an empty namespace environment");
        let late=NamespaceMap::from_stylesheet(".x{}@namespace P 'urn:last';").unwrap();
        assert!(late.resolve(Some("P")).is_none());
    }
    #[test]
    fn specification_css_namespaces_attribute_expanded_names_and_dom_queries() {
        let mut document=Document::new(16);let node=document.create(kind("urn:element")).unwrap();
        document.append(document.root(),node).unwrap();
        document.set_attribute_ns(node,Some("urn:attr"),"a:flag","namespaced").unwrap();
        let index=StyleIndex::new(parse(r#"@namespace "urn:element";@namespace A "urn:attr";item[flag=yes]{width:3px}item[A|flag=namespaced]{height:7px}item[*|flag=namespaced]{opacity:.5}"#).unwrap());
        let style=compute_node(&document,node,None,&index).unwrap();
        assert_eq!(style.width,Some(3.));assert_eq!(style.height,Some(7.));assert_eq!(style.opacity,0.5);
        assert!(crate::selector::query_selector(&document,document.root(),"A|item").is_err());
        assert_eq!(crate::selector::query_selector(&document,document.root(),"*|item").unwrap(),Some(node));
        document.set_attribute_ns(node,Some("urn:other"),"b:flag","changed").unwrap();
        assert_eq!(compute_node(&document,node,None,&index).unwrap().height,Some(7.),"unrelated expanded attribute cannot overwrite matched namespace");
    }    #[test]
    fn specification_css_namespaces_imported_sheet_contexts_never_leak() {
        let text=r#"@import 'child.css';@namespace N 'urn:root';N|item{width:11px}"#;
        let source=StylesheetSource {disabled:false,url:Arc::from("https://namespace.test/root.css"),text:Arc::from(text),imports:vec![LoadedImport {
            rule:imports(text).unwrap().remove(0),source:Some(Box::new(StylesheetSource {disabled:false,url:Arc::from("https://namespace.test/child.css"),text:Arc::from("@namespace N 'urn:child';N|item{height:7px}M|item{opacity:.5}"),imports:Vec::new()}))
        }]};
        let parsed=parse_graph(&source,MediaEnvironment::default()).unwrap();let index=StyleIndex::new(parsed.rules);
        let root=compute(&kind("urn:root"),None,&index).unwrap();let child=compute(&kind("urn:child"),None,&index).unwrap();
        assert_eq!(root.width,Some(11.));assert_eq!(root.height,None);
        assert_eq!(child.height,Some(7.));assert_eq!(child.width,None);assert_eq!(child.opacity,1.);
    }

}
