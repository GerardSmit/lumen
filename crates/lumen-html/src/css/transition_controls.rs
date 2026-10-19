use super::*;

fn property(raw: &str) -> Option<Arc<str>> {
    let raw = raw.trim();
    let mut position = 0;
    let name = consume_selector_identifier(raw, &mut position)?;
    if position != raw.len() { return None; }
    let lower = name.to_ascii_lowercase();
    if matches!(lower.as_str(), "initial"|"inherit"|"unset"|"revert"|"revert-layer"|"default") { return None; }
    if lower == "all" || lower == "none" { return Some(Arc::from(lower)); }
    if name.starts_with("--") { return Some(Arc::from(name)); }
    if PROPERTIES.iter().any(|entry|entry.name == declaration_block::canonical_alias(&lower)) {
        Some(Arc::from(lower))
    } else { Some(Arc::from(name)) }
}

pub(super) fn property_values(raw: &str) -> Option<Arc<[Arc<str>]>> {
    let parts = top_level_split(raw,b',',64)?;
    let mut values = Vec::new(); values.try_reserve_exact(parts.len()).ok()?;
    for part in parts { values.push(property(part)?); }
    if values.is_empty() || values.len()>1 && values.iter().any(|name|name.as_ref()=="none") { return None; }
    Some(values.into())
}

pub(super) fn easing(raw: &str) -> Option<String> {
    let stripped = strip_component_comments(raw, 0, true).ok()?;
    let raw = stripped.trim();
    let mut position = 0;
    let name = consume_selector_identifier(raw,&mut position)?.to_ascii_lowercase();
    let normalized = if position == raw.len() { name } else {
        let arguments = raw.get(position..)?.strip_prefix('(')?.strip_suffix(')')?;
        alloc::format!("{name}({arguments})").to_ascii_lowercase()
    };
    if normalized.starts_with("steps(") { return specified_steps(&normalized); }
    crate::animation::ease(&normalized,0.5)?;
    match normalized.as_str() {
        "step-start" => return Some("steps(1, start)".into()),
        "step-end" => return Some("steps(1)".into()),
        "linear"|"ease"|"ease-in"|"ease-out"|"ease-in-out" => return Some(normalized),
        _ => {}
    }
    if normalized.starts_with("linear(") { return crate::animation::serialize_linear_easing(&normalized); }
    let open = normalized.find('(')?;
    let parts = top_level_split(&normalized[open+1..normalized.len()-1],b',',4)?;
    let mut numbers = Vec::new();
    for part in parts {
        let value = typed_numeric::parse_numeric_value(part.trim())?;
        if value.unit != typed_numeric::NumericUnit::Number {return None;}
        numbers.push(typed_numeric::serialize_numeric_value(value.value,value.unit));
    }
    Some(alloc::format!("cubic-bezier({})",numbers.join(", ")))
}

fn steps_parts(raw:&str)->Option<(&str,&str)> {
    let body=raw.strip_prefix("steps(")?.strip_suffix(')')?;
    let parts=top_level_split(body,b',',2)?;
    let count=parts.first()?.trim();
    let position=parts.get(1).map_or("end",|position|position.trim());
    if !matches!(position,"start"|"end"|"jump-start"|"jump-end"|"jump-none"|"jump-both"){return None;}
    Some((count,position))
}

fn steps_expression(input:&str,index:usize,count:usize)->Option<(typed_numeric::NumericExpression,bool)> {
    let (resolved,sibling)=sibling_context_colors(input,index,count)?;
    let expression=typed_numeric::parse_numeric_expression(&resolved)?;
    if expression.numeric_type()?!=typed_numeric::NumericType::from_unit(typed_numeric::NumericUnit::Number){return None;}
    Some((expression,sibling))
}

fn specified_steps(raw:&str)->Option<String> {
    let (count,position)=steps_parts(raw)?;
    let (_,sibling)=steps_expression(count,1,1)?;
    let calculated=math_function(count)||sibling;
    let count=if calculated {String::from(count)} else {
        let digits=count.strip_prefix('+').unwrap_or(count);
        if digits.is_empty() || !digits.bytes().all(|byte|byte.is_ascii_digit()){return None;}
        let integer=digits.parse::<u64>().ok()?;
        if integer < if position=="jump-none"{2}else{1}{return None;}
        integer.min(u64::from(u32::MAX)).to_string()
    };
    Some(if matches!(position,"end"|"jump-end") {alloc::format!("steps({count})")}
        else {alloc::format!("steps({count}, {position})")})
}

