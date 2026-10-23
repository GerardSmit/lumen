//! Bounded specified declaration storage shared by CSSOM owners.
//! Shorthand grammar comes from the author parser; computed Style is never used
//! to reconstruct relative specified values.
use super::*;
use alloc::collections::BTreeSet;

const MAX_ENTRIES: usize = MAX_DECLARATIONS;
/// Serialization keeps this many entry views on the stack; longer blocks use the heap.
const LOCAL_ENTRIES: usize = 128;
/// Sources beyond this many spans compact wide-keyword longhands into `all` cohorts.
const COMPACT_WIDE_SPANS: usize = 128;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Pending {
    pub(super) property: Arc<str>,
    pub(super) value: Arc<str>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Specified {
    Text(Arc<str>),
    Pending(Arc<Pending>),
    All(Arc<AllGroup>),
}

// A reset cohort stores one shared value and a bounded registry bitset.
// CSSOM enumeration borrows static names; sparse overrides split only ranges.
#[derive(Clone, Debug, Eq, PartialEq)]
struct AllGroup { targets: Vec<u64>, value: Specified, reset_metadata: bool }
impl AllGroup {
    fn contains(&self,index:usize)->bool { self.targets.get(index/64).is_some_and(|word|word & (1<<(index%64))!=0) }
    fn clear(&mut self,index:usize) { self.targets[index/64]&=!(1<<(index%64)); }
    fn empty(&self)->bool { self.targets.iter().all(|word|*word==0) }
    fn full(value:Specified)->Result<Self,CssError> {
        let mut targets=Vec::new();targets.try_reserve_exact(PROPERTIES.len().div_ceil(64)).map_err(|_|limit())?;
        targets.resize(PROPERTIES.len().div_ceil(64),0);
        for(index,property)in PROPERTIES.iter().enumerate(){if property.in_all && longhands(property.name).is_empty() && canonical_alias(property.name)==property.name{targets[index/64]|=1<<(index%64);}}
        Ok(Self{targets,value,reset_metadata:true})
    }
}
fn all_target(name:&str)->bool {
    registry_index(name).map(|index|&PROPERTIES[index]).is_some_and(|property|
        property.in_all && longhands(name).is_empty() && canonical_alias(name)==name)
}
fn same_specified(left:&Specified,right:&Specified)->bool{
    match(left,right){
        (Specified::Text(left),Specified::Text(right))=>left==right,
        (Specified::Pending(left),Specified::Pending(right))=>Arc::ptr_eq(left,right),
        _=>false,
    }
}
fn compact_all_runs(entries:&mut Vec<Entry>){
    let mut position=1;
    while position<entries.len(){
        let merge=match(&entries[position-1].value,&entries[position].value){
            (Specified::All(left),Specified::All(right))=>entries[position-1].important==entries[position].important
                &&left.reset_metadata==right.reset_metadata&&same_specified(&left.value,&right.value)
                &&(0..PROPERTIES.len()).rev().find(|index|left.contains(*index))
                    <(0..PROPERTIES.len()).find(|index|right.contains(*index)),
            _=>false,
        };
        if merge{
            let right=entries.remove(position);
            let Specified::All(right)=right.value else{unreachable!()};
            let Specified::All(left)=&mut entries[position-1].value else{unreachable!()};
            for(target,right)in Arc::make_mut(left).targets.iter_mut().zip(&right.targets){*target|=right;}
        }else{position+=1;}
    }
}
// An aggregate compatibility slot is not an additional authored declaration.
// Project it only while a complete corresponding component family remains in
// this cohort. Sparse components otherwise inherit the ordinary UA fallback.
fn compatibility_slot_covered(group:&AllGroup,slot:usize)->bool{
    PROPERTIES.iter().filter(|property|property.in_all&&property.ids.contains(&slot)).any(|aggregate|{
        let mut found=false;
        for(index,component)in PROPERTIES.iter().enumerate(){
            if !component.in_all||!longhands(component.name).is_empty()||canonical_alias(component.name)!=component.name||!component.ids.iter().any(|id|*id!=slot&&aggregate.ids.contains(id)){continue;}
            found=true;if !group.contains(index){return false;}
        }
        found
    })
}

// CSSOM set-a-declaration moves an updated declaration after declarations in
// its logical property group with a different mapping logic. Its virtual
// position within a compact all cohort must obey that same observable rule.
fn logical_mapping(name:&str)->Option<(u8,bool)> {
    let logical=name.contains("-inline-")||name.contains("-block-")||name.ends_with("inline-size")||name.ends_with("block-size")
        ||name.starts_with("border-start-")||name.starts_with("border-end-");
    if name.starts_with("border-image-"){return None;}
    let group=if matches!(name,"width"|"height"|"inline-size"|"block-size"){1}
        else if matches!(name,"min-width"|"min-height"|"min-inline-size"|"min-block-size"){2}
        else if matches!(name,"max-width"|"max-height"|"max-inline-size"|"max-block-size"){3}
        else if name.starts_with("margin-"){4}
        else if name.starts_with("padding-"){5}
        else if matches!(name,"top"|"right"|"bottom"|"left")||name.starts_with("inset-"){6}
        else if name.starts_with("border-")&&name.ends_with("-width"){7}
        else if name.starts_with("border-")&&name.ends_with("-style"){8}
        else if name.starts_with("border-")&&name.ends_with("-color"){9}
        else if name.starts_with("border-")&&name.ends_with("-radius"){10}
        else{return None;};
    Some((group,logical))
}
fn different_mapping(left:&str,right:&str)->bool {
    match(logical_mapping(left),logical_mapping(right)){
        (Some((a,x)),Some((b,y)))=>a==b&&x!=y,_=>false,
    }
}
fn order_updated_declaration(entries:&mut Vec<Entry>,name:&str)->Result<(),CssError>{
    let Some(position)=entries.iter().position(|entry|!matches!(entry.value,Specified::All(_))&&entry.name.as_ref()==name)else{return Ok(());};
    let mut destination=None;
    for(index,entry)in entries.iter().enumerate().skip(position+1){
        if let Specified::All(group)=&entry.value {
            if let Some(target)=(0..PROPERTIES.len()).rev().find(|target|group.contains(*target)&&different_mapping(name,PROPERTIES[*target].name)){
                destination=Some((index,Some(target)));
            }
        }else if different_mapping(name,&entry.name){destination=Some((index,None));}
    }
    let Some((destination,target))=destination else{return Ok(());};
    entries.try_reserve(2).map_err(|_|limit())?;
    let entry=entries.remove(position);let destination=destination-1;
    if let Some(target)=target {
        let old=entries.remove(destination);
        let Specified::All(group)=old.value else{unreachable!()};
        let mut before=(*group).clone();let mut after=before.clone();
        for index in 0..PROPERTIES.len(){if index>target{before.clear(index);}if index<=target{after.clear(index);}}
        let mut insertion=destination;
        if !before.empty(){entries.insert(insertion,Entry{name:Arc::from("all"),value:Specified::All(Arc::new(before)),important:old.important});insertion+=1;}
        entries.insert(insertion,entry);insertion+=1;
        if !after.empty(){entries.insert(insertion,Entry{name:Arc::from("all"),value:Specified::All(Arc::new(after)),important:old.important});}
    }else{entries.insert(destination+1,entry);}
    Ok(())
}

#[derive(Clone,Copy)]
struct EntryView<'a>{name:&'a str,value:&'a Specified,important:bool}
struct EntryViews<'a>{entry:&'a Entry,position:usize}
impl<'a> Iterator for EntryViews<'a>{
    type Item=EntryView<'a>;
    fn next(&mut self)->Option<Self::Item>{
        if let Specified::All(group)=&self.entry.value {
            while self.position<PROPERTIES.len(){let index=self.position;self.position+=1;
                if group.contains(index){return Some(EntryView{name:PROPERTIES[index].name,value:&group.value,important:self.entry.important});}}
            None
        }else if self.position==0{self.position=1;Some(EntryView{name:&self.entry.name,value:&self.entry.value,important:self.entry.important})}else{None}
    }
}
fn entry_views(entry:&Entry)->EntryViews<'_>{EntryViews{entry,position:0}}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Entry {
    name: Arc<str>,
    value: Specified,
    important: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DeclarationBlock {
    entries: Vec<Entry>,
}

fn push_text_entries<N: AsRef<str>, V: AsRef<str>>(
    entries: &mut Vec<Entry>,
    values: impl IntoIterator<Item = (N, V)>,
    important: bool,
) {
    for (name, value) in values {
        entries.push(Entry {
            name: Arc::from(name.as_ref()),
            value: Specified::Text(Arc::from(value.as_ref())),
            important,
        });
    }
}

fn limit() -> CssError {
    CssError {
        offset: 0,
        message: "CSS declaration block exceeds limit",
    }
}

/// CSS longhand identities, deliberately separate from renderer state slots.
/// A longhand can update several internal slots, while aggregate slots are not
/// author-visible CSS properties.
const FLAG_KNOWN: u8 = 1;
const FLAG_SHORTHAND: u8 = 2;
const FLAG_ALIAS: u8 = 4;
const NAME_FLAG_SLOTS: usize = 1024;
const _: () = assert!(PROPERTY_NAMES.len() <= NAME_FLAG_SLOTS);
static NAME_FLAGS: [core::sync::atomic::AtomicU8; NAME_FLAG_SLOTS] = [const { core::sync::atomic::AtomicU8::new(0) }; NAME_FLAG_SLOTS];

/// Shorthand and alias facts of a registered name, computed once per name.
fn name_flags(name: &str) -> Option<u8> {
    use core::sync::atomic::Ordering::Relaxed;
    let cell = NAME_FLAGS.get(registry_index(name)?)?;
    let flags = cell.load(Relaxed);
    if flags != 0 {
        return Some(flags);
    }
    let flags = FLAG_KNOWN
        | if longhands_uncached(name).is_empty() { 0 } else { FLAG_SHORTHAND }
        | if canonical_alias_uncached(name) == name { 0 } else { FLAG_ALIAS };
    cell.store(flags, Relaxed);
    Some(flags)
}

pub fn has_longhands(name: &str) -> bool {
    match name_flags(name) {
        Some(flags) => flags & FLAG_SHORTHAND != 0,
        None => !longhands_uncached(name).is_empty(),
    }
}

pub fn longhands(name: &str) -> &'static [&'static str] {
    if name_flags(name).is_some_and(|flags| flags & FLAG_SHORTHAND == 0) {
        return &[];
    }
    longhands_uncached(name)
}

fn longhands_uncached(name: &str) -> &'static [&'static str] {
    match name {
        "columns"=>&["column-width","column-count","column-height","column-wrap"],
        "page-break-before"=>&["break-before"],"page-break-after"=>&["break-after"],"page-break-inside"=>&["break-inside"],
        "contain-intrinsic-size" => &["contain-intrinsic-width","contain-intrinsic-height"],
        "transition" => &["transition-property","transition-duration","transition-timing-function","transition-delay","transition-behavior"],
        "text-decoration" => &["text-decoration-line", "text-decoration-style", "text-decoration-color", "text-decoration-thickness"],
        "place-items" => &["align-items", "justify-items"],
        "place-self" => &["align-self", "justify-self"],
        "place-content" => &["align-content", "justify-content"],
        "gap" => &["row-gap", "column-gap"],
        "size" => &["width", "height"],
        "position-try" => &["position-try-order", "position-try-fallbacks"],
        "offset" => &["offset-position", "offset-path", "offset-distance", "offset-rotate", "offset-anchor"],
        "flex-flow" => &["flex-direction", "flex-wrap"],
        "flex" => &["flex-grow", "flex-shrink", "flex-basis"],
        "margin" => &["margin-top", "margin-right", "margin-bottom", "margin-left"],
        "padding" => &[
            "padding-top",
            "padding-right",
            "padding-bottom",
            "padding-left",
        ],
        "inset" => &["top", "right", "bottom", "left"],
        "margin-inline" => &["margin-inline-start", "margin-inline-end"],
        "margin-block" => &["margin-block-start", "margin-block-end"],
        "padding-inline" => &["padding-inline-start", "padding-inline-end"],
        "padding-block" => &["padding-block-start", "padding-block-end"],
        "inset-inline" => &["inset-inline-start", "inset-inline-end"],
        "animation" => &["animation-name","animation-duration","animation-delay-start","animation-timing-function","animation-iteration-count","animation-direction","animation-fill-mode","animation-play-state","animation-composition","animation-timeline","animation-range-start","animation-range-end","animation-delay-end"],
        "animation-range" => &["animation-range-start","animation-range-end"],
        "animation-delay" => &["animation-delay-start","animation-delay-end"],
        "scroll-timeline"=>&["scroll-timeline-name","scroll-timeline-axis"],
        "view-timeline"=>&["view-timeline-name","view-timeline-axis","view-timeline-inset"],
        "inset-block" => &["inset-block-start", "inset-block-end"],
        "overflow" => &["overflow-x", "overflow-y"],
        "grid-template" => &[
            "grid-template-rows",
            "grid-template-columns",
            "grid-template-areas",
        ],
        "grid" => &[
            "grid-template-rows",
            "grid-template-columns",
            "grid-template-areas",
            "grid-auto-rows",
            "grid-auto-columns",
            "grid-auto-flow",
        ],
        "grid-lanes" => &[
            "grid-template-rows",
            "grid-template-columns",
            "grid-template-areas",
            "grid-lanes-direction",
        ],
        "background" => &[
            "background-color",
            "background-image",
            "background-position-x",
            "background-position-y",
            "background-size",
            "background-repeat",
            "background-attachment",
            "background-origin",
            "background-clip",
        ],
        "mask" | "-webkit-mask" => &[
            "mask-image",
            "mask-position",
            "mask-size",
            "mask-repeat",
            "mask-origin",
            "mask-clip",
            "mask-mode",
            "mask-composite",
        ],
        "list-style" => &["list-style-position", "list-style-image", "list-style-type"],
        "outline" => &["outline-color", "outline-style", "outline-width"],
        "border-width" => &[
            "border-top-width",
            "border-right-width",
            "border-bottom-width",
            "border-left-width",
        ],
        "border-style" => &[
            "border-top-style",
            "border-right-style",
            "border-bottom-style",
            "border-left-style",
        ],
        "border-color" => &[
            "border-top-color",
            "border-right-color",
            "border-bottom-color",
            "border-left-color",
        ],
        "border" => &[
            "border-top-width",
            "border-right-width",
            "border-bottom-width",
            "border-left-width",
            "border-top-style",
            "border-right-style",
            "border-bottom-style",
            "border-left-style",
            "border-top-color",
            "border-right-color",
            "border-bottom-color",
            "border-left-color",
            "border-image-source",
            "border-image-slice",
            "border-image-width",
            "border-image-outset",
            "border-image-repeat",
        ],
        "border-image" => &[
            "border-image-source",
            "border-image-slice",
            "border-image-width",
            "border-image-outset",
            "border-image-repeat",
        ],
        "border-top" => &["border-top-width", "border-top-style", "border-top-color"],
        "border-right" => &[
            "border-right-width",
            "border-right-style",
            "border-right-color",
        ],
        "border-bottom" => &[
            "border-bottom-width",
            "border-bottom-style",
            "border-bottom-color",
        ],
        "border-left" => &[
            "border-left-width",
            "border-left-style",
            "border-left-color",
        ],
        "border-inline-width" => &["border-inline-start-width", "border-inline-end-width"],
        "border-inline-style" => &["border-inline-start-style", "border-inline-end-style"],
        "border-inline-color" => &["border-inline-start-color", "border-inline-end-color"],
        "border-block-width" => &["border-block-start-width", "border-block-end-width"],
        "border-block-style" => &["border-block-start-style", "border-block-end-style"],
        "border-block-color" => &["border-block-start-color", "border-block-end-color"],
        "border-inline" => &[
            "border-inline-start-width",
            "border-inline-end-width",
            "border-inline-start-style",
            "border-inline-end-style",
            "border-inline-start-color",
            "border-inline-end-color",
        ],
        "border-block" => &[
            "border-block-start-width",
            "border-block-end-width",
            "border-block-start-style",
            "border-block-end-style",
            "border-block-start-color",
            "border-block-end-color",
        ],
        "border-inline-start" => &[
            "border-inline-start-width",
            "border-inline-start-style",
            "border-inline-start-color",
        ],
        "border-inline-end" => &[
            "border-inline-end-width",
            "border-inline-end-style",
            "border-inline-end-color",
        ],
        "border-block-start" => &[
            "border-block-start-width",
            "border-block-start-style",
            "border-block-start-color",
        ],
        "border-block-end" => &[
            "border-block-end-width",
            "border-block-end-style",
            "border-block-end-color",
        ],
        "column-rule" => &[
            "column-rule-width",
            "column-rule-style",
            "column-rule-color",
        ],
        "font-variant" => &[
            "font-variant-ligatures",
            "font-variant-caps",
            "font-variant-alternates",
            "font-variant-numeric",
            "font-variant-east-asian",
            "font-variant-position",
            "font-variant-emoji",
        ],
        "font" => &[
            "font-style",
            "font-variant-caps",
            "font-weight",
            "font-width",
            "font-size",
            "line-height",
            "font-family",
            "font-size-adjust",
            "font-variant-ligatures",
            "font-variant-alternates",
            "font-feature-settings",
            "font-variant-numeric",
            "font-variant-east-asian",
            "font-variant-position",
            "font-variant-emoji",
            "font-kerning",
            "font-optical-sizing",
            "font-variation-settings",
            "font-language-override",
        ],
        name if super::extra_props::is_shorthand(name) => super::extra_props::shorthand_longhands(name),
        name if super::corner_radii::is_shorthand(name) => super::corner_radii::longhands(name),
        name if super::grid_lines::is_shorthand(name) => super::grid_lines::longhands(name),
        name if super::text_props::is_shorthand(name) => super::text_props::longhands(name),
        _ => &[],
    }
}

