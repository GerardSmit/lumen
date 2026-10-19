//! CSS Fonts' family-qualified aliases, shared by parsing, CSSOM and shaping.
use super::*;

pub const MAX_FEATURE_ALIASES:usize=1024;
pub const MAX_FEATURE_INDICES:usize=128;
pub const MAX_FEATURE_METADATA_BYTES:usize=1024*1024;
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
#[repr(u8)]
pub enum FeatureKind { Stylistic, HistoricalForms, Styleset, CharacterVariant, Swash, Ornaments, Annotation }
impl FeatureKind {
    pub const ALL:[Self;7]=[Self::Stylistic,Self::HistoricalForms,Self::Styleset,Self::CharacterVariant,Self::Swash,Self::Ornaments,Self::Annotation];
    pub fn name(self)->&'static str { match self {Self::Stylistic=>"stylistic",Self::HistoricalForms=>"historical-forms",Self::Styleset=>"styleset",Self::CharacterVariant=>"character-variant",Self::Swash=>"swash",Self::Ornaments=>"ornaments",Self::Annotation=>"annotation"} }
    pub fn parse(name:&str)->Option<Self>{Self::ALL.into_iter().find(|kind|kind.name().eq_ignore_ascii_case(name))}
    pub fn maximum(self)->usize{match self{Self::Styleset=>MAX_FEATURE_INDICES,Self::CharacterVariant=>2,_=>1}}
}
#[derive(Clone,Debug,PartialEq)]
pub struct FeatureAlias {pub kind:FeatureKind,pub name:Arc<str>,pub indices:Arc<[u32]>}
#[derive(Clone,Debug,PartialEq)]
pub struct AlternateRequest {pub kind:FeatureKind,pub names:Arc<[Arc<str>]>}
#[derive(Clone,Debug,PartialEq)]
pub struct Alternates {pub historical:bool,pub requests:Arc<[AlternateRequest]>}
impl Alternates {
    pub fn retained_bytes(&self)->usize{core::mem::size_of_val(self.requests.as_ref())+self.requests.iter().map(|request|core::mem::size_of_val(request.names.as_ref())+request.names.iter().map(|name|name.len()).sum::<usize>()).sum::<usize>()}
}
fn identifier(raw:&str)->Option<Arc<str>>{
    // Feature names use <ident>, not <custom-ident>; CSS-wide keywords are
    // excluded only when they are the entire property's value.
    let parts=components(raw)?;
    if parts.len()!=1{return None;}
    let raw=parts[0];
    let mut position=0;let name=consume_selector_identifier(raw,&mut position)?;
    (position==raw.len()).then(||Arc::from(name))
}
pub fn parse_alternates(raw:&str)->Option<Option<Arc<Alternates>>>{
    let tokens=components(raw)?;
    if tokens.len()==1 && identifier(tokens[0]).is_some_and(|name|name.eq_ignore_ascii_case("normal")){return Some(None);}
    let mut historical=false;let mut requests=Vec::new();
    for token in tokens {
        let mut position=0;let name=consume_selector_identifier(token,&mut position)?;
        if position==token.len() && name.eq_ignore_ascii_case("historical-forms") {if historical{return None;}historical=true;continue;}
        let body=token.get(position..)?.strip_prefix('(')?;let kind=FeatureKind::parse(&name)?;
        if kind==FeatureKind::HistoricalForms || requests.iter().any(|request:&AlternateRequest|request.kind==kind){return None;}
        let body=body.strip_suffix(')')?;
        let names=comma_components(body,MAX_FEATURE_INDICES)?.into_iter().map(|name|identifier(name.trim())).collect::<Option<Vec<_>>>()?;
        if names.is_empty() || (!matches!(kind,FeatureKind::Styleset|FeatureKind::CharacterVariant) && names.len()!=1){return None;}
        requests.push(AlternateRequest{kind,names:names.into()});
    }
    if !historical && requests.is_empty(){return None;}
    requests.sort_by_key(|request|request.kind as u8);
    Some(Some(Arc::new(Alternates{historical,requests:requests.into()})))
}
pub fn serialize_alternates(value:Option<&Alternates>)->String{
    let Some(value)=value else{return "normal".into();};
    let mut parts=Vec::new();
    for kind in FeatureKind::ALL {
        if kind==FeatureKind::HistoricalForms {if value.historical {parts.push(String::from("historical-forms"));}continue;}
        if let Some(request)=value.requests.iter().find(|request|request.kind==kind) {parts.push(alloc::format!("{}({})",request.kind.name(),request.names.iter().map(|name|serialize_identifier(name)).collect::<Vec<_>>().join(", ")));}
    }
    parts.join(" ")
}
pub fn parse_alias_blocks(input:&str)->Result<Arc<[FeatureAlias]>,CssError>{
    let mut aliases:Vec<FeatureAlias>=Vec::new();
    let mut positions:alloc::collections::BTreeMap<(u8,Arc<str>),usize>=alloc::collections::BTreeMap::new();
    for (start,_end,open,close) in nesting::source_rule_ranges(input,true)? {
        let (Some(open),Some(close))=(open,close) else{continue;};
        let prelude=input[start..open].trim();let Some(name)=prelude.strip_prefix('@') else{continue;};
        let Some(name)=identifier(name) else{continue;};
        let Some(kind)=FeatureKind::parse(&name) else{continue;};
        for (start,end) in declaration_spans_with_recovery(&input[open+1..close],true)? {
            let Some((name,value))=declaration_pair(&input[open+1+start..open+1+end]) else{continue;};
            let Some(name)=identifier(name.trim()) else{continue;};
            if important_value(value).1{continue;}
            let Some(parts)=components(value) else{continue;};
            if kind==FeatureKind::Styleset && parts.len()>MAX_FEATURE_INDICES {return Err(CssError{offset:open,message:"too many font feature indices"});}
            if parts.is_empty() || parts.len()>kind.maximum(){continue;}
            let mut indices=Vec::new();let mut valid=true;
            for part in parts {
                // Reuse the canonical integer-feature grammar, including constant math.
                if !math_function(part) && !part.strip_prefix(['+', '-']).unwrap_or(part).bytes().all(|byte|byte.is_ascii_digit()) {valid=false;break;}
                let Some(settings)=parse_font_feature_settings(&alloc::format!("\"test\" {part}")) else{valid=false;break;};
                let Some(settings)=resolve_font_feature_settings(settings.as_ref(),ContainerUnitContext::default()).flatten() else{valid=false;break;};
                let Some(values)=settings.resolved() else{valid=false;break;};
                indices.push(values[0].value);
            }
            if !valid{continue;}
            if let Some(&at)=positions.get(&(kind as u8,name.clone())) {aliases[at].indices=indices.into();}
            else {if aliases.len()>=MAX_FEATURE_ALIASES{return Err(CssError{offset:open,message:"too many font feature aliases"});}positions.insert((kind as u8,name.clone()),aliases.len());aliases.push(FeatureAlias{kind,name,indices:indices.into()});}
        }
    }
    if aliases.iter().try_fold(0usize,|bytes,alias|bytes.checked_add(alias.name.len())?.checked_add(core::mem::size_of_val(alias.indices.as_ref())))
        .is_none_or(|bytes|bytes>MAX_FEATURE_METADATA_BYTES) {return Err(CssError{offset:0,message:"font feature metadata exceeds limit"});}
    Ok(aliases.into())
}
/// Names defined in an outer tree are considered only after the reference's
/// originating tree, which stays attached when font-family is inherited.
pub fn scope_rank(scope:Option<NodeId>,chain:Option<&[NodeId]>)->Option<usize>{
    match scope {Some(scope)=>chain?.iter().position(|root|*root==scope),None=>Some(chain.map_or(0,<[NodeId]>::len))}
}
pub fn matching_scope(family:&str,rules:&[FontFamilyDisplayRule],environment:MediaEnvironment,chain:Option<&[NodeId]>)->Option<usize>{
    rules.iter().filter(|rule|rule.applies(environment)&&rule.families.iter().any(|name|name.eq_ignore_ascii_case(family)))
        .filter_map(|rule|scope_rank(rule.scope,chain)).min()
}
pub fn resolve_features(value:Option<&Alternates>,family:&str,rules:&[FontFamilyDisplayRule],environment:MediaEnvironment,chain:Option<&[NodeId]>)->Vec<FontFeature>{
    let Some(value)=value else{return Vec::new();};
    let nearest=matching_scope(family,rules,environment,chain);
    let mut aliases=Vec::new();
    for request in value.requests.iter(){for name in request.names.iter(){
        if let Some(alias)=rules.iter().enumerate().filter(|(_,rule)|scope_rank(rule.scope,chain)==nearest && rule.applies(environment)
            && rule.families.iter().any(|family_name|family_name.eq_ignore_ascii_case(family)))
            .filter_map(|(source_order,rule)|rule.aliases.iter().find(|alias|alias.kind==request.kind && &alias.name==name)
                .map(|alias|((rule.cascade_layer_order(),source_order),alias)))
            .max_by_key(|(priority,_)|*priority).map(|(_,alias)|alias) {aliases.push(alias.clone());}
    }}
    selected_features(Some(value),&aliases)
}
/// Variant features precede explicit font-feature-settings in the backend.
pub fn selected_features(value:Option<&Alternates>,aliases:&[FeatureAlias])->Vec<FontFeature>{
    let Some(value)=value else{return Vec::new();};let mut result=Vec::new();
    if value.historical{result.push(FontFeature{tag:*b"hist",value:1});}
    for request in value.requests.iter(){for name in request.names.iter(){
        let Some(alias)=aliases.iter().find(|alias|alias.kind==request.kind && &alias.name==name) else{continue;};
        let Some(&first)=alias.indices.first() else{continue;};
        let mut add=|tag,value|{if let Some(existing)=result.iter_mut().find(|feature|feature.tag==tag){existing.value=value;}else{result.push(FontFeature{tag,value});}};
        match request.kind {
            FeatureKind::Stylistic=>add(*b"salt",first),
            FeatureKind::Swash=>{add(*b"swsh",first);add(*b"cswh",first);},
            FeatureKind::Ornaments=>add(*b"ornm",first),
            FeatureKind::Annotation=>add(*b"nalt",first),
            FeatureKind::HistoricalForms=>{},
            FeatureKind::Styleset=>for &index in alias.indices.iter(){if (1..=99).contains(&index){add([b's',b's',b'0'+(index/10)as u8,b'0'+(index%10)as u8],1);}},
            FeatureKind::CharacterVariant=>if (1..=99).contains(&first){add([b'c',b'v',b'0'+(first/10)as u8,b'0'+(first%10)as u8],alias.indices.get(1).copied().unwrap_or(1));},
        }
    }}
    result
}
pub fn serialize_family_names(families:&[Arc<str>])->String {
    serialize_font_families(&families.iter().cloned().map(FontFamily::Named).collect::<Vec<_>>())
}
pub fn serialize_rule(families:&[Arc<str>],display:Option<FontDisplay>,aliases:&[FeatureAlias])->String{
    let mut text=alloc::format!("@font-feature-values {} {{",serialize_family_names(families));
    if let Some(display)=display {let mut descriptor=FontFaceDescriptors::default();descriptor.display=display;text.push_str(&alloc::format!(" font-display: {};",descriptor.get("display").unwrap()));}
    for kind in FeatureKind::ALL {let mut values=aliases.iter().filter(|alias|alias.kind==kind).collect::<Vec<_>>();if values.is_empty(){continue;}values.sort_by(|a,b|a.name.cmp(&b.name));text.push_str(&alloc::format!(" @{} {{",kind.name()));for alias in values{text.push_str(&alloc::format!(" {}: {};",serialize_identifier(&alias.name),alias.indices.iter().map(ToString::to_string).collect::<Vec<_>>().join(" ")));}text.push_str(" }");}
    text.push_str(" }");text
}