// Keep number-valued functions calculated after substituting their contextual
// coordinates. Their integer range is clamped at computed-value time, rather
// than rejecting a valid function as though it were a specified integer token.
pub(super) fn substitute_sibling_timings(raw:&str,index:usize,count:usize)->Option<String> {
    let parts=top_level_split(raw,b',',64)?;
    let mut result=Vec::new();result.try_reserve_exact(parts.len()).ok()?;
    for part in parts {
        let normalized=easing(part)?;
        if let Some((number,position))=steps_parts(&normalized) {
            let (resolved,sibling)=sibling_context_colors(number,index,count)?;
            if sibling {
                result.push(if matches!(position,"end"|"jump-end") {alloc::format!("steps(calc({resolved}))")}
                    else {alloc::format!("steps(calc({resolved}), {position})")});
                continue;
            }
        }
        result.push(normalized);
    }
    Some(result.join(", "))
}

pub(super) fn resolve_timing(raw:&str,length:LengthContext,query:ContainerUnitContext,index:usize,count:usize)->Option<String> {
    let normalized=easing(raw)?;let raw=normalized.as_str();
    if !raw.starts_with("steps("){return Some(normalized);}
    let (number,position)=steps_parts(raw)?;
    let (expression,_)=steps_expression(number,index,count)?;
    let value=expression.evaluate(&mut FontAngleContext{length:Some(length),query,percent_scale:1.0})?;
    computed_steps_value(value,position)
}

fn computed_steps_value(value:f64,position:&str)->Option<String> {
    if !value.is_finite(){return None;}
    let minimum=if position=="jump-none"{2.0}else{1.0};
    let value=(value+0.5).floor().clamp(minimum,u32::MAX as f64) as u32;
    Some(if matches!(position,"end"|"jump-end") {alloc::format!("steps({value})")}
        else {alloc::format!("steps({value}, {position})")})
}

// Source-preserving numeric consumers (including interpolation maps) already
// own their contextual numeric leaf resolver. Reuse the same timing grammar,
// integer range and serializer without constructing a second length context.
pub(super) fn computationally_independent_easing(raw:&str)->bool {
    let Some(normalized)=easing(raw)else{return false;};
    let Some((count,_))=steps_parts(&normalized)else{return true;};
    typed_numeric::parse_numeric_expression(count).is_some_and(|expression|
        !expression.contains_tree_functions()&&!expression.contains_unit(|unit|!typed_numeric::computationally_independent_unit(unit)))
}

pub(super) fn computed_easing_with(raw:&str,resolve:&mut dyn FnMut(typed_numeric::NumericValue)->Option<typed_numeric::NumericValue>)->Option<String> {
    let normalized=easing(raw)?;
    let Some((count,position))=steps_parts(&normalized)else{return Some(normalized);};
    if !math_function(count){return Some(normalized);}
    let mut expression=typed_numeric::parse_numeric_expression(count)?;
    if expression.numeric_type()?!=typed_numeric::NumericType::from_unit(typed_numeric::NumericUnit::Number){return None;}
    expression.map_numeric_values(resolve)?;expression.simplify_absolute_units();
    if let Some(value)=expression.single_numeric_value(){return computed_steps_value(value.value,position);}
    let count=expression.serialize()?;
    Some(if matches!(position,"end"|"jump-end"){alloc::format!("steps({count})")}else{alloc::format!("steps({count}, {position})")})
}

pub(super) fn resolve_timings(raw:&str,length:LengthContext,query:ContainerUnitContext,index:usize,count:usize)->Option<Arc<[Arc<str>]>> {
    let parts=top_level_split(raw,b',',64)?;
    let mut result=Vec::new();result.try_reserve_exact(parts.len()).ok()?;
    for part in parts {result.push(Arc::from(resolve_timing(part.trim(),length,query,index,count)?));}
    Some(result.into())
}