/// CSS identity metadata for supported grammar, independent of renderer
/// slot arity and of whether canonical expansion is implemented in this block.
pub fn is_shorthand(name: &str) -> bool {
    !longhands(name).is_empty()
        || matches!(
            name,
            "all"
                | "animation"
                | "background"
                | "background-position"
                | "background-repeat"
                | "border-radius"
                | "-webkit-border-radius"
                | "columns"
                | "font-variant"
                | "grid"
                | "grid-template"
                | "grid-area"
                | "grid-column"
                | "grid-row"
                | "grid-lanes"
                | "text-decoration"
                | "white-space"
        )
}

fn normalized_name(name: &str) -> Option<Arc<str>> {
    if name != name.trim()
        || name.is_empty()
        || name.len() > 256
        || name
            .chars()
            .any(|ch| ch.is_whitespace() || ";:(){}[]\"'".contains(ch))
    {
        return None;
    }
    if name.starts_with("--") {
        decoded_custom_property_name(name).map(Arc::from)
    } else {
        let name = name.to_ascii_lowercase();
        Some(Arc::from(canonical_alias(name.as_str())))
    }
}

pub(super) fn parsed_value(name: &str, raw: &str) -> Result<Option<Vec<Declaration>>, CssError> {
    let Some(completed) = syntax::complete(raw)? else { return Ok(None); };
    let raw = completed.as_ref();
    if raw.len() > MAX_CSS_BYTES
        || (!name.starts_with("--") && raw.trim().is_empty())
        || important_value(raw).1
    {
        return Ok(None);
    }
    let spans = declaration_spans(raw)?;
    if !raw.trim().is_empty() && (spans.len() != 1 || spans[0] != (0, raw.len())) {
        return Ok(None);
    }
    let has_variables = raw
        .as_bytes()
        .windows(4)
        .any(|bytes| bytes.eq_ignore_ascii_case(b"var("))
        && component_values::parse_unparsed_value(raw).is_ok_and(|parts| {
            parts
                .iter()
                .any(|part| matches!(part, component_values::UnparsedComponent::Variable { .. }))
        });
    let owner_dependent=anchor::applies(name,raw)||typed_numeric::substitute_sibling_functions(raw,1,1)
        .is_some_and(|(_,dependent)|dependent)||!name.starts_with("--")&&super::owner_values::may_use(raw);
    let mut parsed=Vec::new();
    append_declaration_pair_values(name,raw,0,has_variables||owner_dependent,&mut parsed)?;
    if parsed.is_empty() || !references_parse(raw, MAX_VARIABLE_BYTES) {
        return Ok(None);
    }
    Ok(Some(parsed))
}

pub(super) fn supports(name: &str, raw: &str) -> bool {
    normalized_name(name).is_some_and(|name| {
        if name.as_ref() == "border-image" || border_image_initial(&name).is_some() {
            expansion(name, raw, false).ok().flatten().is_some()
        } else {
            parsed_value(&name, raw).ok().flatten().is_some()
        }
    })
}

fn push_entry(entries: &mut Vec<Entry>, entry: Entry) -> Result<(), CssError> {
    if entries.len() >= MAX_ENTRIES * 2 + 1 || !matches!(entry.value, Specified::All(_)) && entries.len() >= MAX_ENTRIES && entries.iter().filter(|entry| !matches!(entry.value, Specified::All(_))).count() >= MAX_ENTRIES {
        return Err(limit());
    }
    entries.try_reserve(1).map_err(|_| limit())?;
    entries.push(entry);
    Ok(())
}

fn expansion(name: Arc<str>, raw: &str, important: bool) -> Result<Option<Vec<Entry>>, CssError> {
    let Some(completed) = syntax::complete(raw)? else { return Ok(None); };
    let raw = completed.as_ref().trim();
    let anchored;
    let raw = match anchor::canonicalize(&name, raw) {
        None => raw,
        Some(None) => return Ok(None),
        Some(Some(canonical)) => {
            anchored = canonical;
            anchored.as_str()
        }
    };
    let numeric;
    let raw = match super::numeric_tokens::canonicalize(&name, raw) {
        Some(canonical) => {
            numeric = canonical;
            numeric.as_str()
        }
        None => raw,
    };
    if name.as_ref()=="all" {
        let completed=strip_component_comments(raw,0,true)?;
        let completed=completed.trim();let mut position=0;
        let keyword=consume_selector_identifier(completed,&mut position)
            .filter(|_|position==completed.len()).map(|value|value.to_ascii_lowercase())
            .filter(|value|["initial","inherit","unset","revert","revert-layer"].contains(&value.as_str()));
        let source=keyword.as_deref().unwrap_or(raw);
        let Some(parsed)=parsed_value("all",source)? else{return Ok(None);};

        let value=if parsed.iter().any(|declaration|matches!(declaration.value,Value::Deferred(..))){
            if raw.len()>MAX_VARIABLE_BYTES{return Err(limit());}
            Specified::Pending(Arc::new(Pending{property:name.clone(),value:Arc::from(raw)}))
        }else{
            let Some(wide)=["initial","inherit","unset","revert","revert-layer"].into_iter().find(|keyword|source.eq_ignore_ascii_case(keyword)) else{return Ok(None);};
            Specified::Text(Arc::from(wide))
        };
        return Ok(Some(vec![Entry{name,value:Specified::All(Arc::new(AllGroup::full(value)?)),important}]));
    }
    if name.starts_with("--") {
        if !valid_custom_property_name(&name) || raw.len() > MAX_VARIABLE_BYTES {
            return Ok(None);
        }
        if parsed_value(&name, raw)?.is_none() {
            return Ok(None);
        }
        let mut entries = Vec::new();
        push_entry(
            &mut entries,
            Entry {
                name,
                value: Specified::Text(Arc::from(raw)),
                important,
            },
        )?;
        return Ok(Some(entries));
    }
    // These shorthands request original specified parts from the author parser
    // once, before typed resolution loses relative units or repeat topology.
    if matches!(name.as_ref(), "grid" | "grid-template" | "grid-lanes")
        && !raw
            .as_bytes()
            .windows(4)
            .any(|bytes| bytes.eq_ignore_ascii_case(b"var("))
        && !["initial", "inherit", "unset", "revert", "revert-layer"]
            .iter()
            .any(|keyword| raw.eq_ignore_ascii_case(keyword))
    {
        if raw.len() > MAX_CSS_BYTES || important_value(raw).1 {
            return Ok(None);
        }
        let spans = declaration_spans(raw)?;
        if spans.len() != 1 || spans[0] != (0, raw.len()) {
            return Ok(None);
        }
        let Some(values) = specified_grid_expansion(&name, raw) else {
            return Ok(None);
        };
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(values.len())
            .map_err(|_| limit())?;
        push_text_entries(&mut entries, values, important);
        return Ok(Some(entries));
    }
    if name.as_ref() == "size"
        && !raw.as_bytes().windows(4).any(|bytes| bytes.eq_ignore_ascii_case(b"var("))
        && !["initial", "inherit", "unset", "revert", "revert-layer"].iter().any(|keyword| raw.eq_ignore_ascii_case(keyword))
    {
        let Some(parts) = components(raw).filter(|parts| (1..=2).contains(&parts.len())) else {
            return Ok(None);
        };
        let mut entries = Vec::new();
        for (longhand, token) in [("width", parts[0]), ("height", parts.get(1).copied().unwrap_or(parts[0]))] {
            let Some(part) = expansion(Arc::from(longhand), token, important)? else {
                return Ok(None);
            };
            for entry in part {
                push_entry(&mut entries, entry)?;
            }
        }
        return Ok(Some(entries));
    }
    let Some(parsed) = parsed_value(&name, raw)? else {
        return Ok(None);
    };
    let targets = longhands(&name);
    if targets.is_empty() {
        let value = match name.as_ref() {
            _ if !name.starts_with("--") && super::owner_values::may_use(raw) => super::owner_values::specified(&name, raw),
            "order" | "z-index" | "column-count" | "orphans" | "widows" | "column-span" | "fill-opacity" | "stroke-opacity" | "flood-opacity" | "stop-opacity" | "x" | "y" | "cx" | "cy" | "r" | "rx" | "ry" => super::serialize_cssom_property_value(&name,raw),
            "contain-intrinsic-width"|"contain-intrinsic-height"|"contain-intrinsic-inline-size"|"contain-intrinsic-block-size" => super::intrinsic_override::specified(raw),
            "background-image" | "mask-image" => super::image_source::serialize(raw),
            "background-size" | "mask-size" => super::background_sizes::specified(raw),
            "mask-repeat" => super::background_repeats(raw).map(|repeats| join_css_components(repeats.iter().map(|repeat| serialize_background_repeat_pair(*repeat)))),
            "border-radius" => super::corner_radii::specified_border_radius(raw),
            "text-indent" => super::text_indent::specified(raw),
            "white-space" => super::text_props::white_space_specified(raw),
            "mask-position" => super::background_positions::shorthand_lists(raw).and_then(|(x, y)| super::background_positions::join_axes(&x, &y)),
            "background-clip" => top_level_split(raw, b',', MAX_BACKGROUND_LAYERS).and_then(|layers| {
                let layers: Option<Vec<String>> = layers.iter().map(|layer| {
                    let tokens = components(layer)?;
                    Some(match tokens.as_slice() {
                        [a, b] if a.eq_ignore_ascii_case("text") && b.eq_ignore_ascii_case("border-area") => String::from("border-area text"),
                        _ => tokens.join(" ").to_ascii_lowercase(),
                    })
                }).collect();
                Some(layers?.join(", "))
            }),
            "filter" => super::serialize_color_filter_declaration(raw),
            "transform"=>super::typed_transforms::specified(raw),
            "translate"|"rotate"|"scale"=>super::individual_transforms::specified(match name.as_ref(){"translate"=>239,"rotate"=>240,_=>241},raw),
            "font-size-adjust" => super::serialize_declared_font_size_adjust(raw),
            "container-type" => parsed.iter().find_map(|declaration| match declaration.value {
                Value::ContainerType(value) => Some(value.as_str().into()), _ => None,
            }),
            "animation-name" | "animation-duration" | "animation-delay-start" | "animation-delay-end"
            | "animation-iteration-count" | "animation-direction" | "animation-fill-mode" | "animation-play-state"
            | "animation-composition" | "animation-timeline" | "animation-range-start" | "animation-range-end"
            | "animation-timing-function" | "transition-property"
 | "transition-duration" | "transition-timing-function" | "transition-delay" | "transition-behavior"
            | "font-style" | "font-width" | "font-feature-settings" | "font-variant-alternates" | "text-transform"
            | "scroll-behavior" | "text-overflow" | "content" | "box-decoration-break" | "flex-wrap"
            | "border-image-source" | "box-shadow" | "text-shadow"
            | "text-decoration-line" | "text-decoration-style" | "text-decoration-color" | "text-underline-position" | "text-decoration-thickness" | "text-underline-offset" => parsed.iter().find_map(|declaration|
                serialize_typed(&declaration.value, raw, &name, &name)),
            "font-family" => parsed.iter().find_map(|declaration| match &declaration.value {
                Value::FontFamily(value) => Some(serialize_font_families(value)),
                _ => None,
            }),
            "grid-template-rows"
            | "grid-template-columns"
            | "grid-auto-rows"
            | "grid-auto-columns" => computed_values::specified_grid_tracks(raw),
            "grid-template-areas" => parsed.iter().find_map(|declaration| {
                if let Value::GridAreas(areas, dimensions) = &declaration.value {
                    computed_values::serialize_grid_areas(areas, *dimensions)
                } else {
                    None
                }
            }),
            "grid-auto-flow" => parsed.iter().find_map(|declaration| {
                if let Value::GridAutoFlow(flow) = &declaration.value {
                    Some(computed_values::serialize_grid_flow(*flow))
                } else {
                    None
                }
            }),
            "grid-lanes-direction" => parsed.iter().find_map(|declaration| {
                if let Value::GridLanesDirection(_) = &declaration.value {
                    Some(
                        raw.split_ascii_whitespace()
                            .map(str::to_ascii_lowercase)
                            .collect::<Vec<_>>()
                            .join(" "),
                    )
                } else {
                    None
                }
            }),
            "flex-grow" | "flex-shrink" if !raw.contains('(') => parsed.iter().find_map(|declaration| match declaration.value {
                Value::FlexGrow(value) | Value::FlexShrink(value) => Some(value.to_string()),
                _ => None,
            }),
            _ if super::extra_props::property_index(&name).is_some() => parsed.iter().rev().find_map(|declaration| match &declaration.value {
                Value::ExtraRaw(slot, text) => super::extra_props::serialize_specified(*slot, text),
                _ => None,
            }),
            "color-interpolation-filters"=>parsed.iter().find_map(|declaration|
                if let Value::SvgFilterColorSpace(value)=declaration.value {Some(value.css_keyword().into())}else{None}),
            "aspect-ratio" if !raw.trim().to_ascii_lowercase().starts_with("var(")=>super::sizing_values::aspect_ratio_specified(raw),
            "outline-width" | "outline-offset" | "row-gap" | "column-gap" | "flow-tolerance" if raw.trim() == "0" => Some("0px".into()),
            _ if super::calc_size::unitless_zero(&name,raw).is_some()=>super::calc_size::unitless_zero(&name,raw),
            _ if super::calc_size::applies(&name,raw)=>super::calc_size::specified(&name,raw),
            _ if super::animation_color_property(&name)=>super::serialize_cssom_property_value(&name,raw),
            _ => parsed.iter().find_map(|declaration| match &declaration.value {
                Value::FontSizeKeyword(value) => Some(value.as_str().into()),
                Value::IntrinsicSize(_, value) | Value::FlexBasisIntrinsic(value) =>
                    Some(value.as_str().into()),
                _ => None,
            }).or_else(|| specified_alignment_expansion(&name, raw)
                .and_then(|mut values| (values.len() == 1).then(|| values.remove(0).1)))
                .or_else(|| (!name.starts_with("--") && super::math_function(raw))
                    .then(|| super::typed_numeric::parse_numeric_expression(raw)?.serialize_specified()).flatten()),
        }
        .unwrap_or_else(|| ["initial","inherit","unset","revert","revert-layer"].into_iter().find(|keyword|raw.eq_ignore_ascii_case(keyword)).unwrap_or(raw).to_owned());
        let mut entries = Vec::new();
        push_entry(
            &mut entries,
            Entry {
                name,
                value: Specified::Text(Arc::from(value)),
                important,
            },
        )?;
        return Ok(Some(entries));
    }
    let mut entries = Vec::new();
    entries
        .try_reserve_exact(targets.len())
        .map_err(|_| limit())?;
    if parsed
        .iter()
        .any(|declaration| matches!(declaration.value, Value::Deferred(..)))
    {
        if raw.len() > MAX_VARIABLE_BYTES {
            return Err(limit());
        }
        let pending = Arc::new(Pending {
            property: name,
            value: Arc::from(raw),
        });
        for &target in targets {
            entries.push(Entry {
                name: Arc::from(target),
                value: Specified::Pending(pending.clone()),
                important,
            });
        }
        return Ok(Some(entries));
    }
    let wide = ["initial", "inherit", "unset", "revert", "revert-layer"]
        .into_iter()
        .find(|keyword| raw.eq_ignore_ascii_case(keyword));
    if let Some(wide) = wide {
        let value: Arc<str> = Arc::from(wide);
        for &target in targets {
            entries.push(Entry {
                name: Arc::from(target),
                value: Specified::Text(value.clone()),
                important,
            });
        }
        return Ok(Some(entries));
    }
    if name.as_ref() == "offset" {
        let Some(values) = super::shapes::offset_longhands(raw) else {
            return Ok(None);
        };
        push_text_entries(&mut entries, values, important);
        return Ok(Some(entries));
    }
    if name.as_ref() == "position-try" {
        let Some(values) = anchor::position_try_longhands(raw) else {
            return Ok(None);
        };
        push_text_entries(&mut entries, values, important);
        return Ok(Some(entries));
    }
    if let Some(values) = super::grid_lines::expand(&name, raw) {
        push_text_entries(&mut entries, values, important);
        return Ok(Some(entries));
    }
    if let Some(values) = super::extra_props::expand_shorthand(&name, raw).or_else(|| super::corner_radii::expand(&name, raw)) {
        push_text_entries(&mut entries, values, important);
        return Ok(Some(entries));
    }
    if let Some(values) = super::text_props::expand(&name, raw) {
        push_text_entries(&mut entries, values, important);
        return Ok(Some(entries));
    }
    if let Some(values) = specified_alignment_expansion(&name, raw) {
        push_text_entries(&mut entries, values, important);
        return Ok(Some(entries));
    }
    if name.as_ref() == "background" {
        let Some(parts) = background_shorthand(raw) else {
            return Ok(None);
        };
        let Some(repeats) = parts.repeats else {
            return Ok(None);
        };
        let last_layer = top_level_split(raw, b',', MAX_BACKGROUND_LAYERS)
            .and_then(|layers| layers.last().copied())
            .unwrap_or(raw);
        let color = match parts.color {
            Some(value) => {let Some(value)=serialize_typed(&value,last_layer,"background","background-color") else {return Ok(None);};value},
            None => "rgba(0, 0, 0, 0)".into(),
        };
        let Some((position_x, position_y)) = super::background_positions::shorthand_lists(&parts.positions) else {
            return Ok(None);
        };
        let values = [
            color,
            match super::image_source::serialize(&parts.images){Some(value)=>value,None=>return Ok(None)},
            position_x,
            position_y,
            match super::background_sizes::specified(&parts.sizes) {
                Some(sizes) => sizes,
                None => return Ok(None),
            },
            join_css_components(repeats.into_iter().map(serialize_background_repeat_pair)),
            join_css_components(parts.attachments.into_iter().map(|value| match value {
                BackgroundAttachment::Scroll => "scroll",
                BackgroundAttachment::Fixed => "fixed",
                BackgroundAttachment::Local => "local",
            })),
            join_css_components(parts.origins.into_iter().map(serialize_background_box)),
            join_css_components(parts.clips.into_iter().map(serialize_background_box)),
        ];
        for (&target, value) in targets.iter().zip(values) {
            entries.push(Entry {
                name: Arc::from(target),
                value: Specified::Text(Arc::from(value)),
                important,
            });
        }
        return Ok(Some(entries));
    }
    if name.as_ref() == "mask" {
        let Some(parts) = super::mask_shorthand_parts(raw) else {
            return Ok(None);
        };
        let Some(images) = super::image_source::serialize(&parts.images) else {
            return Ok(None);
        };
        let values = [
            images,
            parts.positions,
            parts.sizes,
            join_css_components(parts.repeats.into_iter().map(serialize_background_repeat_pair)),
            join_css_components(parts.origins.into_iter().map(serialize_background_box)),
            join_css_components(parts.clips.into_iter().map(serialize_background_box)),
            join_css_components(parts.modes.into_iter().map(|mode| match mode {
                MaskMode::MatchSource => "match-source",
                MaskMode::Alpha => "alpha",
                MaskMode::Luminance => "luminance",
            })),
            join_css_components(parts.composites),
        ];
        for (&target, value) in targets.iter().zip(values) {
            entries.push(Entry {
                name: Arc::from(target),
                value: Specified::Text(Arc::from(value)),
                important,
            });
        }
        return Ok(Some(entries));
    }
    for &target in targets {
        // Border resets these properties even though it cannot specify them.
        // Retain their specified identities without manufacturing renderer slots.
        if name.as_ref() == "border" {
            if let Some(value) = border_image_initial(target) {
                entries.push(Entry {
                    name: Arc::from(target),
                    value: Specified::Text(Arc::from(value)),
                    important,
                });
                continue;
            }
        }
        let target_slots = slots(target);
        let Some(value) = parsed
            .iter()
            .rev()
            .find(|declaration| target_slots.contains(&declaration.value.slot()))
            .and_then(|declaration| serialize_typed(&declaration.value, raw, &name, target))
        else {
            // Keep the declaration unchanged when the supported renderer grammar
            // cannot represent a real longhand. Never invent a computed default.
            return Ok(None);
        };
        entries.push(Entry {
            name: Arc::from(target),
            value: Specified::Text(Arc::from(value)),
            important,
        });
    }
    Ok(Some(entries))
}

