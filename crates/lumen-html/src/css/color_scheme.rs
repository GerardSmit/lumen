//! Color Adjustment's negotiated scheme is one compact environment value.
//! Author support remains the original computed identifier list on Style.
use super::*;

#[derive(Clone,Copy,Debug,Default,PartialEq,Eq,Hash,PartialOrd,Ord)]
pub enum UsedColorScheme { #[default] Light, Dark }
#[derive(Clone,Copy,Debug,Default,PartialEq,Eq)]
pub enum ColorSchemePreference { #[default] None, Light, Dark }
#[derive(Clone,Copy,Debug,Default,PartialEq,Eq)]
pub struct ColorSchemeSupport(u8);
impl ColorSchemeSupport {
    pub fn from_computed(value:Option<&str>)->Self {
        match value {
            None|Some("normal")=>return Self(0),Some("light")=>return Self(5),Some("light only")=>return Self(13),
            Some("dark")=>return Self(2),Some("dark only")=>return Self(10),Some("light dark")=>return Self(7),Some("light dark only")=>return Self(15),
            Some("dark light")=>return Self(3),Some("dark light only")=>return Self(11),_=>{}
        }
        let mut flags=0u8;
        if let Some(value)=value { let mut position=0;while skip_css_space_comments(value,&mut position).is_some()&&position<value.len(){
            let Some(token)=consume_selector_identifier(value,&mut position) else{break;};
            match token.as_str() {"light"=>{if flags&3==0{flags|=4;}flags|=1;},"dark"=>{flags|=2;},"only"=>flags|=8,_=>{}}
        }}
        Self(flags)
    }
    pub fn parse(value:&str)->Option<Self> {parse_color_scheme(value).map(|value|Self::from_computed(value.as_deref()))}
    pub fn resolve(self,preference:ColorSchemePreference,default:UsedColorScheme)->UsedScheme {
        if self.0&3==0{return UsedScheme{scheme:default,defaulted:true};}
        let first=if self.0&4!=0{UsedColorScheme::Light}else{UsedColorScheme::Dark};
        let scheme=match preference {ColorSchemePreference::Light if self.0&1!=0=>UsedColorScheme::Light,
            ColorSchemePreference::Dark if self.0&2!=0=>UsedColorScheme::Dark,_=>first};
        UsedScheme{scheme,defaulted:false}
    }
}
#[derive(Clone,Copy,Debug,Default,PartialEq,Eq)]
pub struct UsedScheme {pub scheme:UsedColorScheme,pub defaulted:bool}
#[derive(Clone,Copy,Debug,Default,PartialEq,Eq)]
pub struct ColorSchemeEnvironment {
    pub preference:ColorSchemePreference,
    pub page_support:ColorSchemeSupport,
    pub default:UsedColorScheme,
}
impl ColorSchemeEnvironment {
    pub fn page(self)->UsedScheme {self.page_support.resolve(self.preference,self.default)}
    pub fn element(self,support:Option<&str>)->UsedScheme {
        match support {None|Some("normal")=>self.page(),Some(value)=>ColorSchemeSupport::from_computed(Some(value)).resolve(self.preference,self.default)}
    }
    pub fn embedded(self,scheme:UsedColorScheme)->Self {
        Self{preference:match scheme{UsedColorScheme::Light=>ColorSchemePreference::Light,UsedColorScheme::Dark=>ColorSchemePreference::Dark},page_support:ColorSchemeSupport::default(),default:self.default}
    }
}

/// Declaration admission must retain scheme-dependent images until the
/// element support has participated in the cascade. URL/string tokens are opaque.
pub(super) fn has_branches(raw:&str)->bool {
    let Some(mut cursor)=syntax::Cursor::new(raw,0).ok() else{return false;};
    while let Some(token)=cursor.next() {
        if token.kind==syntax::TokenKind::Other && raw.as_bytes().get(token.end)==Some(&b'(')
            && decoded_css_keyword(&raw[token.start..token.end],"light-dark"){return true;}
    }
    false
}

/// Select computed branches using the shared lexical/component authority.
/// Strings, comments and URL tokens are opaque. Ordinary sources stay borrowed.
pub(super) fn select_branches(raw:&str,scheme:UsedColorScheme)->Option<alloc::borrow::Cow<'_,str>> {
    fn select(raw:&str,scheme:UsedColorScheme,depth:u8)->Option<alloc::borrow::Cow<'_,str>> {
        if depth>=8 || raw.len()>MAX_CSS_BYTES{return None;}
        if !raw.contains('\\') && !raw.as_bytes().windows(10).any(|name|name.eq_ignore_ascii_case(b"light-dark")) {return Some(alloc::borrow::Cow::Borrowed(raw));}
        let mut cursor=syntax::Cursor::new(raw,0).ok()?;let mut copied=0;let mut output=None::<String>;
        while let Some(token)=cursor.next() {
            if token.kind!=syntax::TokenKind::Other || raw.as_bytes().get(token.end)!=Some(&b'('){continue;}
            let name=&raw[token.start..token.end];
            if !decoded_css_keyword(name,"light-dark"){continue;}
            let block=syntax::block(raw,token.end).ok()?;
            let parts=top_level_split(&raw[token.end+1..block.content_end],b',',2)?;
            let [light,dark]=parts.as_slice() else{return None;};
            let selected=select(if scheme==UsedColorScheme::Dark{dark}else{light},scheme,depth+1)?;
            // Color5 image-none is a transparent image, which suppresses marker
            // fallback and retains image semantics inside nested compositions.
            let selected=if decoded_css_keyword(selected.trim(),"none"){alloc::borrow::Cow::Borrowed("image(transparent)")}else{selected};
            let output=output.get_or_insert_with(String::new);
            output.try_reserve(token.start-copied+selected.len()).ok()?;
            output.push_str(&raw[copied..token.start]);output.push_str(&selected);
            copied=block.after;cursor.position=block.after;
        }
        if let Some(mut output)=output {output.try_reserve(raw.len()-copied).ok()?;output.push_str(&raw[copied..]);
            (output.len()<=MAX_CSS_BYTES).then_some(alloc::borrow::Cow::Owned(output))
        }else{Some(alloc::borrow::Cow::Borrowed(raw))}
    }
    select(raw,scheme,0)
}