pub(super) fn needs_computation(raw:&str)->bool {
    top_level_split(raw,b',',64).is_some_and(|parts|parts.into_iter().any(|part|
        steps_parts(part.trim()).is_some_and(|(count,_)|math_function(count)
            || sibling_context_colors(count,1,1).is_some_and(|(_,replaced)|replaced))))
}

pub(super) fn timing_dependencies(raw:&str)->(bool,bool,bool) {
    let mut font=false;let mut container=false;let mut sibling=false;
    if let Some(parts)=top_level_split(raw,b',',64) {for part in parts {
        if let Some((count,_))=steps_parts(part.trim()) {if let Some((expression,uses_sibling))=steps_expression(count,1,1) {
            font|=expression.contains_unit(font_relative_unit);
            container|=expression.contains_unit(container_unit);
            sibling|=uses_sibling;
        }}
    }}
    (font,container,sibling)
}

fn timing_values(raw: &str) -> Option<Arc<[Arc<str>]>> {
    let parts = top_level_split(raw,b',',64)?;
    let mut values = Vec::new(); values.try_reserve_exact(parts.len()).ok()?;
    for part in parts {values.push(Arc::from(easing(part)?));}
    (!values.is_empty()).then(||values.into())
}

fn behavior(raw: &str) -> Option<TransitionBehavior> {
    let mut position=0;
    let name=consume_selector_identifier(raw.trim(),&mut position)?;
    if position != raw.trim().len() {return None;}
    match name.to_ascii_lowercase().as_str() {
        "normal"=>Some(TransitionBehavior::Normal),"allow-discrete"=>Some(TransitionBehavior::AllowDiscrete),_=>None,
    }
}

fn behavior_values(raw:&str)->Option<Arc<[TransitionBehavior]>> {
    let parts=top_level_split(raw,b',',64)?;
    let mut values=Vec::new();values.try_reserve_exact(parts.len()).ok()?;
    for part in parts {values.push(behavior(part.trim())?);}
    (!values.is_empty()).then(||values.into())
}

// Reuse the component scanner for ordinary input; escaped identifiers use the
// canonical identifier consumer, which also consumes a hex escape terminator.
fn components(raw:&str)->Option<Vec<&str>> {
    if !raw.contains('\\') {return grid_components(raw);}
    if raw.len()>MAX_VARIABLE_BYTES {return None;}
    let mut result=Vec::new();let mut position=0;
    while position<raw.len() {
        while raw.as_bytes().get(position).is_some_and(u8::is_ascii_whitespace) {position+=1;}
        if position==raw.len(){break;}
        let start=position;
        if consume_selector_identifier(raw,&mut position).is_some() {
            if raw.as_bytes().get(position)==Some(&b'(') {position=selector_function_end(raw,position)?.checked_add(1)?;}
        } else {
            position=start;
            let part=*grid_components(&raw[start..])?.first()?;
            position+=part.len();
        }
        if result.len()>=64 || position==start {return None;}
        result.try_reserve(1).ok()?;result.push(&raw[start..position]);
    }
    Some(result)
}

fn append_time(values:&mut Vec<typed_numeric::NumericValue>,sources:&mut Option<Vec<String>>,raw:&str,nonnegative:bool)->Option<()>{
    let literal=typed_numeric::parse_numeric_value(raw.trim()).is_some();
    if !literal&&sources.is_none(){
        let mut pending=Vec::new();pending.try_reserve_exact(64).ok()?;
        for value in values.iter(){pending.push(typed_numeric::serialize_numeric_value(value.value,value.unit));}
        *sources=Some(pending);
    }
    if let Some(sources)=sources{sources.push(String::from(raw));}
    values.push(if literal{transition_time_value(raw,nonnegative)?}else{time_numeric_expression(raw,nonnegative)?;typed_numeric::NumericValue{value:0.0,unit:typed_numeric::NumericUnit::S}});
    Some(())
}
fn time_control(slot:usize,values:Vec<typed_numeric::NumericValue>,sources:Option<Vec<String>>)->Option<Value>{
    if let Some(sources)=sources{let raw=sources.join(", ");if raw.len()>MAX_VARIABLE_BYTES{return None;}Some(Value::TransitionTimesRaw(slot,raw))}else{Some(Value::TransitionTimes(slot,values.into()))}
}