fn border_style(style: BorderStyle) -> &'static str {
    style.as_str()
}

fn css_color(value: Rgba, raw: &str) -> String {
    if let Some(token) = components(raw)
        .and_then(|tokens| tokens.into_iter().find(|token| color(token) == Some(value)))
    {
        return super::color_values::serialize_declared(token,0)
            .unwrap_or_else(||computed_values::color(value));
    }
    computed_values::color(value)
}

fn display_color(value: &str) -> Option<String> {
    if !color_values::structured(value) {
        return None;
    }
    color_values::serialize_declared_display(value)
}

fn length_percentage(value: LengthPercentage) -> String {
    computed_values::lp(value)
}

fn specified_border_width(value: f32, raw: &str, origin: &str, target: &str) -> String {
    let tokens = components(raw).unwrap_or_default();
    if matches!(origin, "border-inline-width" | "border-block-width") && !tokens.is_empty() {
        return tokens[usize::from(target.contains("-end-")) .min(tokens.len()-1)].to_ascii_lowercase();
    }
    if origin == "border-width" && !tokens.is_empty() {
        let side = if target.contains("-top-") {
            0
        } else if target.contains("-right-") {
            1
        } else if target.contains("-bottom-") {
            2
        } else {
            3
        };
        let token = match side {
            0 => tokens[0],
            1 => *tokens.get(1).unwrap_or(&tokens[0]),
            2 => *tokens.get(2).unwrap_or(&tokens[0]),
            _ => *tokens.get(3).unwrap_or(tokens.get(1).unwrap_or(&tokens[0])),
        };
        return token.to_owned();
    }
    for token in &tokens {
        if matches!(*token, "thin" | "medium" | "thick") {
            return (*token).to_owned();
        }
    }
    // Width is reset to medium when a border/column-rule shorthand omits it.
    if value == 3.0
        && !tokens
            .iter()
            .any(|token| nonnegative_length(token).is_some())
    {
        return "medium".into();
    }
    alloc::format!("{value}px")
}

/// Canonical CSSOM identity of legacy property aliases.
pub fn canonical_alias(name: &str) -> &str {
    if name_flags(name).is_some_and(|flags| flags & FLAG_ALIAS == 0) {
        return name;
    }
    canonical_alias_uncached(name)
}

fn canonical_alias_uncached(name: &str) -> &str {
    match name {
        "word-wrap" => "overflow-wrap",
        "grid-gap" => "gap",
        "grid-row-gap" => "row-gap",
        "grid-column-gap" => "column-gap",
        "font-stretch" => "font-width",
        "-webkit-appearance" => "appearance",
        "-webkit-mask" => "mask",
        "-webkit-mask-image" => "mask-image",
        "-webkit-mask-position" => "mask-position",
        "-webkit-mask-size" => "mask-size",
        "-webkit-mask-repeat" => "mask-repeat",
        "-webkit-mask-origin" => "mask-origin",
        "-webkit-mask-clip" => "mask-clip",
        "-webkit-align-content" => "align-content",
        "-webkit-align-items" => "align-items",
        "-webkit-align-self" => "align-self",
        "-webkit-flex" => "flex",
        "-webkit-flex-basis" => "flex-basis",
        "-webkit-flex-direction" => "flex-direction",
        "-webkit-flex-flow" => "flex-flow",
        "-webkit-flex-grow" => "flex-grow",
        "-webkit-flex-shrink" => "flex-shrink",
        "-webkit-flex-wrap" => "flex-wrap",
        "-webkit-justify-content" => "justify-content",
        "-webkit-order" => "order",
        _ => super::corner_radii::alias(name).unwrap_or(name),
    }
}

fn specified_flex_wrap(raw: &str, wraps: bool) -> &'static str {
    let has = |keyword: &str| raw.split_whitespace().any(|word| word.eq_ignore_ascii_case(keyword));
    match (has("wrap-reverse"), has("balance")) {
        (true, true) => "wrap-reverse balance",
        (true, false) => "wrap-reverse",
        (false, true) => "balance",
        (false, false) if wraps => "wrap",
        (false, false) => "nowrap",
    }
}

fn serialize_typed(value: &Value, raw: &str, origin: &str, target: &str) -> Option<String> {
    if origin=="columns" && matches!(target,"column-width"|"column-count") && !matches!(value,Value::Default(..)|Value::Revert(..)|Value::RevertLayer(..)) {return super::columns::specified_component(origin,target,raw);}
    Some(match value {
        Value::Deferred(property,source) if matches!(property.as_ref(),"transition-timing-function"|"animation-timing-function")=>
            super::serialize_cssom_property_value(property,source)?,
        Value::Background(value) => css_color(*value,raw),
        Value::BackgroundImageRaw(_) | Value::BackgroundImages(_) => super::image_source::serialize(raw)?,
        Value::ListStyleImage(value)=>match value {
            None=>"none".into(),
            Some(value) if super::background_images(value,Style::initial().color,None).is_some()=>super::image_source::serialize(value)?,
            Some(value)=>alloc::format!("url({})",super::serialize_css_string(value)),
        },
        Value::ShadowsRaw(_) | Value::TextShadowsRaw(_) | Value::Shadows(_) | Value::TextShadows(_,_) => super::serialize_declared_shadows(raw)?,
        Value::SourceColor(_,value)=>super::color_values::serialize(value.value,value.color_function)?,
        Value::BackgroundCurrentColor => "currentcolor".into(),
        Value::GeneratedContent(value) => serialize_generated_content(value),
        Value::TransitionProperty(_) | Value::TransitionTimingFunction(_) | Value::TransitionBehavior(_) | Value::TransitionTimes(_,_) | Value::TransitionTimesRaw(_,_) => serialize_transition_control_value(value)?,
        Value::Animation(_, value) => value.to_string(),
        Value::ContextLength(slot,raw,_) if matches!(*slot,222|223)=>animation_controls::endpoints(raw,*slot==222)?,
        Value::ContextLength(140, raw, _) => super::serialize_declared_font_size_adjust(raw)?,
        Value::TextDecoration(value) => serialize_text_decoration_line(*value),
        Value::TextDecorationStyle(value) => value.as_str().into(),
        Value::TextDecorationColor(None) => "currentcolor".into(),
        Value::TextDecorationColor(Some(value)) => css_color(*value, raw),
        Value::TextUnderlinePosition(value) => value.serialize(),
        Value::DecorationLength(_,value)=>value.serialize(),
        Value::FontSizeKeyword(value) => value.as_str().into(),
        Value::ContainerType(value) => value.as_str().into(),
        Value::ColumnSpan(value)=>value.serialize(),
        Value::ExtraRaw(slot,raw)=>super::extra_props::serialize_specified(*slot,raw)?,
        Value::BoxDecorationBreak(value)=>value.serialize(),
        Value::TransformOrigin(..)|Value::TransformOriginRaw(_)=>super::transform_origin::specified(raw)?,
        Value::SvgFilterColorSpace(value)=>value.css_keyword().into(),
        Value::LineBreakCount(_,value)=>value.to_string(),
        Value::BreakControl(_,value)=>super::break_control::serialize(target,*value)?.into(),
        Value::ColumnWidth(_)=>super::columns::specified("column-width",raw)?,
        Value::ContextLength(238,raw,_)=>super::columns::specified("column-width",raw)?,
        Value::ContainIntrinsic(_,_)=>super::intrinsic_override::specified_component(origin,target,raw)?,
        Value::ContextLength(slot,value,_) if (234..=237).contains(slot)=>super::intrinsic_override::specified(value)?,
        Value::ContextLength(slot, value, _) if matches!(slot,214|215) => serialize_decoration_length(value)?,
        Value::ContextLength(slot, value, _) if matches!(slot,48|74|75|130|244|245|255) => super::typed_numeric::parse_numeric_expression(value)?.serialize_specified()?,
        Value::ContextLength(_, value, _) => value.to_string(),
        Value::ColorRaw(_,value)=>super::color_values::serialize_declared(value,0)?,
        Value::GapRaw(_,value)=>value.to_string(),
        Value::MarginAuto(_)
        | Value::LogicalMargin(_, None)
        | Value::LogicalOffset(_, None)
        | Value::Offset(_, None) => "auto".into(),
        Value::MarginSide(_, value)
        | Value::PaddingSide(_, value)
        | Value::LogicalMargin(_, Some(value))
        | Value::LogicalOffset(_, Some(value))
        | Value::LogicalEdge(_, value)
        | Value::Offset(_, Some(value))
        | Value::ColumnGap(Some(value))
        | Value::FontSize(value) => alloc::format!("{value}px"),
        Value::Gap(_) => "normal".into(),
        Value::GapLength(_, value) => length_percentage(*value),
        Value::ColumnGap(None) => "normal".into(),
        Value::FontStyle(value) => specified_font_style(*value, raw),
        Value::FontStyleExpression(expression) => alloc::format!("oblique {}", expression.serialize()?),
        Value::FontWeight(value) => match value {
            400_000 => "normal".into(),
            700_000 => "bold".into(),
            -1 => "bolder".into(),
            -2 => "lighter".into(),
            value => {
                let mut weight = FontSpec::default();
                weight.set_weight_milli(*value as u32);
                weight.weight_text()
            }
        },
        Value::FontStretch(value) if origin == "font" => font_stretch_percentage_keyword(*value)?.into(),
        Value::FontStretch(value) => font_stretch_keyword(raw).map(String::from).unwrap_or_else(|| alloc::format!("{}%", computed_values::number(*value))),
        Value::FontStretchExpression(expression) => expression.serialize()?,
        Value::FontSizeAdjust(None) => "none".into(),
        Value::FontSizeAdjust(Some(_)) => super::serialize_declared_font_size_adjust(raw)?,
        Value::FontVariantCaps(value) => value.as_str().into(),
        Value::FontLigatures(value) => serialize_font_ligatures(*value),
        Value::FontAlternates(value) => font_feature_values::serialize_alternates(value.as_deref()),
        Value::UnicodeBidi(value) => value.as_str().into(),
        Value::BorderImageInitial(slot)=>border_images::serialize(&BorderImage::default(),*slot),
        Value::BorderImage(slot,value)=>border_images::serialize(value,*slot),
        Value::BorderImageRaw(slot,raw)=>if *slot==227{super::image_source::serialize(raw)?}else{border_images::serialize_specified(*slot,raw)?},
        Value::Appearance(value) => value.as_str().into(),
        Value::ScrollBehavior(value) => value.as_str().into(),
        Value::TextOverflow(value) => value.as_str().into(),
        Value::TextOverflowMarkers(value) => value.serialize(),
        Value::TextTransform(value) => value.as_str().into(),
        Value::FontFeatureSettings(value) => serialize_font_feature_settings(value.as_deref()),
        Value::FontFamily(value) => serialize_font_families(value),
        Value::ListStyleType(value) => computed_values::list_type(value),
        Value::ListStylePosition(value) => if *value == ListStylePosition::Inside {
            "inside"
        } else {
            "outside"
        }
        .into(),
        Value::LineHeight(value) => match value {
            LineHeight::Normal => "normal".into(),
            LineHeight::Number(value) => value.to_string(),
            LineHeight::Pixels(value) => alloc::format!("{value}px"),
        },
        Value::FlexDirection(value) => match value {
            FlexDirection::Row => "row",
            FlexDirection::RowReverse => "row-reverse",
            FlexDirection::Column => "column",
            FlexDirection::ColumnReverse => "column-reverse",
        }
        .into(),
        Value::FlexWrap(value) => if *value {
            specified_flex_wrap(raw, true)
        } else {
            "nowrap"
        }
        .into(),
        Value::FlexWrapReverse(value) => if *value {
            "wrap-reverse"
        } else if raw.split_whitespace().any(|word| word == "wrap") {
            "wrap"
        } else {
            "nowrap"
        }
        .into(),
        Value::FlexWrapReverseBalance(_) => specified_flex_wrap(raw, true).into(),
        Value::FlexGrow(value) | Value::FlexShrink(value) => value.to_string(),
        Value::FlexBasis(Some(value)) => alloc::format!("{value}px"),
        Value::FlexBasis(None) => "auto".into(),
        Value::FlexBasisContent => "content".into(),
        Value::FlexBasisIntrinsic(value) | Value::IntrinsicSize(_, value) => value.as_str().into(),
        Value::LogicalBorder(_, component) => match component {
            LogicalBorderComponent::Width(value) => {
                specified_border_width(*value, raw, origin, target)
            }
            LogicalBorderComponent::Color(value) => css_color(*value, raw),
            LogicalBorderComponent::CurrentColor => "currentcolor".into(),
            LogicalBorderComponent::Style(value) => border_style(*value).into(),
        },
        Value::BorderStyle(value) => border_style(*value).into(),
        Value::OutlineWidth(value) => specified_border_width(*value, raw, origin, target),
        Value::OutlineStyle(value) => value.as_str().into(),
        Value::OutlineColor(value) => {
            value.map_or_else(|| "currentcolor".into(), |value| css_color(value, raw))
        }
        Value::OutlineOffset(value) => alloc::format!("{value}px"),
        Value::TextOrientation(value) => value.as_str().into(),
        Value::ColumnRuleWidth(value) => specified_border_width(*value, raw, origin, target),
        Value::ColumnRuleColor(value) => {
            value.map_or_else(|| "currentcolor".into(), |value| css_color(value, raw))
        }
        Value::ColumnRuleStyle(value) => value.as_str().into(),
        Value::WordBreak(value) => value.as_str().into(),
        Value::OverflowWrap(value) => value.as_str().into(),
        Value::OverflowAxis(_, value) => match value {
            Overflow::Visible => "visible",
            Overflow::Hidden => "hidden",
            Overflow::Scroll => "scroll",
            Overflow::Auto => "auto",
            Overflow::Clip => "clip",
        }
        .into(),
        _ => return None,
    })
}