#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn specification_font_feature_aliases_escaped_delimiters_survive_source_roundtrip_and_selection() {
        let parsed=parse_stylesheet(r"@font-feature-values Fancy{@stylistic{\:first:2;middle:3;\3a last:4}} ").unwrap();
        let aliases=&parsed.family_display[0].aliases;
        assert_eq!(aliases.iter().map(|alias|alias.name.as_ref()).collect::<Vec<_>>(),[":first","middle",":last"]);
        let text=serialize_rule(&parsed.family_display[0].families,None,aliases);
        let again=parse_stylesheet(&text).unwrap();
        assert_eq!(again.family_display[0].aliases.iter().map(|alias|alias.name.as_ref()).collect::<Vec<_>>(),[":first",":last","middle"],"canonical serialization sorts aliases");
        for alias in aliases.iter() {
            assert_eq!(again.family_display[0].aliases.iter().find(|next|next.name==alias.name),Some(alias));
        }
        let request=parse_alternates(r"stylistic(\:last)").unwrap().unwrap();
        assert_eq!(resolve_features(Some(&request),"Fancy",&again.family_display,MediaEnvironment::default(),None),[FontFeature{tag:*b"salt",value:4}]);
    }

    #[test]
    fn specification_font_feature_aliases_follow_shared_layers_conditions_and_family_display() {
        let mut groups=[parse_stylesheet(r#"
            @layer one,two,three;
            @font-face{font-family:Fancy;src:url(fancy.otf)}
            @layer three{@font-feature-values Fancy{font-display:fallback;@stylistic{pick:3}@styleset{foo:1;bar:1}}}
            @layer one{@font-feature-values Fancy{font-display:block;@stylistic{pick:1}@styleset{foo:2;bar:2;baz:2}}}
            @layer two{@font-feature-values Fancy{@styleset{baz:3}}}
        "#).unwrap(),parse_stylesheet(r#"
            @layer one{@font-feature-values Fancy{@stylistic{pick:2}}}
            @media(min-width:900px){@font-feature-values Fancy{@stylistic{pick:9}}}
        "#).unwrap()];
        assert_eq!(groups[0].layers.as_ref(), &[String::from("one"),String::from("two"),String::from("three")], "first sheet retains explicit ordering statement");
        assert_eq!(groups[1].layers.as_ref(), &[String::from("one")], "second sheet retains local named layer identity");
        assert_eq!(groups[0].family_display.iter().map(|rule|rule.layer).collect::<Vec<_>>(), [Some(2),Some(0),Some(1)], "font aliases keep sheet-local layer indices");
        assert_eq!(groups[1].family_display.iter().map(|rule|rule.layer).collect::<Vec<_>>(), [Some(0),None], "conditional unlayered aliases stay distinct from named layer");
        let faces=canonicalize_font_faces(&mut groups);
        assert_eq!(groups[0].family_display.iter().map(|rule|rule.layer_path.map(|path|path[0])).collect::<Vec<_>>(), [Some(3),Some(1),Some(2)], "first sheet canonical layer paths");
        assert_eq!(groups[1].family_display.iter().map(|rule|rule.layer_path.map(|path|path[0])).collect::<Vec<_>>(), [Some(1),None], "repeated named layer uses its first global rank");
        let environment=MediaEnvironment{width:800.,height:600.,..MediaEnvironment::default()};
        let request=parse_alternates("stylistic(pick) styleset(foo,bar,baz)").unwrap().unwrap();
        let features=resolve_features(Some(&request),"Fancy",&faces[0].family_display,environment,None);
        assert_eq!(features,[FontFeature{tag:*b"salt",value:3},FontFeature{tag:*b"ss01",value:1},FontFeature{tag:*b"ss03",value:1}]);
        assert_eq!(faces[0].effective_display(environment),FontDisplay::Fallback);
        let wide=MediaEnvironment{width:1000.,..environment};
        assert_eq!(resolve_features(Some(&request),"Fancy",&faces[0].family_display,wide,None)[0],FontFeature{tag:*b"salt",value:9});
        let mut nested=parse_stylesheet("@layer outer{ @layer early,late; @layer late{@font-feature-values Fancy{@stylistic{pick:4}}} @layer early{@font-feature-values Fancy{@stylistic{pick:1}}}}").unwrap();
        assert_eq!(resolve_features(Some(&request),"Fancy",&nested.family_display,environment,None)[0],FontFeature{tag:*b"salt",value:4});
        // Recanonicalizing the same rule graph must preserve anonymous layer
        // ordering and the source order of repeated names within one layer.
        nested=parse_stylesheet("@layer{@font-feature-values Fancy{@stylistic{pick:5}}}@layer{@font-feature-values Fancy{@stylistic{pick:6}}}").unwrap();
        canonicalize_font_faces(core::slice::from_mut(&mut nested));
        assert_eq!(resolve_features(Some(&request),"Fancy",&nested.family_display,environment,None)[0],FontFeature{tag:*b"salt",value:6});
    }
    #[test]
    fn specification_font_feature_descriptors_preserve_adjacent_alias_blocks() {
        let aliases=parse_alias_blocks("font-display: fallback; @stylistic { pick: 3 } font-display: swap; @styleset { chosen: 1 3 } font-display: optional;").unwrap();
        assert_eq!(aliases.len(),2,"descriptors before, between and after feature blocks remain separate source items");
        assert_eq!(aliases[0].kind,FeatureKind::Stylistic);
        assert_eq!(aliases[0].name.as_ref(),"pick");
        assert_eq!(aliases[0].indices.as_ref(),&[3]);
        assert_eq!(aliases[1].kind,FeatureKind::Styleset);
        assert_eq!(aliases[1].indices.as_ref(),&[1,3]);
    }
    #[test]
    fn specification_font_feature_alias_identifiers_decode_keywords_functions_and_comments() {
        let names=["initial","inherit","unset","revert","revert-layer","default"];
        let aliases=parse_alias_blocks(r"@\73 tylistic { initial: 1; inherit: 2; unset: 3; revert: 4; revert-layer: 5; default: 6; \43 ase: 7; }").unwrap();
        assert_eq!(aliases.len(),7);
        for (index,name) in names.into_iter().enumerate() {
            let request=parse_alternates(&alloc::format!("stylistic({name})")).unwrap().unwrap();
            assert_eq!(selected_features(Some(&request),&aliases),[FontFeature{tag:*b"salt",value:index as u32+1}]);
            assert!(parse_alternates(name).is_none(),"CSS-wide keywords are not a standalone alternates value");
        }
        let request=parse_alternates(r"\73 tylistic(/**/\43 ase/**/) /**/ \68 istorical-forms").unwrap().unwrap();
        assert_eq!(serialize_alternates(Some(&request)),"stylistic(Case) historical-forms");
        assert_eq!(selected_features(Some(&request),&aliases),[FontFeature{tag:*b"hist",value:1},FontFeature{tag:*b"salt",value:7}]);
        assert_eq!(parse_alternates(r"/**/\6e ormal/**/").unwrap(),None);
        assert!(parse_alternates("stylistic/**/(Case)").is_none(),"a comment separates the identifier from a function token");
        assert!(parse_alternates("stylistic(Ca/**/se)").is_none(),"comments cannot concatenate distinct alias identifiers");
        assert!(identifier("1name").is_none());
        assert!(identifier("two names").is_none());
    }
    #[test]
    fn specification_font_feature_aliases_resolve_active_family_scopes_and_unsigned_indices() {
        let parsed=parse_stylesheet(r#"
          @font-feature-values Fancy, "serif" {
            @stylistic { Case: 1; Case: 3; ignored: -1; }
            @swash { both: 2; invalid: 1 2; }
            @styleset { sets: 1 9 99 0 100; }
            @character-variant { chosen: 4 2; implicit: 5; invalid: 1 2 3; }
          }
          @font-feature-values Fancy { @stylistic { Case: 4; } }
          @media (min-width: 900px) { @font-feature-values Fancy { @stylistic { Case: 7; } } }
        "#).unwrap();
        assert_eq!(parsed.family_display.len(),3);
        let alternates=parse_alternates("historical-forms stylistic(Case) swash(both) styleset(sets) character-variant(chosen, implicit)").unwrap().unwrap();
        let environment=MediaEnvironment{width:800.,height:600.,..MediaEnvironment::default()};
        let actual=resolve_features(Some(&alternates),"Fancy",&parsed.family_display,environment,None);
        for (tag,value) in [(*b"hist",1),(*b"salt",4),(*b"swsh",2),(*b"cswh",2),(*b"ss01",1),(*b"ss09",1),(*b"ss99",1),(*b"cv04",2),(*b"cv05",1)] {
            assert!(actual.contains(&FontFeature{tag,value}),"family alias resolves its actual OpenType tag/value");
        }
        assert!(!actual.iter().any(|feature|feature.tag==*b"ss00"));
        assert_eq!(resolve_features(parse_alternates("stylistic(case)").unwrap().as_deref(),"Fancy",&parsed.family_display,environment,None),[]);
        assert!(parse_alternates("stylistic(a) stylistic(b)").is_none());
        assert!(parse_alternates("swash(a, b)").is_none());
        assert!(parse_alternates("styleset(inherit)").is_some());
        let mut document=Document::new(4);
        let scope=document.create(crate::NodeKind::Text("scope".into())).unwrap();
        let chain=[scope];
        let mut scoped=parsed.family_display.clone();
        let mut inner=scoped[1].clone();inner.scope=Some(scope);inner.aliases=parse_alias_blocks("@stylistic { Case: 2; }").unwrap();scoped.push(inner);
        assert_eq!(resolve_features(parse_alternates("stylistic(Case)").unwrap().as_deref(),"Fancy",&scoped,environment,Some(&chain)),[FontFeature{tag:*b"salt",value:2}]);
        assert_eq!(resolve_features(parse_alternates("swash(both)").unwrap().as_deref(),"Fancy",&scoped,environment,Some(&chain)),[],"nearest family definition prevents searching an outer alias table");
        assert_eq!(resolve_features(parse_alternates("stylistic(Case)").unwrap().as_deref(),"Fancy",&scoped,environment,None),[FontFeature{tag:*b"salt",value:4}],"an inherited outer font-family retains its reference scope");
    }
}