pub(super) fn values(name:&str,raw:&str)->Option<Vec<Value>> {
    let stripped=strip_component_comments(raw,0,true).ok()?;let raw=stripped.trim();
    match name {
        "transition-property"=>return Some(vec![Value::TransitionProperty(property_values(raw)?)]),
        "transition-timing-function"=>return Some(vec![Value::TransitionTimingFunction(timing_values(raw)?)]),
        "transition-behavior"=>return Some(vec![Value::TransitionBehavior(behavior_values(raw)?)]),
        "transition"=>{},_=>return None,
    }
    let parts=top_level_split(raw,b',',64)?;
    let mut properties=Vec::new();let mut durations=Vec::new();let mut timings=Vec::new();let mut delays=Vec::new();let mut behaviors=Vec::new();let mut duration_sources=None;let mut delay_sources=None;
    for item in parts {
        let mut property_value=None;let mut duration=None;let mut delay=None;let mut timing=None;let mut behavior_value=None;
        let tokens=components(item)?;if tokens.is_empty(){return None;}
        for token in tokens {
            if transition_time_source_valid(token,false) {
                if duration.is_none() {if !transition_time_source_valid(token,true){return None;}duration=Some(token);}
                else if delay.is_none() {delay=Some(token);}else{return None;}
            } else if let Some(value)=timing.is_none().then(||easing(token)).flatten() {
                timing=Some(Arc::from(value));
            } else if let Some(value)=behavior_value.is_none().then(||behavior(token)).flatten() {
                behavior_value=Some(value);
            } else {
                if property_value.replace(property(token)?).is_some(){return None;}
            }
        }
        properties.push(property_value.unwrap_or_else(||Arc::from("all")));
        append_time(&mut durations,&mut duration_sources,duration.unwrap_or("0s"),true)?;
        append_time(&mut delays,&mut delay_sources,delay.unwrap_or("0s"),false)?;
        timings.push(timing.unwrap_or_else(||Arc::from("ease")));
        behaviors.push(behavior_value.unwrap_or_default());
    }
    if properties.len()>1 && properties.iter().any(|name|name.as_ref()=="none"){return None;}
    Some(vec![Value::TransitionProperty(properties.into()),time_control(175,durations,duration_sources)?,
        Value::TransitionTimingFunction(timings.into()),time_control(176,delays,delay_sources)?,Value::TransitionBehavior(behaviors.into())])
}

pub(super) fn serialize_properties(values:Option<&[Arc<str>]>) -> String {
    values.map_or_else(||"all".into(),|values|values.iter().map(|value|serialize_identifier(value)).collect::<Vec<_>>().join(", "))
}
pub(super) fn serialize_timings(values:Option<&[Arc<str>]>) -> String {
    values.map_or_else(||"ease".into(),|values|values.iter().map(|value|value.as_ref()).collect::<Vec<_>>().join(", "))
}
pub(super) fn serialize_behaviors(values:Option<&[TransitionBehavior]>) -> String {
    values.map_or_else(||"normal".into(),|values|values.iter().map(|value|value.as_str()).collect::<Vec<_>>().join(", "))
}

pub(super) fn shorthand(values:&[&str])->Option<String> {
    let columns=values.iter().map(|value|top_level_split(value,b',',64)).collect::<Option<Vec<_>>>()?;
    let count=columns.first()?.len();
    if columns.len()!=5 || count==0 || columns.iter().any(|column|column.len()!=count){return None;}
    let mut output=Vec::new();
    for index in 0..count {
        let [property,duration,timing,delay,behavior]:[&str;5]=core::array::from_fn(|column|columns[column][index].trim());
        let ambiguous_timing=easing(property).is_some();
        let ambiguous_behavior=super::transition_controls::behavior(property).is_some();
        let mut parts=Vec::new();
        if property!="all" && !ambiguous_timing && !ambiguous_behavior {parts.push(property);}
        if duration!="0s" || delay!="0s" {parts.push(duration);}
        if timing!="ease" || ambiguous_timing {parts.push(timing);}
        if delay!="0s" {parts.push(delay);}
        if behavior!="normal" || ambiguous_behavior {parts.push(behavior);}
        if ambiguous_timing || ambiguous_behavior {parts.push(property);}
        if parts.is_empty() {parts.push("all");}
        output.push(parts.join(" "));
    }
    Some(output.join(", "))
}