impl DeclarationBlock {
    pub fn parse(input: &str) -> Result<Self, CssError> {
        Self::parse_with_keyframe_policy(input, false)
    }

    pub(super) fn parse_keyframes(input: &str) -> Result<Self, CssError> {
        Self::parse_with_keyframe_policy(input, true)
    }

    fn parse_with_keyframe_policy(input: &str, keyframes: bool) -> Result<Self, CssError> {
        let mut result = Self::default();
        let spans=declaration_spans_with_limit(input,true,MAX_ENTRIES+PROPERTIES.len())?;
        let compact_wide=spans.len()>COMPACT_WIDE_SPANS;
        let mut ordinary_sources=0usize;
        for (start,end) in spans {
            let Some((name, raw)) = declaration_pair(&input[start..end]) else {
                continue;
            };
            let Some(decoded_name)=parsed_declaration_name(name) else {continue;};
            let Some(name) = normalized_name(decoded_name.as_ref()) else {
                continue;
            };
            let Some(completed) = syntax::complete(raw)? else { continue; };
            let raw = completed.as_ref();
            let (raw, important) = important_value(raw);
            if keyframes && (important || name.as_ref() == "animation"
                || name.starts_with("animation-") && !matches!(name.as_ref(),
                    "animation-timing-function" | "animation-composition")) {
                continue;
            }
            let Some(entries) = expansion(name, raw, important)? else {
                continue;
            };
            if compact_wide && !entries.iter().all(|entry|all_target(&entry.name)
                && matches!(&entry.value,Specified::Text(value) if ["initial","inherit","unset","revert","revert-layer"].contains(&value.as_ref()))) {
                ordinary_sources+=1;
                if ordinary_sources>MAX_ENTRIES{return Err(limit());}
            }
            // This block is unpublished until parsing succeeds, so it needs
            // no rollback copy after each source declaration. CSSOM setters
            // still prepare a separate sequence before committing.
            Self::merge_entries(&mut result.entries, entries, true, compact_wide)?;
        }
        Ok(result)
    }

    pub(super) fn into_keyframe_pairs(self) -> Result<Vec<(String, String)>, CssError> {
        let mut output=Vec::new();
        output.try_reserve_exact(self.entries.len()).map_err(|_|limit())?;
        let mut pending=BTreeSet::new();
        for entry in &self.entries {
            if let Specified::All(group)=&entry.value {
                if let Specified::Pending(pending_group)=&group.value {
                    if pending.insert(Arc::as_ptr(pending_group) as usize){output.push((pending_group.property.to_string(),pending_group.value.to_string()));}
                }else{for view in entry_views(entry){if let Specified::Text(value)=view.value{output.try_reserve(1).map_err(|_|limit())?;output.push((view.name.to_string(),value.to_string()));}}}
                continue;
            }
            match &entry.value {
                Specified::Text(value)=>output.push((entry.name.to_string(),value.to_string())),
                Specified::Pending(group)=>{
                    // An unresolved shorthand remains one authored value;
                    // its shared cohort must not become empty longhands.
                    if pending.insert(Arc::as_ptr(group) as usize) {
                        output.push((group.property.to_string(),group.value.to_string()));
                    }
                }
                Specified::All(_)=>unreachable!(),
            }
        }
        Ok(output)
    }

    fn merge(&mut self, incoming: Vec<Entry>, source: bool) -> Result<bool, CssError> {
        // Prepare the final bounded sequence before committing. Arc values make
        // this rollback buffer share retained strings and pending cohorts.
        let mut next = Vec::new();
        next.try_reserve_exact(self.entries.len())
            .map_err(|_| limit())?;
        next.extend(self.entries.iter().cloned());
        Self::merge_entries(&mut next, incoming, source, false)?;
        let changed = self.entries != next;
        if changed {
            self.entries = next;
        }
        Ok(changed)
    }

    /// Apply the same source-order and CSSOM replacement rules to either a
    /// fresh parse result or a transactional setter buffer.
    fn merge_entries(entries:&mut Vec<Entry>,incoming:Vec<Entry>,source:bool,compact_wide:bool)->Result<(),CssError>{
        for mut entry in incoming {
            // Serialized partial resets contain many wide-valued longhands.
            // Compact those source declarations too, without increasing the
            // ordinary entry allowance or allocating one retained Arc per name.
            if source && compact_wide && all_target(&entry.name) && matches!(&entry.value,Specified::Text(value) if ["initial","inherit","unset","revert","revert-layer"].contains(&value.as_ref())) {
                let index=registry_index(entry.name.as_ref()).ok_or_else(limit)?;
                let mut group=AllGroup::full(entry.value)?;
                for target in 0..PROPERTIES.len(){if target!=index{group.clear(target);}}
                group.reset_metadata=false;
                entry.name=Arc::from("all");entry.value=Specified::All(Arc::new(group));
            }

            if let Specified::All(incoming)=&entry.value {
                let mut group=(**incoming).clone();
                for old in entries.iter_mut(){
                    if let Specified::All(previous)=&mut old.value{
                        let previous=Arc::make_mut(previous);
                        for index in 0..PROPERTIES.len(){if previous.contains(index)&&group.contains(index){
                            if source {if old.important&&!entry.important{group.clear(index);}else{previous.clear(index);}}
                            else{group.clear(index);}
                        }}
                        if !source{previous.value=group.value.clone();old.important=entry.important;}
                    }else if let Some(index)=registry_index(old.name.as_ref()){
                        if !group.contains(index){continue;}
                        if source{if old.important&&!entry.important{group.clear(index);}else{old.name=Arc::from("");}}
                        else{old.value=group.value.clone();old.important=entry.important;group.clear(index);}
                    }
                }
                entries.retain(|old|!old.name.is_empty()&&!matches!(&old.value,Specified::All(group) if group.empty()));
                if !group.empty(){entry.value=Specified::All(Arc::new(group));push_entry(entries,entry)?;}
                compact_all_runs(entries);
                continue;
            }
            let position=entries.iter().position(|old|entry_views(old).any(|view|view.name==entry.name.as_ref()));
            if let Some(position)=position{
                if source&&entries[position].important&&!entry.important{continue;}
                if let Specified::All(group)=&entries[position].value{
                    let index=registry_index(entry.name.as_ref()).ok_or_else(limit)?;
                    let mut before=(**group).clone();let mut after=before.clone();
                    if source{before.clear(index);if before.empty(){entries.remove(position);}else{entries[position].value=Specified::All(Arc::new(before));}}
                    else{
                        for target in 0..PROPERTIES.len(){if target>=index{before.clear(target);}if target<=index{after.clear(target);}}
                        let important=entries[position].important;entries.remove(position);entries.try_reserve(3).map_err(|_|limit())?;
                        let mut insertion=position;
                        if !before.empty(){entries.insert(insertion,Entry{name:Arc::from("all"),value:Specified::All(Arc::new(before)),important});insertion+=1;}
                        let updated=entry.name.clone();entries.insert(insertion,entry);insertion+=1;
                        if !after.empty(){entries.insert(insertion,Entry{name:Arc::from("all"),value:Specified::All(Arc::new(after)),important});}
                        order_updated_declaration(entries,&updated)?;
                        continue;
                    }
                }else if !source{let updated=entry.name.clone();entries[position]=entry;order_updated_declaration(entries,&updated)?;continue;}else{entries.remove(position);}
            }
            push_entry(entries,entry)?;
        }
        compact_all_runs(entries);
        if entries.len()>MAX_ENTRIES*2+1||entries.iter().filter(|entry|!matches!(entry.value,Specified::All(_))).count()>MAX_ENTRIES{return Err(limit());}
        let mut bytes=0usize;let mut identities=BTreeSet::new();
        for entry in entries.iter(){
            bytes=bytes.checked_add(entry.name.len()).ok_or_else(limit)?;
            let value=if let Specified::All(group)=&entry.value{
                bytes=bytes.checked_add(core::mem::size_of::<AllGroup>()+2*core::mem::size_of::<usize>()).and_then(|bytes|bytes.checked_add(group.targets.capacity().checked_mul(core::mem::size_of::<u64>())?)).ok_or_else(limit)?;&group.value
            }else{&entry.value};
            let(identity,size)=match value{
                Specified::Text(text)=>(Arc::as_ptr(text) as *const () as usize,text.len()),
                Specified::Pending(group)=>(Arc::as_ptr(group) as usize,group.property.len().checked_add(group.value.len()).ok_or_else(limit)?),
                Specified::All(_)=>unreachable!(),
            };
            if identities.insert(identity){bytes=bytes.checked_add(size).ok_or_else(limit)?;}
        }
        if bytes>MAX_CSS_BYTES{return Err(limit());}Ok(())
    }
    fn views(&self)->impl Iterator<Item=EntryView<'_>>{self.entries.iter().flat_map(entry_views)}

    pub(crate) fn render_declarations(&self) -> Result<Vec<Declaration>, CssError> {
        let mut result = Vec::new();
        if self.entries.iter().any(|entry|matches!(entry.value,Specified::All(_))) {
        // Retain aggregate compatibility slots without invented authored names.
        // Deferred projections reuse the canonical pending-cohort cache.
        let mut represented=[false;PROPERTY_COUNT];
        for property in PROPERTIES.iter().filter(|property|property.in_all && longhands(property.name).is_empty() && canonical_alias(property.name)==property.name){
            for &slot in property.ids{represented[slot]=true;}
        }
        let mut projected=BTreeSet::new();
        for entry in &self.entries {
            let Specified::All(group)=&entry.value else{continue;};
            if !group.reset_metadata{continue;}
            let parsed=if let Specified::Text(raw)=&group.value{parsed_value("all",raw)?.unwrap_or_default()}else{Vec::new()};
            for &slot in &ALL_PROPERTY_IDS {
                if represented[slot]||!compatibility_slot_covered(group,slot){continue;}
                let identity=match &group.value{Specified::Text(raw)=>Arc::as_ptr(raw) as *const () as usize,Specified::Pending(group)=>Arc::as_ptr(group) as usize,_=>unreachable!()};
                if !projected.insert((identity,slot,entry.important)){continue;}
                let value=match &group.value{
                    Specified::Text(_)=>parsed.iter().find(|declaration|declaration.value.slot()==slot).map(|declaration|declaration.value.clone()),
                    Specified::Pending(group)=>Some(Value::DeferredSlot(group.clone(),slot)),
                    Specified::All(_)=>unreachable!(),
                };
                if let Some(value)=value{result.try_reserve(1).map_err(|_|limit())?;result.push(Declaration{value,important:entry.important});}
            }
        }
        }
        for entry in self.views() {
            match entry.value {
                Specified::Pending(group) => {
                    let target = PROPERTIES.iter().map(|property|property.name).find(|target|*target==entry.name)
                        .ok_or_else(limit)?;
                    result.try_reserve(1).map_err(|_| limit())?;
                    result.push(Declaration {
                        value: Value::DeferredLonghand(group.clone(), target),
                        important: entry.important,
                    });
                }
                Specified::Text(raw) => {
                    let Some(mut parsed) = parsed_value(entry.name, raw)? else {
                        continue;
                    };
                    result.try_reserve(parsed.len()).map_err(|_| limit())?;
                    for declaration in &mut parsed {
                        declaration.important = entry.important;
                    }
                    result.extend(parsed);
                }
                Specified::All(_) => unreachable!(),
            }
        }
        Ok(result)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.views().map(|entry| entry.name)
    }
    /// Conservative retained source and virtual-target storage charge.
    pub fn retained_text_bytes(&self)->usize{
        self.entries.iter().fold(0usize,|bytes,entry|{
            let(mut bytes,value)=(bytes.saturating_add(entry.name.len()),&entry.value);
            let value=if let Specified::All(group)=value{bytes=bytes.saturating_add(core::mem::size_of::<AllGroup>()+2*core::mem::size_of::<usize>()).saturating_add(group.targets.capacity().saturating_mul(core::mem::size_of::<u64>()));&group.value}else{value};
            bytes.saturating_add(match value{Specified::Text(text)=>text.len(),Specified::Pending(group)=>group.property.len().saturating_add(group.value.len()),Specified::All(_)=>unreachable!()})
        })
    }
    pub fn len(&self)->usize{self.views().count()}
    pub fn retain_properties(&mut self,keep:impl Fn(&str)->bool){self.entries.retain(|entry|keep(&entry.name));}
    pub fn retain_declarations(&mut self,keep:impl Fn(&str,bool)->bool){self.entries.retain(|entry|keep(&entry.name,entry.important));}
    pub fn is_empty(&self)->bool{self.entries.is_empty()}

    pub fn value(&self, name: &str) -> Option<(String, bool)> {
        let name = normalized_name(name)?;
        if name.as_ref()=="all"{
            let mut first:Option<EntryView<'_>>=None;
            for property in PROPERTIES.iter().filter(|property|property.in_all && longhands(property.name).is_empty() && canonical_alias(property.name)==property.name){
                let entry=self.views().find(|entry|entry.name==property.name)?;
                if let Some(previous)=first{if entry.important!=previous.important||!same_specified(entry.value,previous.value){return None;}}else{first=Some(entry);}
            }
            let first=first?;return match first.value{
                Specified::Text(value) if ["initial","inherit","unset","revert","revert-layer"].contains(&value.as_ref())=>Some((value.to_string(),first.important)),
                Specified::Pending(group) if group.property.as_ref()=="all"=>Some((group.value.to_string(),first.important)),_=>None,
            };
        }
        let targets=longhands(&name);
        if targets.is_empty(){

            return self
                .views()
                .find(|entry| entry.name == name.as_ref())
                .map(|entry| {
                    (
                        match &entry.value {
                            Specified::Text(text) => text.to_string(),
                            Specified::Pending(_) => String::new(),
                            Specified::All(_) => unreachable!(),
                        },
                        entry.important,
                    )
                });
        }
        let first=self.views().find(|entry|entry.name==targets[0])?;
        let mut entries = [first; 24];
        for (index, target) in targets.iter().enumerate() {
            entries[index]=self.views().find(|entry|entry.name==*target)?;
        }
        let entries = &entries[..targets.len()];
        let important = entries.first()?.important;
        if entries.iter().any(|entry| entry.important != important) {
            return None;
        }
        if let Specified::Pending(group) = &entries[0].value {
            if !entries.iter().all(|entry| matches!(&entry.value, Specified::Pending(other) if Arc::ptr_eq(group, other))) {
                return None;
            }
            return Some(if group.property == name { (group.value.to_string(), important) } else { (String::new(), important) });
        }
        let mut values = [""; 24];
        for (index, entry) in entries.iter().enumerate() {
            values[index] = match &entry.value {
                Specified::Text(text) => text.as_ref(),
                Specified::Pending(_) => return None,
                Specified::All(_) => unreachable!(),
            };
        }
        let values = &values[..targets.len()];
        if values.iter().any(|value| {
            value
                .as_bytes()
                .windows(4)
                .any(|bytes| bytes.eq_ignore_ascii_case(b"var("))
                && component_values::parse_unparsed_value(value).is_ok_and(|parts| {
                    parts.iter().any(|part| {
                        matches!(part, component_values::UnparsedComponent::Variable { .. })
                    })
                })
        }) {
            return None;
        }
        let wide = |value: &str| {
            ["inherit", "initial", "unset", "revert", "revert-layer"].contains(&value)
        };
        if values.iter().any(|value| wide(value)) {
            return values
                .iter()
                .all(|value| *value == values[0])
                .then(|| (values[0].to_owned(), important));
        }
        synthesize(&name, values).map(|value| (value, important))
    }