/// The UA palette is shared by named-color resolution, initial foreground,
/// form controls and the canvas. Author color functions keep precise channels.
pub fn system_color(name:&str,scheme:UsedColorScheme)->Option<Rgba> {
    let dark=scheme==UsedColorScheme::Dark;
    let (r,g,b)=match name {
        "canvas"|"field"=>if dark{(18,18,18)}else{(255,255,255)},
        "canvastext"|"buttontext"|"fieldtext"|"highlighttext"|"selecteditemtext"|"buttonborder"|"marktext"=>if dark{(255,255,255)}else{(0,0,0)},
        "linktext"=>if dark{(158,158,255)}else{(0,0,238)},
        "visitedtext"=>if dark{(208,173,240)}else{(85,26,139)},
        "activetext"=>(255,0,0),
        "buttonface"|"threedface"=>if dark{(51,51,51)}else{(239,239,239)},
        "graytext"=>(128,128,128),
        "highlight"=>if dark{(38,79,120)}else{(180,215,254)},
        "selecteditem"=>if dark{(38,79,120)}else{(180,215,255)},
        "accentcolor"=>(0,117,255),"accentcolortext"=>(255,255,255),"mark"=>(255,255,0),
        _=>return None,
    };
    Some(Rgba{r,g,b,a:255})
}

/// HTML metadata uses the first valid candidate in actual document tree order.
/// Shadow/template/detached subtrees are excluded by the canonical iterator.
pub(crate) fn document_support(document:&Document)->Result<ColorSchemeSupport,crate::Error> {
    let root=document.root();let mut node=root;
    while let Some(next)=crate::selector::next_descendant(document,root,node)? {
        node=next;
        if let NodeKind::Element{namespace:Namespace::Html,name,attributes}=document.kind(node)? {
            if crate::svg::local_name(name)=="meta" && crate::svg::attribute(attributes,"name").is_some_and(|name|name.eq_ignore_ascii_case("color-scheme")) {
                if let Some(value)=crate::svg::attribute(attributes,"content").and_then(ColorSchemeSupport::parse){return Ok(value);}
            }
        }
    }
    Ok(ColorSchemeSupport::default())
}
pub(crate) fn subtree_has_meta(document:&Document,root:NodeId)->bool {
    let mut node=Some(root);
    while let Some(current)=node {
        if matches!(document.kind(current),Ok(NodeKind::Element{namespace:Namespace::Html,name,..}) if crate::svg::local_name(name)=="meta"){return true;}
        node=match crate::selector::next_descendant(document,root,current){Ok(node)=>node,Err(_)=>return true};
    }
    false
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn specification_branch_selection_freezes_only_selected_dependencies_and_opaque_tokens() {
        let raw=r#"cross-fade(light-dark(linear-gradient(currentColor, red), url("light-dark(a,b)")), light-dark(none, linear-gradient(blue, white)))"#;
        let selected=select_branches(raw,UsedColorScheme::Dark).unwrap();
        assert_eq!(selected,r#"cross-fade(url("light-dark(a,b)"), linear-gradient(blue, white))"#);
        assert!(!color_values::uses_current_color(&selected));
        assert_eq!(select_branches(r"LIGHT-DARK(red, blue)",UsedColorScheme::Dark).unwrap(),"blue");
        assert_eq!(select_branches(r"l\69 ght-dark(red, blue)",UsedColorScheme::Light).unwrap(),"red");
        assert!(select_branches("light-dark(red)",UsedColorScheme::Light).is_none());
        assert_eq!(select_branches("light-dark(none,url(dark.png))",UsedColorScheme::Light).unwrap(),"image(transparent)");
    }
    #[test]
    fn specification_color_scheme_negotiates_order_preferences_only_and_unknown_identifiers() {
        for (support,preference,expected,defaulted) in [
            ("normal",ColorSchemePreference::Dark,UsedColorScheme::Light,true),
            ("dark light",ColorSchemePreference::None,UsedColorScheme::Dark,false),
            ("light dark",ColorSchemePreference::Dark,UsedColorScheme::Dark,false),
            ("only light",ColorSchemePreference::Dark,UsedColorScheme::Light,false),
            ("unknown",ColorSchemePreference::Dark,UsedColorScheme::Light,true),
            ("unknown dark",ColorSchemePreference::None,UsedColorScheme::Dark,false),
            (r"foo\ dark light",ColorSchemePreference::Dark,UsedColorScheme::Light,false),
        ] {
            assert_eq!(ColorSchemeSupport::parse(support).unwrap().resolve(preference,UsedColorScheme::Light),UsedScheme{scheme:expected,defaulted},"{support}");
        }
    }
}