pub(super) fn longhands(raw:&str)->Vec<Arc<str>> {
    let Some(property)=property(raw) else{return Vec::new();};
    if property.as_ref()=="none" {return Vec::new();}
    if property.starts_with("--") {return vec![property];}
    if property.as_ref()=="all" {
        return PROPERTIES.iter().filter(|entry|entry.name!="all" && declaration_block::canonical_alias(entry.name)==entry.name && !declaration_block::is_shorthand(entry.name)
                && !entry.ids.iter().any(|slot|is_logical_property_slot(*slot))
                && transition_value_kind(entry.name)!=TransitionValueKind::NotAnimatable)
            .map(|entry|Arc::from(entry.name)).collect();
    }
    let property:Arc<str>=Arc::from(declaration_block::canonical_alias(&property));
    let longhands=declaration_block::longhands(&property);
    if !longhands.is_empty() {return longhands.iter().map(|name|Arc::from(*name)).collect();}
    if PROPERTIES.iter().any(|entry|entry.name==property.as_ref()) {vec![property]} else {Vec::new()}
}

#[cfg(test)]
mod tests {
    use super::*;
    fn computed(raw:&str,parent:Option<&Style>)->Style {
        let kind=NodeKind::Element{namespace:Namespace::Html,name:"div".into(),attributes:vec![("style".into(),raw.into())]};
        compute(&kind,parent,&StyleIndex::new(Vec::new())).unwrap()
    }

    #[test]
    fn specification_transition_controls_grammar_and_canonical_shorthand() {
        for (raw,expected) in [
            ("1s","1s"),("all 1s","1s"),("none","none"),
            ("1s -3s CUBIC-BEZIER(0,-2,1,3) top","top 1s cubic-bezier(0, -2, 1, 3) -3s"),
            ("allow-discrete display 3s ease-in-out 1s","display 3s ease-in-out 1s allow-discrete"),
            ("opacity 250ms steps(2, jump-end), --Case 1s -25ms","opacity 250ms steps(2), --Case 1s -25ms"),
            ("linear(0, .25, 1)","linear(0, 0.25, 1)"),
            (r"\77 idth 1s", "width 1s"),
        ] {
            assert!(supports_declaration("transition",raw),"{raw}");
            let mut block=DeclarationBlock::default();assert!(block.set("transition",raw,false).unwrap());
            assert_eq!(block.value("transition").unwrap().0,expected,"{raw}");
            assert_eq!(block.len(),5,"shorthand stores all five real longhands");
        }
        for raw in ["-1s","1s 2s 3s","none, top","none top","width 1px","1s cubic-bezier(2,0,0,1)","steps(1,jump-none)","linear(0)","initial top","default 1s"] {
            assert!(!supports_declaration("transition",raw),"{raw}");
        }
        assert_eq!(serialize_cssom_property_value("transition-timing-function","STEP-END, STEPS(4, END)").unwrap(),"steps(1), steps(4)");
    }

#[test]
fn specification_transition_logical_properties_share_physical_record_identity() {
    for (raw, pairs) in [
        ("writing-mode:horizontal-tb;direction:ltr", [("inline-size","width"),("block-size","height"),("margin-inline-start","margin-left"),("padding-block-end","padding-bottom"),("border-start-start-radius","border-top-left-radius")]),
        ("writing-mode:horizontal-tb;direction:rtl", [("inline-size","width"),("block-size","height"),("margin-inline-start","margin-right"),("padding-block-end","padding-bottom"),("border-start-start-radius","border-top-right-radius")]),
        ("writing-mode:vertical-rl;direction:ltr", [("inline-size","height"),("block-size","width"),("margin-inline-start","margin-top"),("padding-block-end","padding-left"),("border-start-start-radius","border-top-right-radius")]),
    ] {
        let style=computed(raw,None);
        for (logical,physical) in pairs {assert_eq!(transition_property_physical_name(&style,logical),physical,"{raw}: {logical}");}
        let all=transition_property_longhands("all");
        assert!(all.iter().all(|name|transition_property_physical_name(&style,name)==name.as_ref()));
        assert_eq!(all.iter().filter(|name|name.as_ref()=="width").count(),1);
        assert_eq!(all.iter().filter(|name|name.as_ref()=="height").count(),1);
        assert_eq!(transition_property_physical_name(&style,"--Case"),"--Case");
        assert_eq!(transition_property_physical_name(&style,"UnknownName"),"UnknownName");
    }
}