    pub fn set(&mut self, name: &str, value: &str, important: bool) -> Result<bool, CssError> {
        if value.is_empty() {
            let before = self.len();
            self.remove(name)?;
            return Ok(before != self.len());
        }
        let Some(name) = normalized_name(name) else {
            return Ok(false);
        };
        let Some(entries) = expansion(name, value, important)? else {
            return Ok(false);
        };
        self.merge(entries, false)
    }

    pub fn remove(&mut self, name: &str) -> Result<String, CssError> {
        let old = self
            .value(name)
            .map_or_else(String::new, |(value, _)| value);
        let Some(name) = normalized_name(name) else {
            return Ok(old);
        };
        let targets=longhands(&name);
        let affects=|target:&str|name.as_ref()=="all"&&all_target(target)||target==name.as_ref()||targets.contains(&target);
        for entry in &mut self.entries{if let Specified::All(group)=&mut entry.value{let group=Arc::make_mut(group);
            for(index,property)in PROPERTIES.iter().enumerate(){if group.contains(index)&&affects(property.name){group.clear(index);}}}}
        self.entries.retain(|entry|match &entry.value{Specified::All(group)=>!group.empty(),_=>!affects(&entry.name)});
        if self.entries.is_empty() {
            self.entries = Vec::new();
        }
        Ok(old)
    }

    /// CSSOM readback of a property: stored structured colors keep exact nested
    /// channels, and script observes their canonical 8-bit legacy serialization.
    pub fn display_value(&self, name: &str) -> Option<(String, bool)> {
        let (value, important) = self.value(name)?;
        let value = match normalized_name(name) {
            Some(name) if animation_color_property(&name) => display_color(&value).unwrap_or(value),
            _ => value,
        };
        Some((value, important))
    }

    pub fn serialize_display(&self) -> Result<String, CssError> {
        let mut block = self.clone();
        for entry in &mut block.entries {
            if let Specified::Text(text) = &mut entry.value {
                if animation_color_property(&entry.name) {
                    if let Some(display) = display_color(text) {
                        *text = Arc::from(display);
                    }
                }
            }
        }
        block.serialize()
    }

    pub fn serialize(&self) -> Result<String, CssError> {
        let mut output = String::new();
        let Some(first)=self.views().next() else{return Ok(output);};
        let mut local_entries=[first;LOCAL_ENTRIES];let mut local_used=[false;LOCAL_ENTRIES];
        let mut expanded_entries=Vec::new();let mut expanded_used=Vec::new();
        let length=self.len();
        let(entries,used):(&[EntryView<'_>],&mut[bool])=if length<=LOCAL_ENTRIES{
            for(index,entry)in self.views().enumerate(){local_entries[index]=entry;}
            (&local_entries[..length],&mut local_used[..length])
        }else{
            expanded_entries.try_reserve_exact(length).map_err(|_|limit())?;expanded_entries.extend(self.views());
            expanded_used.try_reserve_exact(length).map_err(|_|limit())?;expanded_used.resize(length,false);
            (&expanded_entries,&mut expanded_used)
        };

        let all_value=self.value("all");
        let mut all_targets=Vec::new();
        if all_value.is_some(){all_targets.try_reserve_exact(PROPERTIES.len()).map_err(|_|limit())?;all_targets.extend(PROPERTIES.iter().filter(|property|property.in_all && longhands(property.name).is_empty() && canonical_alias(property.name)==property.name).map(|property|property.name));}
        for (position,entry) in entries.iter().enumerate(){
            if used[position] {
                continue;
            }
            let mut serialized = false;
            for &shorthand in core::iter::once(&"all").chain(SHORTHANDS.iter()) {
                let targets=if shorthand=="all"{all_targets.as_slice()}else{longhands(shorthand)};
                if !targets.contains(&entry.name.as_ref()) {
                    continue;
                }
                let Some((value, important)) = (if shorthand=="all" {all_value.clone()}else{self.value(shorthand)}) else {
                    continue;
                };
                let mut local_indices=[0;24];let mut expanded_indices=Vec::new();
                let indices:&mut[usize]=if targets.len()<=local_indices.len(){&mut local_indices[..targets.len()]}else{
                    expanded_indices.try_reserve_exact(targets.len()).map_err(|_|limit())?;expanded_indices.resize(targets.len(),0);&mut expanded_indices
                };

                let mut complete = true;
                for (index, target) in targets.iter().enumerate() {
                    if let Some(position)=entries.iter().position(|entry|entry.name==*target)
                    {
                        indices[index] = position;
                    } else {
                        complete = false;
                        break;
                    }
                }
                let indices = &indices[..targets.len()];
                if !complete || indices.iter().any(|&index| used[index]) {
                    continue;
                }
                // Interleaved independent declarations can be crossed. Logical
                // and physical properties that update the same edge cannot.
                let first = *indices.iter().min().unwrap_or(&position);
                let last = *indices.iter().max().unwrap_or(&position);
                if first != position {
                    continue;
                }
                if (first..=last).any(|index| {
                    !indices.contains(&index)
                        && targets.iter().any(|target| {
                            declaration_order_conflict(target, entries[index].name)
                        })
                }) {
                    continue;
                }
                append_serialized(&mut output, shorthand, &value, important)?;
                for &index in indices {
                    used[index] = true;
                }
                serialized = true;
                break;
            }
            if !serialized {
                let value = match &entry.value {
                    Specified::Text(text) => text.as_ref(),
                    Specified::Pending(_) => "",
                    Specified::All(_) => unreachable!(),
                };
                append_serialized(&mut output, entry.name, value, entry.important)?;
                used[position] = true;
            }
        }
        Ok(output)
    }
}

const SHORTHANDS: &[&str] = &[
    "animation","animation-range","animation-delay","scroll-timeline","view-timeline",
    "transition",
    "text-decoration",
    "outline",
    "border",
    "font",
    "background",
    "background-position",
    "mask",
    "grid",
    "grid-template",
    "grid-lanes",
    "grid-area",
    "grid-row",
    "grid-column",
    "border-inline-width",
    "border-inline-style",
    "border-inline-color",
    "border-block-width",
    "border-block-style",
    "border-block-color",
    "border-inline",
    "border-block",
    "border-image",
    "border-color",
    "border-style",
    "border-width",
    "border-top",
    "border-right",
    "border-bottom",
    "border-left",
    "border-inline-start",
    "border-inline-end",
    "border-block-start",
    "border-block-end",
    "margin",
    "padding",
    "inset",
    "margin-inline",
    "margin-block",
    "padding-inline",
    "padding-block",
    "inset-inline",
    "inset-block",
    "place-items",
    "place-self",
    "place-content",
    "gap",
    "position-try",
    "offset",
    "flex",
    "flex-flow",
    "columns",
    "column-rule",
    "row-rule",
    "rule",
    "rule-width",
    "rule-color",
    "rule-style",
    "rule-break",
    "rule-visibility-items",
    "column-rule-inset",
    "row-rule-inset",
    "rule-inset",
    "column-rule-inset-cap",
    "row-rule-inset-cap",
    "rule-inset-cap",
    "column-rule-inset-junction",
    "row-rule-inset-junction",
    "rule-inset-junction",
    "column-rule-inset-start",
    "row-rule-inset-start",
    "rule-inset-start",
    "column-rule-inset-end",
    "row-rule-inset-end",
    "rule-inset-end",
    "border-top-radius",
    "border-right-radius",
    "border-bottom-radius",
    "border-left-radius",
    "border-block-start-radius",
    "border-block-end-radius",
    "border-inline-start-radius",
    "border-inline-end-radius",
    "corner-shape",
    "corner-top-shape",
    "corner-right-shape",
    "corner-bottom-shape",
    "corner-left-shape",
    "corner-inline-start-shape",
    "corner-inline-end-shape",
    "corner-block-start-shape",
    "corner-block-end-shape",
    "list-style",
    "overflow",
];

/// Shared four-side CSS compression for specified and computed reflection.
pub fn serialize_box_sides(values: [&str; 4]) -> String {
    let end = if values[0] == values[1] && values[0] == values[2] && values[0] == values[3] {
        1
    } else if values[0] == values[2] && values[1] == values[3] {
        2
    } else if values[1] == values[3] {
        3
    } else {
        4
    };
    values[..end].join(" ")
}

fn synthesize(name: &str, values: &[&str]) -> Option<String> {
    if name == "offset" {
        let &[position, path, distance, rotate, anchor] = values else { return None };
        return super::shapes::offset_shorthand_text([position, path, distance, rotate, anchor]);
    }
    if name == "position-try" {
        let &[order, fallbacks] = values else { return None };
        return Some(anchor::serialize_position_try(order, fallbacks));
    }
    if super::extra_props::is_shorthand(name) {
        return super::extra_props::synthesize_shorthand(name, values);
    }
    if super::corner_radii::is_shorthand(name) {
        return super::corner_radii::synthesize(name, values);
    }
    if super::grid_lines::is_shorthand(name) {
        return super::grid_lines::synthesize(name, values);
    }
    if super::text_props::is_shorthand(name) {
        return super::text_props::shorthand_value(name, values);
    }
    if let Some(slot)=super::break_control::slot(name).filter(|_|name.starts_with("page-")) {
        let [value]=values else{return None;};
        let target=match slot{247=>"break-before",248=>"break-after",_=>"break-inside"};
        if ["initial","inherit","unset","revert","revert-layer"].contains(value){return Some((*value).into());}
        return Some(super::break_control::serialize(name,super::break_control::parse(target,value)?)?.into());
    }
    if name=="columns" {return match values{[width,count,height,"auto"]=>Some(super::columns::shorthand_with_height(width,count,height)),_=>None};}
    if matches!(name,"contain-intrinsic-size"|"size") {return match values{[a,b]=>Some(if a==b{(*a).into()}else{alloc::format!("{a} {b}")}),_=>None};}
    if name=="animation" {return animation_controls::shorthand(values);}
    if name=="animation-range" {return animation_controls::range_shorthand(values);}
    if name=="animation-delay" {return animation_controls::delay_shorthand(values);}
    if matches!(name,"scroll-timeline"|"view-timeline"){return animation_controls::timeline_shorthand(values);}
    if name=="transition" {return transition_controls::shorthand(values);}
    if name == "text-decoration" {
        let &[line, style, color, thickness] = values else { return None; };
        let mut result = String::new();
        for (value, initial) in [(line,"none"),(thickness,"auto"),(style,"solid"),(color,"currentcolor")] {
            if value != initial { if !result.is_empty() { result.push(' '); } result.push_str(value); }
        }
        if result.is_empty() { result.push_str("none"); }
        return Some(result);
    }
    if name == "grid-template" {
        let &[rows, columns, areas] = values else {
            return None;
        };
        return computed_values::serialize_grid_template(rows, columns, areas);
    }
    if name == "grid" {
        return computed_values::serialize_grid_shorthand(values);
    }
    if name == "grid-lanes" {
        return computed_values::serialize_grid_lanes(values);
    }
    if name == "background" {
        return serialize_background(values);
    }
    if name == "mask" {
        return super::mask_shorthand::synthesize(values);
    }
    if name == "list-style" {
        let mut result = [""; 3];
        let mut count = 0;
        for (value, initial) in values.iter().zip(["outside", "none", "disc"]) {
            if *value != initial {
                result[count] = *value;
                count += 1;
            }
        }
        return Some(if count == 0 {
            "disc".into()
        } else {
            result[..count].join(" ")
        });
    }
    if matches!(
        name,
        "margin" | "padding" | "inset" | "border-width" | "border-style" | "border-color"
    ) {
        return Some(serialize_box_sides([
            values[0], values[1], values[2], values[3],
        ]));
    }
    if name == "outline" {
        return Some(serialize_outline_components(
            values[2], values[1], values[0],
        ));
    }
    if name == "border" {
        if !values[..4].iter().all(|value| *value == values[0])
            || !values[4..8].iter().all(|value| *value == values[4])
            || !values[8..12].iter().all(|value| *value == values[8])
        {
            return None;
        }
        if longhands("border")[12..]
            .iter()
            .zip(&values[12..])
            .any(|(name, value)| border_image_initial(name) != Some(*value))
        {
            return None;
        }
        return Some(serialize_border_components(values[0], values[4], values[8]));
    }
    if name == "border-image" {return Some(border_images::serialize_shorthand(values[0],values[1],values[2],values[3],values[4]));}
    if matches!(name, "border-inline" | "border-block") {
        if values[0] != values[1] || values[2] != values[3] || values[4] != values[5] {
            return None;
        }
        return Some(serialize_border_components(values[0], values[2], values[4]));
    }
    if matches!(
        name,
        "border-top"
            | "border-right"
            | "border-bottom"
            | "border-left"
            | "border-inline-start"
            | "border-inline-end"
            | "border-block-start"
            | "border-block-end"
    ) {
        return Some(serialize_border_components(values[0], values[1], values[2]));
    }
    if name == "font-variant" {
        return super::font_variant::serialize(FontVariantCaps::parse(values[1])?, font_ligatures(values[0])?, font_feature_values::parse_alternates(values[2])?.as_deref(), [values[3], values[4], values[5], values[6]]);
    }
    if name == "font" {
        if values[10] != "normal" || values[9] != "normal" || values[8] != "normal" || values[11..15].iter().any(|value| *value != "normal") || values[15] != "auto" || values[16] != "auto" || values[17] != "normal" || values[18] != "normal" || values[7] != "none" || values[1] != "normal" && values[1] != "small-caps" {
            return None;
        }
        let mut parts = Vec::new();
        for &value in &values[..3] {
            if value != "normal" {
                parts.push(value);
            }
        }
        let stretch = font_stretch_keyword(values[3]).or_else(|| font_stretch(values[3]).and_then(font_stretch_percentage_keyword)).or_else(|| font_width_expression(values[3]).and_then(|expression| resolve_font_width(&expression,None,ContainerUnitContext::default())).and_then(font_stretch_percentage_keyword))?;
        if stretch != "normal" {
            parts.push(stretch);
        }
        let mut result = parts.join(" ");
        if !result.is_empty() {
            result.push(' ');
        }
        result.push_str(values[4]);
        if values[5] != "normal" {
            result.push_str(" / ");
            result.push_str(values[5]);
        }
        result.push(' ');
        result.push_str(values[6]);
        return Some(result);
    }
    if name == "flex-flow" && values.len() == 2 {
        let direction = (values[0] != "row").then_some(values[0]);
        let wrap = (values[1] != "nowrap").then_some(values[1]);
        return Some(match (direction, wrap) {
            (None, None) => "row".to_owned(),
            (Some(value), None) | (None, Some(value)) => value.to_owned(),
            (Some(direction), Some(wrap)) => alloc::format!("{direction} {wrap}"),
        });
    }
    if values.len() == 2 && !matches!(name, "flex-flow") && values[0] == values[1] {
        return Some(values[0].to_owned());
    }
    Some(values.join(" "))
}

fn join_css_components<T: AsRef<str>>(values: impl IntoIterator<Item = T>) -> String {
    let mut output = String::new();
    for value in values {
        if !output.is_empty() {
            output.push_str(", ");
        }
        output.push_str(value.as_ref());
    }
    output
}

pub(super) fn serialize_background_repeat_pair(values: [BackgroundRepeat; 2]) -> String {
    let keyword = |value| match value {
        BackgroundRepeat::Repeat => "repeat",
        BackgroundRepeat::Space => "space",
        BackgroundRepeat::Round => "round",
        BackgroundRepeat::NoRepeat => "no-repeat",
    };
    if values[0] == values[1] {
        keyword(values[0]).into()
    } else if values == [BackgroundRepeat::Repeat, BackgroundRepeat::NoRepeat] {
        "repeat-x".into()
    } else if values == [BackgroundRepeat::NoRepeat, BackgroundRepeat::Repeat] {
        "repeat-y".into()
    } else {
        alloc::format!("{} {}", keyword(values[0]), keyword(values[1]))
    }
}

pub(super) fn serialize_background_box(value: BackgroundBox) -> &'static str {
    match value {
        BackgroundBox::Border => "border-box",
        BackgroundBox::Padding => "padding-box",
        BackgroundBox::Content => "content-box",
        BackgroundBox::Text => "text",
        BackgroundBox::BorderArea => "border-area",
        BackgroundBox::BorderAreaText => "border-area text",
    }
}

fn serialize_background(values: &[&str]) -> Option<String> {
    let &[color, image, position_x, position_y, size, repeat, attachment, origin, clip] = values else { return None };
    let positions = super::background_positions::synthesize("background-position", &[position_x, position_y])?;
    let values = [color, image, positions.as_str(), size, repeat, attachment, origin, clip];
    let mut components: [Vec<&str>; 7] = core::array::from_fn(|_| Vec::new());
    for (list, value) in components.iter_mut().zip(&values[1..]) {
        *list = top_level_split(value, b',', MAX_BACKGROUND_LAYERS)?;
    }
    let count = components[0].len();
    if components.iter().any(|list| list.len() != count) {
        return None;
    }
    let mut output = String::new();
    for layer in 0..count {
        if layer > 0 {
            output.push_str(", ");
        }
        let start = output.len();
        let [image, position, size, repeat, attachment, origin, clip] =
            components.each_ref().map(|list| list[layer]);
        let mut append = |value: &str| {
            if output.len() > start {
                output.push(' ');
            }
            output.push_str(value);
        };
        if image != "none" {
            append(image);
        }
        if position != "0% 0%" || !matches!(size, "auto" | "auto auto") {
            append(position);
            if !matches!(size, "auto" | "auto auto") {
                append("/");
                append(size);
            }
        }
        if repeat != "repeat" && repeat != "repeat repeat" {
            append(repeat);
        }
        if attachment != "scroll" {
            append(attachment);
        }
        if origin != "padding-box" || clip != "border-box" {
            let special_clip = clip.starts_with("border-area") || clip == "text";
            if !(special_clip && origin == "border-box") {
                append(origin);
            }
            if clip != origin {
                append(clip);
            }
        }
        if layer + 1 == count && !matches!(values[0], "transparent" | "rgba(0, 0, 0, 0)") {
            append(values[0]);
        }
        if output.len() == start {
            output.push_str("none");
        }
        if output.len() > MAX_CSS_BYTES {
            return None;
        }
    }
    Some(output)
}

fn serialize_border_components(width: &str, style: &str, color: &str) -> String {
    serialize_stroke_components([(width, "medium"), (style, "none"), (color, "currentcolor")])
}
fn serialize_outline_components(width: &str, style: &str, color: &str) -> String {
    serialize_stroke_components([(width, "medium"), (style, "none"), (color, "currentcolor")])
}
fn serialize_stroke_components(components: [(&str, &str); 3]) -> String {
    let mut values = [""; 3];
    let mut count = 0;
    for (value, initial) in components {
        if value != initial {
            values[count] = value;
            count += 1;
        }
    }
    if count == 0 {
        "medium".into()
    } else {
        values[..count].join(" ")
    }
}

fn declaration_order_conflict(left: &str, right: &str) -> bool {
    if slots(left).iter().any(|slot| slots(right).contains(slot)) {
        return true;
    }
    // Logical edges use distinct renderer slots and are mapped only after the
    // cascade knows writing-mode/direction. Their author order still matters.
    let logical = |name: &str| name.contains("-inline-") || name.contains("-block-");
    let family = |name: &str| {
        if name.starts_with("margin-") {
            1
        } else if name.starts_with("contain-intrinsic-") {
            7
        } else if name.starts_with("padding-") {
            2
        } else if matches!(name, "top" | "right" | "bottom" | "left") || name.starts_with("inset-")
        {
            3
        } else if name.starts_with("border-") && name.ends_with("-width") {
            4
        } else if name.starts_with("border-") && name.ends_with("-style") {
            5
        } else if name.starts_with("border-") && name.ends_with("-color") {
            6
        } else {
            0
        }
    };
    let left_family = family(left);
    left_family != 0 && left_family == family(right) && (logical(left) || logical(right))
}

fn border_image_initial(name: &str) -> Option<&'static str> {
    match name {
        "border-image-source" => Some("none"),
        "border-image-slice" => Some("100%"),
        "border-image-width" => Some("1"),
        "border-image-outset" => Some("0"),
        "border-image-repeat" => Some("stretch"),
        _ => None,
    }
}