    #[test]
    fn specification_transition_controls_lists_unknowns_escapes_and_registry() {
        assert_eq!(easing("ease /**/"), Some("ease".into()));
        assert_eq!(easing(r"/**/\73 tep-end/**/"), Some("steps(1)".into()));
        assert_eq!(easing("cubic-bezier(0, /**/ 0, 1, 1)"), Some("cubic-bezier(0, 0, 1, 1)".into()));
        let style=computed(r"transition-property:opacity, UnknownName, --Case, \77 idth;transition-behavior:normal, ALLOW-DISCRETE",None);
        assert_eq!(style.transition_property().unwrap().iter().map(|value|value.as_ref()).collect::<Vec<_>>(),["opacity","UnknownName","--Case","width"]);
        assert_eq!(style.transition_behavior().unwrap(),[TransitionBehavior::Normal,TransitionBehavior::AllowDiscrete]);
        assert!(transition_property_longhands("UnknownName").is_empty());
        assert_eq!(transition_property_longhands("--Case").iter().map(|name|name.as_ref()).collect::<Vec<_>>(),["--Case"]);
        assert_eq!(transition_property_longhands("margin").iter().map(|name|name.as_ref()).collect::<Vec<_>>(),declaration_block::longhands("margin"));
        let all=transition_property_longhands("all");
        assert!(all.iter().any(|name|name.as_ref()=="opacity"));
        assert!(all.iter().all(|name|transition_value_kind(name)!=TransitionValueKind::NotAnimatable && !declaration_block::is_shorthand(name)));
        for raw in ["none, opacity","initial, width","default","opacity,,width","'opacity'","1"] {assert!(!supports_declaration("transition-property",raw),"{raw}");}
        let list=core::iter::repeat_n("opacity",64).collect::<Vec<_>>().join(",");
        assert!(supports_declaration("transition-property",&list));
        assert!(!supports_declaration("transition-property",&alloc::format!("{list}, opacity")));
        for name in ["transition-property","transition-timing-function","transition-behavior","transition"] {assert!(cssom_property_names().contains(&name));}
    }

    #[test]
    fn specification_transition_controls_cascade_variables_wide_and_lossless_lists() {
        let context=computed_values::ComputedValueContext::default();
        let parent=computed("transition:opacity 250ms ease-in 20ms allow-discrete",None);
        let child=computed("transition:inherit",Some(&parent));
        assert_eq!(child.computed_css_value("transition",context).unwrap(),"opacity 0.25s ease-in 0.02s allow-discrete");
        for reset in ["initial","unset","revert","all:initial"] {
            let source=if reset=="all:initial" {reset.into()} else {alloc::format!("transition:{reset}")};
            assert_eq!(computed(&source,Some(&parent)).computed_css_value("transition",context).unwrap(),"all");
        }
        let substituted=computed("--settings:display 1s allow-discrete;transition:var(--settings)",None);
        assert_eq!(substituted.computed_css_value("transition",context).unwrap(),"display 1s allow-discrete");
        let invalid=computed("--settings:none,opacity;transition:var(--settings)",None);
        assert_eq!(invalid.computed_css_value("transition",context).unwrap(),"all");
        let mut block=DeclarationBlock::default();
        block.set("transition","opacity 1s, width 2s",true).unwrap();
        block.set("transition-delay","20ms",true).unwrap();
        assert!(block.value("transition").is_none(),"unequal lists cannot synthesize a shorthand that would lose computed list lengths");
        block.set("transition","initial",true).unwrap();
        for name in declaration_block::longhands("transition") {assert_eq!(block.value(name).unwrap(),("initial".into(),true));}
    }

    #[test]
    fn specification_transition_contextual_steps_use_actual_siblings_and_font() {
        fn resolve(document:&Document,node:NodeId,index:&StyleIndex,cache:&mut StyleCache)->Style {
            let mut ancestors=Vec::new();let mut current=Some(node);
            while let Some(node)=current {if matches!(document.kind(node),Ok(NodeKind::Element{..})){ancestors.push(node);}current=document.parent(node).unwrap();}
            let mut parent=None;
            for node in ancestors.into_iter().rev(){parent=Some(compute_node_cached(document,node,parent.as_ref(),index,cache).unwrap());}
            parent.unwrap()
        }
        for property in ["transition-timing-function","animation-timing-function"] {
            let mut document=crate::html::parse("<main id=p style='font-size:20px'><span id=a></span>text<!--comment--><span id=b></span><span id=c></span></main>",64).unwrap();
            let nodes=["#a","#b","#c"].map(|selector|crate::selector::query_selector(&document,document.root(),selector).unwrap().unwrap());
            let index=StyleIndex::new(parse(&alloc::format!("span{{{property}:steps(sibling-index(),jump-none),steps(calc(sibling-count() * 2)),steps(calc(4 + sign(1em - 30px)))}} ")).unwrap());
            let mut cache=StyleCache::default();let context=computed_values::ComputedValueContext::default();
            for (position,node) in nodes.into_iter().enumerate(){
                let style=resolve(&document,node,&index,&mut cache);
                assert_eq!(style.computed_css_value(property,context).unwrap(),alloc::format!("steps({}, jump-none), steps(6), steps(3)",(position+1).max(2)));
                assert!(style.sibling_position_dependent);
            }
            let parent=crate::selector::query_selector(&document,document.root(),"#p").unwrap().unwrap();
            document.set_attribute(parent,"style","font-size:40px").unwrap();cache.invalidate_node(parent);
            assert_eq!(resolve(&document,nodes[2],&index,&mut cache).computed_css_value(property,context).unwrap(),"steps(3, jump-none), steps(6), steps(5)");
            document.insert_before(parent,nodes[2],Some(nodes[0])).unwrap();
            assert_eq!(resolve(&document,nodes[2],&index,&mut cache).computed_css_value(property,context).unwrap(),"steps(2, jump-none), steps(6), steps(5)","cache cannot conceal changed sibling coordinates");
        }
    }

    #[test]
    fn specification_transition_contextual_steps_round_clamp_and_query_resolution() {
        let context=computed_values::ComputedValueContext::default();
        for property in ["transition-timing-function","animation-timing-function"] {
            let mut block=declaration_block::DeclarationBlock::default();
            block.set(property,"StEpS(sibling-index(), JuMp-NoNe)",false).unwrap();
            assert_eq!(block.value(property).unwrap().0,"steps(sibling-index(), jump-none)");
            let style=computed(&alloc::format!("{property}:steps(calc(2.5)),steps(calc(-3)),steps(calc(-3),jump-none)"),None);
            assert_eq!(style.computed_css_value(property,context).unwrap(),"steps(3), steps(1), steps(2, jump-none)");
            for raw in ["steps(0)","steps(-2)","steps(1,jump-none)","steps(2.0)","steps(calc(2%))","steps(sibling-index(2))"] {assert!(!supports_declaration(property,raw),"{property}:{raw}");}
            let mut style=computed(&alloc::format!("font-size:1cqw;{property}:steps(calc(4 + sign(1em - 3px)))"),None);
            assert!(style.has_query_container_dependencies());
            assert!(!style.resolve_query_context(ContainerUnitContext::default()));
            let query=ContainerUnitContext{width:ContainerUnitBasis::Size(200.0),..ContainerUnitContext::default()};
            assert!(style.resolve_query_context(query));
            assert_eq!(style.computed_css_value(property,context).unwrap(),"steps(3)","actual current font is resolved before the number-valued sign argument");
            let mut style=computed(&alloc::format!("{property}:steps(calc(4 + sign(1cqw - 3px)))"),None);
            assert!(style.resolve_query_context(ContainerUnitContext{width:ContainerUnitBasis::Size(400.0),..ContainerUnitContext::default()}));
            assert_eq!(style.computed_css_value(property,context).unwrap(),"steps(5)");
        }
    }
}