fn append_serialized(
    output: &mut String,
    name: &str,
    value: &str,
    important: bool,
) -> Result<(), CssError> {
    let extra = name
        .len()
        .checked_add(value.len())
        .and_then(|n| n.checked_add(if important { 16 } else { 4 }))
        .ok_or_else(limit)?;
    if output
        .len()
        .checked_add(extra)
        .is_none_or(|n| n > MAX_CSS_BYTES)
    {
        return Err(limit());
    }
    output.try_reserve(extra).map_err(|_| limit())?;
    if !output.is_empty() {
        output.push(' ');
    }
    output.push_str(name);
    output.push_str(": ");
    output.push_str(value);
    if important {
        output.push_str(" !important");
    }
    output.push(';');
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn specification_svg_paint_opacity_specified_numbers_preserve_range_and_roundtrip() {
        for name in ["fill-opacity", "stroke-opacity", "flood-opacity"] {
            let mut block = DeclarationBlock::default();
            for (authored, expected) in [("-100%", "-1"), ("50%", "0.5"), ("300%", "3"), ("0.125", "0.125")] {
                assert!(block.set(name, authored, false).unwrap());
                assert_eq!(block.value(name).unwrap().0, expected);
                let replay = DeclarationBlock::parse(&block.serialize().unwrap()).unwrap();
                assert_eq!(replay.value(name), block.value(name));
            }
            assert!(block.set(name, "inherit", true).unwrap());
            assert_eq!(block.value(name), Some(("inherit".into(), true)));
        }
    }

    #[test]
    fn specification_pending_timings_serialize_through_shared_easing_authority(){
        for name in ["transition-timing-function","animation-timing-function"]{
            let mut block=DeclarationBlock::default();
            assert!(block.set(name,"StEpS(sibling-index(), JuMp-NoNe), StEp-EnD",false).unwrap());
            assert_eq!(block.value(name).unwrap().0,"steps(sibling-index(), jump-none), steps(1)");
            assert!(block.render_declarations().unwrap().iter().any(|declaration|matches!(declaration.value,Value::Deferred(..))));
        }
    }

    #[test]
    fn specification_tree_numeric_sources_are_pending_in_cssom_and_keyframes(){
        let mut block=DeclarationBlock::default();
        for name in ["image-set","-webkit-image-set"]{
            block=DeclarationBlock::default();
            let source=alloc::format!("{name}(url(\"x.png\") calc(1dppx * sibling-index()))");
            assert!(block.set("background-image",&source,false).unwrap());
            let specified=block.value("background-image").unwrap().0;
            assert_eq!(specified,"image-set(url(\"x.png\") calc(1dppx * sibling-index()))");
            assert!(specified.contains("sibling-index()"));
            assert!(block.render_declarations().unwrap().iter().any(|d|matches!(d.value,Value::Deferred(..)|Value::DeferredLonghand(..)|Value::DeferredSlot(..))));
        }
        let pairs=DeclarationBlock::parse_keyframes("top:calc(100px * sibling-index());transform:translateX(calc(10px * sibling-count()))").unwrap().into_keyframe_pairs().unwrap();
        assert!(pairs.iter().any(|(name,value)|name=="top"&&value.contains("sibling-index()")));
        assert!(pairs.iter().any(|(name,value)|name=="transform"&&value.contains("sibling-count()")));
        assert!(!block.set("background-image","image-set(url(\"x.png\") calc(1dppx * sibling-index(1)))",false).unwrap());
    }

    use super::*;
    #[test]
    fn specification_cssom_logical_mapping_setters_reposition_actual_and_virtual_declarations(){
        for (physical,logical,old,new) in [
            ("width","inline-size","1px","4px"),("height","block-size","1px","4px"),
            ("min-width","min-inline-size","1px","4px"),("min-height","min-block-size","1px","4px"),
            ("max-width","max-inline-size","1px","4px"),("max-height","max-block-size","1px","4px"),
            ("margin-left","margin-inline-start","1px","4px"),("padding-top","padding-block-start","1px","4px"),
            ("left","inset-inline-start","1px","4px"),
            ("border-top-width","border-block-start-width","1px","4px"),
            ("border-right-style","border-inline-end-style","solid","dashed"),
            ("border-bottom-color","border-block-end-color","red","blue"),
            ("border-top-left-radius","border-start-start-radius","1px","4px"),
        ]{
            let mut block=DeclarationBlock::parse(&alloc::format!("{physical}:{old};{logical}:{old};color:green")).unwrap();
            assert!(block.set(physical,new,true).unwrap(),"{physical}");
            assert_eq!(block.names().collect::<Vec<_>>(),vec![logical,physical,"color"],"{physical}/{logical}");
            let roundtrip=DeclarationBlock::parse(&block.serialize().unwrap()).unwrap();
            assert_eq!(roundtrip.names().collect::<Vec<_>>(),vec![logical,physical,"color"],"{physical}: serialized mapping order");
            assert_eq!(roundtrip.value(physical),block.value(physical));
            let mut virtual_block=DeclarationBlock::parse("all:initial").unwrap();
            assert!(virtual_block.set(physical,new,false).unwrap());
            let names=virtual_block.names().collect::<Vec<_>>();
            let position=names.iter().position(|name|*name==physical).unwrap();
            assert!(names.iter().enumerate().filter(|(_,name)|different_mapping(physical,name)).all(|(index,_)|index<position),"{physical}: every virtual opposite mapping precedes setter");
            assert!(virtual_block.entries.len()<8,"{physical}: no retained virtual longhand expansion");
        }
        for (source,property,value,expected) in [
            ("width:1px;height:2px;color:red","width","3px",vec!["width","height","color"]),
            ("inline-size:1px;block-size:2px","inline-size","3px",vec!["inline-size","block-size"]),
            ("margin-left:1px;margin-right:2px","margin-left","3px",vec!["margin-left","margin-right"]),
            ("border-image-width:1;border-inline-start-width:2px","border-image-width","initial",vec!["border-image-width","border-inline-start-width"]),
        ]{
            let mut block=DeclarationBlock::parse(source).unwrap();assert!(block.set(property,value,false).unwrap());
            assert_eq!(block.names().collect::<Vec<_>>(),expected,"same mapping and unrelated groups preserve position");
        }
        let mut bounded=DeclarationBlock::parse("all:initial").unwrap();
        for index in 0..MAX_ENTRIES{bounded.set(&alloc::format!("--v{index}"),"token",false).unwrap();}
        let original=bounded.clone();assert!(bounded.set("width","3px",false).is_err());
        assert_eq!(bounded,original,"failed virtual split preserves exact bounded declaration state");
    }
    #[test]
    fn specification_cssom_all_cohorts_priority_order_exclusions_and_roundtrip(){
        let mut block=DeclarationBlock::parse("direction:rtl;unicode-bidi:isolate;--kept:yes;all:REVERT").unwrap();
        assert_eq!(DeclarationBlock::parse(r"all:\69 nitial/**/").unwrap().value("all"),Some(("initial".into(),false)));
        assert_eq!(block.value("all"),Some(("revert".into(),false)));
        assert_eq!(block.value("width"),Some(("revert".into(),false)));
        assert_eq!(block.value("font").unwrap().0,"revert");
        assert_eq!(block.value("margin").unwrap().0,"revert");
        assert!(block.len()>COMPACT_WIDE_SPANS);
        assert_eq!(block.entries.len(),4,"one reset must not allocate a retained longhand list");
        assert!(block.retained_text_bytes()<512);
        let names=block.names().map(String::from).collect::<Vec<_>>();
        assert_eq!(names.iter().filter(|name|name.as_str()=="width").count(),1);
        assert!(!names.iter().any(|name|name=="all"));
        block.set("width","50px",false).unwrap();
        let mut expected=names.clone();expected.retain(|name|name!="width");let after=expected.iter().rposition(|name|matches!(name.as_str(),"inline-size"|"block-size")).unwrap()+1;expected.insert(after,"width".into());
        assert_eq!(block.names().map(String::from).collect::<Vec<_>>(),expected,"CSSOM replacement follows later logical mapping declarations");
        assert!(block.value("all").is_none());
        assert_eq!(block.value("width").unwrap().0,"50px");
        block.set("margin","initial",true).unwrap();
        assert_eq!(block.value("margin"),Some(("initial".into(),true)));
        let serialized=block.serialize().unwrap();
        let roundtrip=DeclarationBlock::parse(&serialized).unwrap();
        for name in block.names(){assert_eq!(roundtrip.value(name),block.value(name),"{name}: {serialized}");}
        block.set("all","unset",false).unwrap();
        assert_eq!(block.value("all"),Some(("unset".into(),false)));
        assert_eq!(block.value("margin"),Some(("unset".into(),false)));
        assert_eq!(block.value("direction").unwrap().0,"rtl");
        assert_eq!(block.value("unicode-bidi").unwrap().0,"isolate");
        assert_eq!(block.value("--kept").unwrap().0,"yes");
        assert!(block.serialize().unwrap().contains("all: unset;"));
        assert_eq!(block.remove("all").unwrap(),"unset");
        assert_eq!(block.len(),3);
        assert!(block.value("width").is_none());
        let protected=DeclarationBlock::parse("width:12px!important;all:initial;color:red").unwrap();
        assert_eq!(protected.value("width"),Some(("12px".into(),true)));
        assert_eq!(protected.value("height").unwrap().0,"initial");
        assert!(protected.value("all").is_none());
        let mut removed=DeclarationBlock::parse("width:20px;color:green;direction:rtl").unwrap();
        assert_eq!(removed.remove("all").unwrap(),"");
        assert_eq!(removed.names().collect::<Vec<_>>(),vec!["direction"]);
    }

    #[test]
    fn specification_cssom_partial_all_sources_project_registry_wide_without_relaxing_ordinary_bounds(){
        let mut block=DeclarationBlock::parse("direction:rtl;unicode-bidi:isolate;--kept:green;all:initial").unwrap();
        block.set("width","37px",false).unwrap();block.set("color","green",false).unwrap();
        let source=block.serialize().unwrap();
        assert!(declaration_spans_with_limit(&source,true,MAX_ENTRIES+PROPERTIES.len()).unwrap().len()>COMPACT_WIDE_SPANS);
        let rule_source=alloc::format!("div{{{source}}}");
        let single=nesting::parse_one_source_rule(&rule_source,&[]).unwrap();
        assert_eq!(single.kind,nesting::SourceRuleKind::Style);
        assert!(nesting::parse_one_source_rule(&alloc::format!("{rule_source} p{{color:red}}"),&[]).is_err(),"registry cohort admission does not relax strict one-rule extra input");
        let sheet=parse_stylesheet(&rule_source).unwrap();
        let kind=NodeKind::Element{namespace:crate::Namespace::Html,name:"div".into(),attributes:Vec::new()};
        let actual=compute(&kind,None,&StyleIndex::new(sheet.rules)).unwrap();
        let expected=computed(&block);
        assert_eq!(actual.width,Some(37.0));assert_eq!(actual.color,expected.color);
        assert_eq!(actual.direction,expected.direction);assert_eq!(actual.unicode_bidi,expected.unicode_bidi);
        assert_eq!(actual.border_width_sides(),expected.border_width_sides());
        assert!(parse_stylesheet(&alloc::format!("div{{{}}}","color:red;".repeat(MAX_DECLARATIONS+1))).is_err(),"ordinary repeated declarations remain bounded before deduplication");
        assert!(DeclarationBlock::parse(&"color:red;".repeat(MAX_ENTRIES+1)).is_err());
        assert_eq!(nesting::parse_one_source_rule(&alloc::format!("div{{{}}}","color:red;".repeat(MAX_DECLARATIONS+1)),&[]).unwrap_err().message,"too many CSS source items");
    }

    #[test]
    fn specification_cssom_all_cohorts_variables_renderer_and_bounded_rollback(){
        let mut block=DeclarationBlock::parse("--reset:initial;all:var(--reset);width:23px").unwrap();
        assert!(block.value("all").is_none());
        assert_eq!(block.value("height").unwrap().0,"");
        assert_eq!(computed(&block).width,Some(23.0));
        block.set("--reset","inherit",false).unwrap();
        block.set("all","var(--reset)",true).unwrap();
        assert_eq!(block.value("all"),Some(("var(--reset)".into(),true)));
        assert_eq!(block.value("width"),Some((String::new(),true)));
        let before=block.clone();
        assert!(!block.set("all","red",false).unwrap());assert_eq!(block,before);
        assert!(!block.set("all","initial inherit",false).unwrap());assert_eq!(block,before);
        for(index,name)in ["width","height","margin-top","border-top-width"].iter().enumerate(){block.set(name,&alloc::format!("{}px",index+1),false).unwrap();}
        block.remove("all").unwrap();assert_eq!(block.names().collect::<Vec<_>>(),vec!["--reset"]);
        let reset=DeclarationBlock::parse("all:initial").unwrap();
        let actual=computed(&reset);
        let expected=compute(&NodeKind::Element{namespace:crate::Namespace::Html,name:"div".into(),attributes:vec![("style".into(),"all:initial".into())]},None,&StyleIndex::new(Vec::new())).unwrap();
        assert_eq!(actual.border_width_sides(),expected.border_width_sides());
        assert_eq!(actual.margin_sides,expected.margin_sides);
        assert_eq!(actual.white_space,expected.white_space);
        assert_eq!(actual.font_size,expected.font_size);
        let mut bounded=DeclarationBlock::parse("all:initial").unwrap();
        for index in 0..MAX_ENTRIES{bounded.set(&alloc::format!("--v{index}"),"token",false).unwrap();}
        let before=bounded.clone();assert!(bounded.set("--overflow","token",false).is_err());assert_eq!(bounded,before);
        bounded.remove("all").unwrap();assert_eq!(bounded.len(),MAX_ENTRIES);
        for index in 0..MAX_ENTRIES{bounded.remove(&alloc::format!("--v{index}")).unwrap();}
        assert!(bounded.is_empty());assert_eq!(bounded.entries.capacity(),0);
    }

    #[test]
    fn declaration_block_outline_expands_canonical_components_and_preserves_offset() {
        let mut block = DeclarationBlock::parse(
            "outline-width:2px;outline-style:dotted;outline-color:blue;outline-offset:-1px",
        )
        .unwrap();
        assert_eq!(block.value("outline").unwrap().0, "2px dotted blue");
        assert_eq!(
            DeclarationBlock::parse(&block.serialize().unwrap())
                .unwrap()
                .value("outline"),
            block.value("outline")
        );
        block.set("outline", "red solid thick", true).unwrap();
        for name in longhands("outline") {
            assert!(block.value(name).unwrap().1);
        }
        assert_eq!(block.value("outline-offset").unwrap().0, "-1px");
        let before = block.clone();
        assert!(!block.set("outline", "red hidden 2px", false).unwrap());
        assert_eq!(block, before);
        block.set("outline-style", "dashed", false).unwrap();
        assert!(block.value("outline").is_none());
        block.remove("outline").unwrap();
        assert!(block.value("outline-width").is_none());
        assert_eq!(block.value("outline-offset").unwrap().0, "-1px");
        let mut pending =
            DeclarationBlock::parse("--edge:blue dotted 2px;outline:var(--edge)!important")
                .unwrap();
        assert_eq!(pending.value("outline").unwrap().0, "var(--edge)");
        assert_eq!(computed(&pending).outline.width, 2.0);
        pending.set("outline-color", "red", false).unwrap();
        assert!(pending.value("outline").is_none());
    }

    #[test]
    fn declaration_block_grid_projects_relative_tracks_area_rows_and_boundary_names() {
        let block = DeclarationBlock::parse(
            "grid-template:[top] \"a a\" [bottom] [next] \"b b\" 1em / repeat(2,1fr)",
        )
        .unwrap();
        assert_eq!(
            block.value("grid-template-rows").unwrap().0,
            "[top] auto [bottom next] 1em"
        );
        assert_eq!(
            block.value("grid-template-columns").unwrap().0,
            "repeat(2, 1fr)"
        );
        assert_eq!(
            block.value("grid-template-areas").unwrap().0,
            "\"a a\" \"b b\""
        );
        let serialized = block.serialize().unwrap();
        let roundtrip = DeclarationBlock::parse(&serialized).unwrap();
        for name in longhands("grid-template") {
            assert_eq!(
                roundtrip.value(name),
                block.value(name),
                "{name}: {serialized}"
            );
        }
        assert_eq!(
            computed(&block).grid_rows.as_deref().unwrap()[1],
            GridTrack::Pixels(16.0)
        );
    }

    #[test]
    fn declaration_block_grid_mutations_keep_resets_priority_and_pending_cohorts_atomic() {
        let mut block =
            DeclarationBlock::parse("grid:auto-flow 1em / 10px!important;grid-auto-rows:9px")
                .unwrap();
        assert_eq!(block.value("grid-auto-rows").unwrap(), ("1em".into(), true));
        block.set("grid-auto-rows", "9px", false).unwrap();
        assert_eq!(
            block.value("grid-auto-rows").unwrap(),
            ("9px".into(), false)
        );
        assert!(block.value("grid").is_none());
        block.set("grid-template", "none", false).unwrap();
        assert_eq!(block.value("grid-auto-rows").unwrap().0, "9px");
        let before = block.clone();
        assert!(!block
            .set("grid-template", "\"a b\" \"a\" / 10px", false)
            .unwrap());
        assert_eq!(block, before);
        let too_many = "1px ".repeat(MAX_GRID_TRACKS + 1);
        assert!(!block
            .set("grid-template", &alloc::format!("{too_many} / 10px"), false)
            .unwrap());
        assert_eq!(block, before);
        let mut pending =
            DeclarationBlock::parse("--tracks:20px / 30px;grid:var(--tracks)!important").unwrap();
        assert_eq!(pending.value("grid").unwrap().0, "var(--tracks)");
        pending.set("grid-template-columns", "10px", false).unwrap();
        assert!(pending.value("grid").is_none());
        let resolved = computed(&pending);
        assert_eq!(
            resolved.grid_columns.as_deref().unwrap(),
            &[GridTrack::Pixels(10.0)]
        );
        assert_eq!(
            resolved.grid_rows.as_deref().unwrap(),
            &[GridTrack::Pixels(20.0)]
        );
        pending.remove("grid-template").unwrap();
        assert!(pending.value("grid-template-rows").is_none());
        assert_eq!(pending.value("grid-auto-flow").unwrap().1, true);
    }

    #[test]
    fn declaration_block_grid_lanes_transposes_area_rows_and_roundtrips_state() {
        let block = DeclarationBlock::parse(
            "grid-lanes:\"a a b\" 1em 2fr 3fr row track-reverse fill-reverse",
        )
        .unwrap();
        assert_eq!(block.value("grid-template-columns").unwrap().0, "none");
        assert_eq!(block.value("grid-template-rows").unwrap().0, "1em 2fr 3fr");
        assert_eq!(
            block.value("grid-template-areas").unwrap().0,
            "\"a\" \"a\" \"b\""
        );
        assert_eq!(
            block.value("grid-lanes-direction").unwrap().0,
            "row track-reverse fill-reverse"
        );
        let text = block.serialize().unwrap();
        let roundtrip = DeclarationBlock::parse(&text).unwrap();
        for name in longhands("grid-lanes") {
            assert_eq!(roundtrip.value(name), block.value(name), "{name}: {text}");
        }
        let mut mixed = block.clone();
        mixed.set("grid-template-columns", "10px", false).unwrap();
        assert!(mixed.value("grid-lanes").is_none());
    }

    #[test]
    fn declaration_block_background_projects_priorities_pending_values_and_relative_layers() {
        let block = DeclarationBlock::parse(
            "background-color:blue; background:red!important; background-color:green",
        )
        .unwrap();
        assert_eq!(block.value("background").unwrap(), ("red".into(), true));
        assert_eq!(computed(&block).background, color("red").unwrap());
        let mut block = DeclarationBlock::parse("background:red").unwrap();
        block.set("background-color", "green", true).unwrap();
        assert_eq!(block.value("background"), None);
        assert_eq!(computed(&block).background, color("green").unwrap());
        let mut block = DeclarationBlock::parse("--bg:red; background:var(--bg)").unwrap();
        assert_eq!(block.value("background").unwrap().0, "var(--bg)");
        assert_eq!(block.value("background-position").unwrap().0, "");
        block.set("background-color", "blue", false).unwrap();
        assert_eq!(block.value("background"), None);
        assert_eq!(computed(&block).background, color("blue").unwrap());
        let block = DeclarationBlock::parse("background:url(\"a,b.png\") 1em 20% / 2em auto no-repeat fixed content-box border-box, red").unwrap();
        assert_eq!(
            block.value("background-position").unwrap().0,
            "1em 20%, 0% 0%"
        );
        assert_eq!(block.value("background-size").unwrap().0, "2em auto, auto");
        let text = block.serialize().unwrap();
        let roundtrip = DeclarationBlock::parse(&text).unwrap();
        for name in longhands("background") {
            assert_eq!(roundtrip.value(name), block.value(name), "{name}: {text}");
        }
        let block = DeclarationBlock::parse("color:blue;background:currentcolor").unwrap();
        assert_eq!(block.value("background-color").unwrap().0, "currentcolor");
        assert_eq!(computed(&block).background, color("blue").unwrap());
    }

    #[test]
    fn declaration_block_list_style_keeps_real_longhands_and_none_ambiguity() {
        let block = DeclarationBlock::parse(
            "list-style-type:circle;list-style-position:inside;list-style-image:none",
        )
        .unwrap();
        assert_eq!(block.serialize().unwrap(), "list-style: inside circle;");
        let block = DeclarationBlock::parse("list-style:inside none").unwrap();
        assert_eq!(block.value("list-style-type").unwrap().0, "none");
        assert_eq!(block.value("list-style-image").unwrap().0, "none");
        assert_eq!(block.value("list-style").unwrap().0, "inside none");
        let mut block = DeclarationBlock::parse("list-style:var(--markers)").unwrap();
        assert_eq!(block.value("list-style-type").unwrap().0, "");
        assert_eq!(block.value("list-style").unwrap().0, "var(--markers)");
        block.set("list-style-position", "inside", false).unwrap();
        assert_eq!(block.value("list-style"), None);
    }

    #[test]
    fn declaration_block_border_serialization_requires_reset_only_components() {
        let sides = "border-width:1px; border-style:solid; border-color:black";
        let block = DeclarationBlock::parse(sides).unwrap();
        assert_eq!(block.value("border"), None);
        assert!(!block.serialize().unwrap().contains("border:"));
        let mut block = DeclarationBlock::parse("border:1px solid black").unwrap();
        assert_eq!(block.value("border").unwrap().0, "1px solid black");
        assert_eq!(block.value("border-image-slice").unwrap().0, "100%");
        assert_eq!(block.serialize().unwrap(), "border: 1px solid black;");
        assert!(block.set("border-image-source", "inherit", false).unwrap());
        assert_eq!(block.value("border"), None);
        let roundtrip = DeclarationBlock::parse(&block.serialize().unwrap()).unwrap();
        assert_eq!(roundtrip.value("border-image-source").unwrap().0, "inherit");
        assert_eq!(roundtrip.value("border"), None);
        assert!(block.set("border", "2px dashed red", true).unwrap());
        assert_eq!(
            block.value("border").unwrap(),
            ("2px dashed red".into(), true)
        );
        assert!(block.set("border-image-width", "1", false).unwrap());
        assert_eq!(block.value("border"), None);
        block.remove("border").unwrap();
        assert!(block.is_empty());
    }

    #[test]
    fn declaration_block_border_groups_omit_defaults_and_preserve_edge_order() {
        let block = DeclarationBlock::parse("border:1px red").unwrap();
        assert_eq!(block.serialize().unwrap(), "border: 1px red;");
        let block = DeclarationBlock::parse(
            "border-top:1px;border-right:2px;border-bottom:3px;border-left:4px",
        )
        .unwrap();
        let text = block.serialize().unwrap();
        assert!(text.contains("border-width: 1px 2px 3px 4px;"), "{text}");
        assert!(text.contains("border-style: none;"), "{text}");
        assert!(text.contains("border-color: currentcolor;"), "{text}");
        let block = DeclarationBlock::parse(
            "border-top:1px;border-right:1px;border-bottom:1px;border-left:1px;border-image:none",
        )
        .unwrap();
        assert_eq!(block.serialize().unwrap(), "border: 1px;");
        let block = DeclarationBlock::parse("margin-top:1px; margin-inline-start:9px; margin-right:2px; margin-bottom:3px; margin-left:4px").unwrap();
        let text = block.serialize().unwrap();
        assert!(!text.contains("margin:"), "{text}");
        assert_eq!(
            computed(&DeclarationBlock::parse(&text).unwrap()).margin_sides,
            computed(&block).margin_sides
        );
    }

    fn computed(block: &DeclarationBlock) -> Style {
        let mut rules = super::super::parse("div {}").unwrap();
        rules[0].set_cssom_declarations(block).unwrap();
        let kind = NodeKind::Element {
            namespace: crate::Namespace::Html,
            name: "div".into(),
            attributes: Vec::new(),
        };
        compute(&kind, None, &StyleIndex::new(rules)).unwrap()
    }

    #[test]
    fn declaration_block_width_aliases_share_identity_priority_and_lossless_shorthand() {
        let mut block = DeclarationBlock::parse("font-stretch:75%!important;font-width:125%").unwrap();
        assert_eq!(block.len(),1);
        assert_eq!(block.names().collect::<Vec<_>>(),vec!["font-width"]);
        assert_eq!(block.value("font-width"),Some(("75%".into(),true)));
        assert_eq!(block.value("font-stretch"),block.value("font-width"));
        assert!(block.set("font-stretch","condensed",false).unwrap());
        assert_eq!(block.value("font-width"),Some(("condensed".into(),false)));
        assert_eq!(block.remove("font-width").unwrap(),"condensed");
        assert!(block.is_empty());
        block.set("font","16px serif",false).unwrap();
        block.set("font-width","75%",false).unwrap();
        block.set("font-width","calc(50%)",false).unwrap();
        assert_eq!(block.value("font-width").unwrap().0,"calc(50%)");
        assert!(block.value("font").unwrap().0.starts_with("ultra-condensed "));
        block.set("font-width","75%",false).unwrap();
        assert_eq!(block.value("font").unwrap().0,"condensed 16px serif");
        block.set("font-stretch","76%",false).unwrap();
        assert!(block.value("font").is_none(),"shorthand grammar permits width keywords, never an ambiguous percentage");
        block.set("font-width","calc(100% + sign(20cqw - 10px)*5%)",false).unwrap();
        assert_eq!(block.value("font-stretch").unwrap().0,"calc(100% + (5% * sign(20cqw - 10px)))");
        let roundtrip = DeclarationBlock::parse(&block.serialize().unwrap()).unwrap();
        assert_eq!(roundtrip.value("font-width"),block.value("font-width"));
        block.set("font","16px serif",false).unwrap();
        assert_eq!(block.value("font-width").unwrap().0,"normal");
    }

    #[test]
    fn declaration_block_feature_settings_preserve_specified_duplicates_and_font_reset() {
        let block = DeclarationBlock::parse("font-feature-settings:'smcp' on,'liga' 0,'smcp' off;font-variant:small-caps").unwrap();
        assert_eq!(block.value("font-feature-settings").unwrap().0,"\"smcp\", \"liga\" 0, \"smcp\" 0");
        let roundtrip = DeclarationBlock::parse(&block.serialize().unwrap()).unwrap();
        assert_eq!(computed(&block).font,computed(&roundtrip).font);
        let reset = DeclarationBlock::parse("font-feature-settings:'liga' off;font:16px serif").unwrap();
        assert_eq!(reset.value("font-feature-settings").unwrap().0,"normal");
        assert_eq!(reset.value("font").unwrap().0,"16px serif");
        let controlled = DeclarationBlock::parse("font:16px serif;font-feature-settings:'liga' off").unwrap();
        assert!(controlled.value("font").is_none());
        let escaped = DeclarationBlock::parse("font-feature-settings:'\\22\\7d\\2f\\2a'").unwrap();
        assert_eq!(escaped.value("font-feature-settings").unwrap().0,"\"\\\"}/*\"");
    }

    #[test]
    fn declaration_block_ligatures_expand_and_preserve_lossless_shorthand_serialization() {
        let block = DeclarationBlock::parse("font-variant:small-caps contextual no-common-ligatures").unwrap();
        assert_eq!(block.value("font-variant-ligatures").unwrap().0, "no-common-ligatures contextual");
        assert_eq!(block.value("font-variant").unwrap().0, "no-common-ligatures contextual small-caps");
        let roundtrip = DeclarationBlock::parse(&block.serialize().unwrap()).unwrap();
        assert_eq!(computed(&roundtrip).font, computed(&block).font);
        let none = DeclarationBlock::parse("font-variant:none").unwrap();
        assert_eq!(none.value("font-variant").unwrap().0, "none");
        let controlled = DeclarationBlock::parse("font:16px serif;font-variant-ligatures:none").unwrap();
        assert!(controlled.value("font").is_none());
        let reset = DeclarationBlock::parse("font-variant-ligatures:none;font:16px serif").unwrap();
        assert_eq!(reset.value("font-variant-ligatures").unwrap().0, "normal");
        assert_eq!(reset.value("font").unwrap().0, "16px serif");
    }

    #[test]
    fn declaration_block_oblique_angles_preserve_specified_units_and_round_trip() {
        for (raw, specified, degrees) in [("oblique 10grad", "oblique 10grad", 9.0),
            ("oblique calc(100deg)", "oblique calc(100deg)", 90.0), ("oblique 0deg", "normal", 0.0)] {
            let block = DeclarationBlock::parse(&alloc::format!("font-style:{raw}")).unwrap();
            assert_eq!(block.value("font-style").unwrap().0, specified, "{raw}");
            assert_eq!(computed(&block).font.style.angle(), Some(degrees));
            let roundtrip = DeclarationBlock::parse(&block.serialize().unwrap()).unwrap();
            assert_eq!(computed(&roundtrip).font.style, computed(&block).font.style);
        }
        let block = DeclarationBlock::parse("font:oblique 25deg small-caps bold condensed 16px serif").unwrap();
        let serialized = block.value("font").unwrap().0;
        assert!(serialized.contains("oblique 25deg"));
        let roundtrip = DeclarationBlock::parse(&alloc::format!("font:{serialized}")).unwrap();
        assert_eq!(computed(&roundtrip).font, computed(&block).font);
    }

    #[test]
    fn declaration_block_font_keywords_system_fonts_and_family_lists_round_trip() {
        let families = DeclarationBlock::parse(r#"font-family:"serif", "inherit", Serif, "Serif", "New Century Schoolbook""#).unwrap();
        let canonical_families = r#""serif", "inherit", serif, "Serif", New Century Schoolbook"#;
        assert_eq!(families.value("font-family").unwrap().0, canonical_families);
        let roundtrip = DeclarationBlock::parse(&families.serialize().unwrap()).unwrap();
        assert_eq!(computed(&roundtrip).font, computed(&families).font);
        assert_eq!(DeclarationBlock::parse("font-family:FirstName, LastName, revert-rule").unwrap().value("font-family"), None);
        let mut block = DeclarationBlock::parse("font:condensed small-caps italic bold xx-large/normal FB Armada, serif").unwrap();
        let canonical = "italic small-caps bold condensed xx-large FB Armada, serif";
        assert_eq!(block.value("font").unwrap().0, canonical);
        assert_eq!(block.value("font-size").unwrap().0, "xx-large");
        let serialized = block.serialize().unwrap();
        assert_eq!(DeclarationBlock::parse(&serialized).unwrap().value("font").unwrap().0, canonical);
        assert_eq!(computed(&block).font_size, 32.0);
        for keyword in ["caption", "icon", "menu", "message-box", "small-caption", "status-bar"] {
            block.set("font", canonical, false).unwrap();
            assert_eq!(block.value("font").unwrap().0, canonical, "reset before {keyword}");
            assert!(block.set("font", keyword, false).unwrap(), "{keyword} must replace the previous font");
            let serialized = block.value("font").unwrap().0;
            assert_eq!(serialized, "16px sans-serif", "{keyword} UA font substitution");
            let mut roundtrip = DeclarationBlock::default();
            assert!(roundtrip.set("font", &serialized, false).unwrap(), "{keyword} serialization {serialized:?} must parse in an empty block");
            assert_eq!(roundtrip.value("font").unwrap().0, serialized, "{keyword} serialization round-trip");
            assert_eq!(computed(&roundtrip).font, computed(&block).font, "{keyword} selection round-trip");
            assert_eq!(computed(&roundtrip).font_size, computed(&block).font_size, "{keyword} size round-trip");
            assert!(!block.set("font", &serialized, false).unwrap(), "{keyword} identical expanded values must be a no-op");
        }
        assert!(block.set("font-size", "LARGER", false).unwrap());
        assert_eq!(block.value("font-size").unwrap().0, "larger");
        assert!(block.set("font", "0px/12px emoji, math, ui-serif", true).unwrap());
        assert_eq!(block.value("font").unwrap(), ("0px / 12px emoji, math, ui-serif".into(), true));
        let zero = computed(&block);
        assert_eq!(zero.font_size, 0.0);
        assert_eq!(zero.line_height, LineHeight::Pixels(12.0));
        assert!(!block.set("font", "normal 100 semi-condensed oblique small-caps 10px Menu", false).unwrap());
    }

    #[test]
    fn declaration_block_source_priority_differs_from_explicit_mutation() {
        let mut block = DeclarationBlock::parse(
            "margin: 1px 2px!important; margin-left:9px; padding:3px; margin-top:4px!important",
        )
        .unwrap();
        assert_eq!(block.value("margin-left"), Some(("2px".into(), true)));
        assert_eq!(block.value("margin"), Some(("4px 2px 1px".into(), true)));
        assert!(block.set("margin-left", "9px", false).unwrap());
        assert_eq!(block.value("margin"), None);
        assert_eq!(computed(&block).margin_sides, [4.0, 2.0, 1.0, 9.0]);
        assert_eq!(block.remove("margin").unwrap(), "");
        assert_eq!(
            block.names().collect::<Vec<_>>(),
            [
                "padding-top",
                "padding-right",
                "padding-bottom",
                "padding-left"
            ]
        );
    }

    #[test]
    fn declaration_block_word_wrap_is_a_longhand_alias_with_shared_priority() {
        let mut block = DeclarationBlock::parse(
            "word-wrap:anywhere;overflow-wrap:normal!important;word-wrap:break-word;word-break:BREAK-ALL",
        ).unwrap();
        assert_eq!(block.names().collect::<Vec<_>>(), ["overflow-wrap", "word-break"]);
        assert_eq!(block.value("word-wrap"), Some(("normal".into(), true)));
        assert_eq!(block.value("overflow-wrap"), block.value("word-wrap"));
        assert_eq!(computed(&block).word_break, WordBreak::BreakAll);
        assert!(block.set("word-wrap", "break-word", false).unwrap());
        assert_eq!(computed(&block).overflow_wrap, OverflowWrap::BreakWord);
        assert_eq!(block.remove("word-wrap").unwrap(), "break-word");
        assert!(block.value("overflow-wrap").is_none());
    }

    #[test]
    fn declaration_block_pending_cohorts_project_only_surviving_longhands() {
        let mut block =
            DeclarationBlock::parse("--edges:1px 2px 3px 4px; margin:var(--edges)!important")
                .unwrap();
        assert_eq!(block.value("margin"), Some(("var(--edges)".into(), true)));
        assert_eq!(block.value("margin-top"), Some((String::new(), true)));
        let groups: Vec<_> = block
            .entries
            .iter()
            .filter_map(|entry| {
                if let Specified::Pending(group) = &entry.value {
                    Some(group)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(groups.len(), 4);
        assert!(groups.iter().all(|group| Arc::ptr_eq(groups[0], group)));
        assert!(block.set("margin-left", "9px", false).unwrap());
        assert_eq!(block.value("margin"), None);
        assert_eq!(computed(&block).margin_sides, [1.0, 2.0, 3.0, 9.0]);
        block.set("--edges", "not-a-length", false).unwrap();
        assert_eq!(computed(&block).margin_sides, [0.0, 0.0, 0.0, 9.0]);
        let quoted = DeclarationBlock::parse(r#"font: italic 12px "var(--literal)""#).unwrap();
        assert_eq!(
            quoted.value("font-family").unwrap().0,
            r#""var(--literal)""#
        );
        assert!(quoted.value("font").is_some());
    }

    #[test]
    fn declaration_block_relative_values_and_shared_shorthand_cascade_agree() {
        let widths =
            DeclarationBlock::parse("border-width:thin medium thick 1px; column-rule:hidden")
                .unwrap();
        assert_eq!(
            widths.value("border-width").unwrap().0,
            "thin medium thick 1px"
        );
        assert_eq!(widths.value("column-rule-width").unwrap().0, "medium");
        assert_eq!(widths.value("column-rule-style").unwrap().0, "hidden");
        let source = "font:italic small-caps bold condensed 2em / 1.5 serif; gap:calc(1em + 10%) 25%; margin:1em auto 3px; border:2px dashed red; flex:2 3 10%; place-items:safe center end";
        let block = DeclarationBlock::parse(source).unwrap();
        assert_eq!(block.value("font-size").unwrap().0, "2em");
        assert_eq!(block.value("line-height").unwrap().0, "1.5");
        assert_eq!(block.value("flex-basis").unwrap().0, "10%");
        assert_eq!(block.value("align-items").unwrap().0, "safe center");
        assert_eq!(block.value("justify-items").unwrap().0, "end");
        let kind = NodeKind::Element {
            namespace: crate::Namespace::Html,
            name: "div".into(),
            attributes: vec![("style".into(), source.into())],
        };
        let expected = compute(&kind, None, &StyleIndex::new(Vec::new())).unwrap();
        let actual = computed(&block);
        assert_eq!(actual.font_size, expected.font_size);
        assert_eq!(actual.font, expected.font);
        assert_eq!(actual.margin_sides, expected.margin_sides);
        assert_eq!(actual.margin_auto, expected.margin_auto);
        assert_eq!(actual.gap, expected.gap);
        assert_eq!(actual.column_gap, expected.column_gap);
        assert_eq!(actual.flex_basis, expected.flex_basis);
        assert_eq!(actual.border_width_sides(), expected.border_width_sides());
        assert_eq!(actual.border_styles(), expected.border_styles());
        let roundtrip = DeclarationBlock::parse(&block.serialize().unwrap()).unwrap();
        assert_eq!(computed(&roundtrip).font, expected.font);
        assert_eq!(
            DeclarationBlock::parse("gap:normal 2px")
                .unwrap()
                .value("row-gap")
                .unwrap()
                .0,
            "normal"
        );
    }

    #[test]
    fn declaration_block_holds_hundreds_of_custom_properties_and_serializes_them() {
        assert_eq!(MAX_ENTRIES, MAX_DECLARATIONS);
        let source: String = (0..300).map(|index| alloc::format!("--token-{index}:{index};")).collect();
        let mut block = DeclarationBlock::parse(&source).unwrap();
        assert_eq!(block.len(), 300);
        assert_eq!(block.value("--token-299"), Some(("299".into(), false)));
        block.set("--token-300", "300", false).unwrap();
        block.set("color", "red", false).unwrap();
        assert_eq!(block.len(), 302);
        let serialized = block.serialize().unwrap();
        let reparsed = DeclarationBlock::parse(&serialized).unwrap();
        assert_eq!(reparsed.len(), 302);
        assert_eq!(reparsed.value("--token-0"), Some(("0".into(), false)));
        let too_many: String = (0..=MAX_ENTRIES).map(|index| alloc::format!("--t{index}:1;")).collect();
        assert!(DeclarationBlock::parse(&too_many).is_err());
    }

    #[test]
    fn declaration_block_atomic_limits_replacement_and_clear_reclaim() {
        let mut block = DeclarationBlock::default();
        for index in 0..MAX_ENTRIES {
            block
                .set(&alloc::format!("--v{index}"), "token", false)
                .unwrap();
        }
        let snapshot = block.clone();
        assert!(block.set("margin", "1px", false).is_err());
        assert_eq!(block, snapshot);
        for _ in 0..64 {
            assert!(!block.set("--v0", "token", false).unwrap());
        }
        assert_eq!(block.len(), MAX_ENTRIES);
        assert!(!block.set("--v0", "value; color:red", false).unwrap());
        assert_eq!(block, snapshot);
        for index in 0..MAX_ENTRIES {
            block.remove(&alloc::format!("--v{index}")).unwrap();
        }
        assert!(block.is_empty());
        assert_eq!(block.entries.capacity(), 0);
        assert_eq!(block.retained_text_bytes(), 0);
    }
    #[test]
    fn declaration_block_rule_overlays_match_owner_source_offset_and_rebuild_metadata() {
        let source: Arc<str> = Arc::from("div, span { margin:1px }");
        let mut rules = super::super::parse(&source).unwrap();
        let owner = StylesheetIdentity::Adopted {
            scope: None,
            index: 0,
        };
        for rule in &mut rules {
            rule.stylesheet_owner = Some(owner);
        }
        let offset = rules[0].declaration_offset;
        let mut retained = RuleDeclarationOverride {
            owner,
            source_text: source.clone(),
            source_url: None,
            declaration_offset: offset,
            cssom_path: Arc::from([0usize]),
            block: alloc::rc::Rc::new(
                DeclarationBlock::parse("margin:4px; counter-reset:chapter 7").unwrap(),
            ),
        };
        let mut index = StyleIndex::new(rules.clone());
        retained.owner = StylesheetIdentity::Adopted {
            scope: None,
            index: 1,
        };
        index
            .apply_cssom_rule_overrides(core::slice::from_ref(&retained))
            .unwrap();
        assert!(!index.has_counter_data);
        retained.owner = owner;
        retained.source_text = Arc::from("div { margin:2px }");
        index
            .apply_cssom_rule_overrides(core::slice::from_ref(&retained))
            .unwrap();
        assert!(!index.has_counter_data);
        retained.source_text = source;
        retained.declaration_offset += 1;
        index
            .apply_cssom_rule_overrides(core::slice::from_ref(&retained))
            .unwrap();
        assert!(!index.has_counter_data);
        retained.declaration_offset = offset;
        index
            .apply_cssom_rule_overrides(core::slice::from_ref(&retained))
            .unwrap();
        assert!(index.has_counter_data);
        assert!(Arc::ptr_eq(
            &index.rules[0].declarations,
            &index.rules[1].declarations
        ));
        let kind = NodeKind::Element {
            namespace: crate::Namespace::Html,
            name: "div".into(),
            attributes: Vec::new(),
        };
        assert_eq!(compute(&kind, None, &index).unwrap().margin_sides, [4.0; 4]);
        let nested = super::super::parse("@media all { div { color:red } }").unwrap();
        assert_eq!(nested[0].declaration_offset, "@media all { div {".len());
    }

    #[test]
    fn declaration_block_css_property_identity_is_independent_of_renderer_slots() {
        for shorthand in [
            "columns",
            "font-variant",
            "grid-column",
            "background",
            "border-radius",
            "place-items",
        ] {
            assert!(is_shorthand(shorthand), "{shorthand}");
        }
        for longhand in [
            "width",
            "height",
            "flex-wrap",
            "border-spacing",
            "font-family",
            "transform-origin",
            "border-start-start-radius",
            "border-start-end-radius",
            "border-end-start-radius",
            "border-end-end-radius",
            "--custom",
        ] {
            assert!(!is_shorthand(longhand), "{longhand}");
        }
        assert!(slots("width").len() > 1);
        assert_eq!(slots("columns").len(), 4);
    }
    #[test]
    fn specification_appearance_legacy_alias_has_one_canonical_declaration_identity() {
        let mut block=DeclarationBlock::parse("-webkit-appearance:none!important;appearance:auto").unwrap();
        assert_eq!(block.len(),1);assert_eq!(block.names().collect::<Vec<_>>(),vec!["appearance"]);
        assert_eq!(block.value("-webkit-appearance"),Some(("none".into(),true)));
        assert_eq!(block.serialize().unwrap(),"appearance: none !important;");
        for value in ["initial","inherit","unset","revert","revert-layer","auto"] {
            assert!(block.set("-webkit-appearance",value,false).unwrap());
            assert_eq!(block.value("appearance"),block.value("-webkit-appearance"));
            assert_eq!(block.names().collect::<Vec<_>>(),vec!["appearance"]);
            assert_eq!(block.remove("appearance").unwrap(),value);
            assert!(block.is_empty());
        }
        block.set("appearance","none",true).unwrap();
        assert_eq!(block.remove("-webkit-appearance").unwrap(),"none");
        assert!(block.is_empty());
    }

}
