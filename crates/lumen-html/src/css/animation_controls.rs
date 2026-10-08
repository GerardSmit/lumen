use super::*;

pub(super) const STYLE_COUNT: usize = 19;
pub(super) fn index(slot: usize) -> usize {
    match slot { 163..=171 => slot-163, 206..=211 => slot-197, 222..=225 => slot-207,
        _ => unreachable!("animation control slot") }
}

pub struct AnonymousProgressTimeline {
    pub view:bool,
    pub axis:String,
    pub scroller:String,
    pub insets:[DecorationLength;2],
}

fn decoded_keyword(raw:&str)->Option<String>{
    let mut position=0;let name=consume_selector_identifier(raw,&mut position)?;
    (position==raw.len()).then(||name.to_ascii_lowercase())
}

pub(super) fn inset_pair(raw:&str,context:Option<(LengthContext,ContainerUnitContext)>)->Option<String>{
    let tokens=grid_components(raw)?;
    if tokens.is_empty()||tokens.len()>2{return None;}
    let mut values=Vec::new();
    for token in tokens {
        if decoded_keyword(token).as_deref()==Some("auto"){values.push(String::from("auto"));continue;}
        let value=endpoint(&[token],true)?;
        if value=="normal"||range_name(&value).is_some(){return None;}
        values.push(match context{Some((length,query))=>decoration_length_with_context(&value,length,query)?.computed(),None=>value});
    }
    Some(if values.len()==1||values[0]==values[1]{values.remove(0)}else{values.join(" ")})
}

pub(super) fn timeline(raw:&str,context:Option<(LengthContext,ContainerUnitContext)>)->Option<String>{
    let mut position=0;let name=consume_selector_identifier(raw,&mut position)?;
    if position==raw.len(){
        let lower=name.to_ascii_lowercase();
        return if matches!(lower.as_str(),"auto"|"none"){Some(lower)}else if name.starts_with("--")&&name.len()>2{Some(serialize_identifier(&name))}else{None};
    }
    let function=name.to_ascii_lowercase();
    if !matches!(function.as_str(),"scroll"|"view"){return None;}
    let body=raw.get(position..)?.strip_prefix('(')?.strip_suffix(')')?;
    let mut axis=None;let mut scroller=None;let mut insets=Vec::new();let mut inset_closed=false;
    for token in grid_components(body)?{
        match decoded_keyword(token).as_deref(){
            Some("block"|"inline"|"x"|"y")=>{if axis.is_some(){return None;}inset_closed=!insets.is_empty();axis=decoded_keyword(token);},
            Some("root"|"nearest"|"self") if function=="scroll"=>{if scroller.is_some(){return None;}scroller=decoded_keyword(token);},
            _ if function=="view"=>{if inset_closed||insets.len()==2{return None;}insets.push(token);},
            _=>return None,
        }
    }
    let mut parts=Vec::new();
    if let Some(scroller)=scroller.filter(|value|value!="nearest"){parts.push(scroller);}
    if let Some(axis)=axis.filter(|value|value!="block"){parts.push(axis);}
    if function=="view"&&!insets.is_empty(){
        let inset=inset_pair(&insets.join(" "),context)?;
        if inset!="auto"{parts.push(inset);}
    }
    Some(alloc::format!("{function}({})",parts.join(" ")))
}

pub(super) fn timeline_list(raw:&str,context:Option<(LengthContext,ContainerUnitContext)>)->Option<String>{
    Some(top_level_split(raw,b',',64)?.into_iter().map(|raw|timeline(raw.trim(),context)).collect::<Option<Vec<_>>>()?.join(", "))
}

pub fn parse_anonymous_progress_timeline(raw:&str)->Option<AnonymousProgressTimeline>{
    let raw=timeline(raw,None)?;
    let (view,body)=if let Some(body)=raw.strip_prefix("scroll("){(false,body.strip_suffix(')')?)}else{(true,raw.strip_prefix("view(")?.strip_suffix(')')?)};
    let mut axis=String::from("block");let mut scroller=String::from("nearest");let mut offsets=Vec::new();
    for token in grid_components(body)?{
        match token {
            "block"|"inline"|"x"|"y"=>axis=String::from(token),
            "root"|"self"|"nearest"=>scroller=String::from(token),
            _=>offsets.push(token),
        }
    }
    let mut insets=[DecorationLength::Auto,DecorationLength::Auto];
    for (index,raw)in offsets.into_iter().enumerate(){insets[index]=if raw=="auto"{DecorationLength::Auto}else{decoration_length_with_context(raw,static_length_context(),ContainerUnitContext::default())?};if index==0{insets[1]=insets[0].clone();}}
    Some(AnonymousProgressTimeline{view,axis,scroller,insets})
}

pub(super) fn animation_name(raw:&str)->Option<String>{
    if matches!(raw.as_bytes().first(),Some(b'\''|b'"')){
        return css_string(raw).filter(|value| !value.is_empty()).map(|value| {
            if matches!(value.to_ascii_lowercase().as_str(),"none"|"initial"|"inherit"|"unset"|"revert"|"revert-layer"|"default"){serialize_string(&value)}else{serialize_identifier(&value)}
        });
    }

    let mut position=0;let value=consume_selector_identifier(raw,&mut position)?;
    if position!=raw.len()||matches!(value.to_ascii_lowercase().as_str(),"initial"|"inherit"|"unset"|"revert"|"revert-layer"|"default"){return None;}
    Some(if value.eq_ignore_ascii_case("none"){String::from("none")}else{serialize_identifier(&value)})
}

fn numeric_control(raw:&str,number:bool,nonnegative:bool,context:Option<(LengthContext,ContainerUnitContext)>)->Option<String>{
    let (validation,_) = sibling_context_colors(raw,1,1)?;
    if !number {
        if context.is_some(){let value=computed_time_value(&validation,nonnegative,context)?;return Some(typed_numeric::serialize_numeric_value(value.value,value.unit));}
        return if validation==raw{if typed_numeric::parse_numeric_value(raw.trim()).is_some(){
            let value=computed_time_value(raw,nonnegative,None)?;Some(typed_numeric::serialize_numeric_value(value.value,value.unit))
        }else{time_numeric_expression(raw,nonnegative)?.serialize_specified()}}else{time_numeric_expression(&validation,nonnegative)?;Some(String::from(raw))};
    }
    let expression=typed_numeric::parse_numeric_expression(&validation)?;
    let unit=if number{typed_numeric::NumericUnit::Number}else{typed_numeric::NumericUnit::S};
    if expression.numeric_type()?!=typed_numeric::NumericType::from_unit(unit){return None;}
    if let typed_numeric::NumericExpression::Value(value)=&expression{
        if nonnegative&&value.value<0.0{return None;}
        if context.is_none(){return Some(typed_numeric::serialize_numeric_value(value.value,value.unit));}
    }
    match context{
        None=>if validation==raw{expression.serialize()}else{Some(String::from(raw))},
        Some((length,query))=>{
            let value=expression.evaluate(&mut FontAngleContext{length:Some(length),query,percent_scale:1.0})?;
            let value=if value.is_finite(){value}else{f64::from(typed_numeric::computed_f32(value))};
            Some(typed_numeric::serialize_numeric_value(if nonnegative{value.max(0.0)}else{value},unit))
        }
    }
}
pub(super) fn contains_unit(raw:&str,predicate:fn(typed_numeric::NumericUnit)->bool)->bool{
    top_level_split(raw,b',',64).is_some_and(|items|items.into_iter().any(|item|{
        let item=item.trim();
        let contents=if item.starts_with("view(")||item.starts_with("scroll("){item.split_once('(').and_then(|(_,body)|body.strip_suffix(')')).unwrap_or(item)}else{item};
        grid_components(contents).is_some_and(|tokens|tokens.into_iter().any(|token|typed_numeric::parse_numeric_expression(token).is_some_and(|expression|expression.contains_unit(predicate))))
    }))
}

pub(super) fn numeric_list(raw:&str,slot:usize,context:Option<(LengthContext,ContainerUnitContext)>)->Option<String>{
    Some(top_level_split(raw,b',',64)?.into_iter().map(|part|{
        let part=part.trim();
        if slot==164&&decoded_keyword(part).as_deref()==Some("auto"){return Some(String::from("auto"));}
        if slot==167&&decoded_keyword(part).as_deref()==Some("infinite"){return Some(String::from("infinite"));}
        numeric_control(part,slot==167,matches!(slot,164|167|224),context)
    }).collect::<Option<Vec<_>>>()?.join(", "))
}

pub(super) fn parse_shorthand(raw:&str)->Option<Vec<(usize,String)>>{
    let mut columns:[Vec<String>;9]=core::array::from_fn(|_|Vec::new());
    for item in top_level_split(raw,b',',64)?{
        let mut values=["none","auto","0s","ease","1","normal","none","running","auto"].map(String::from);
        let mut seen=[false;9];let mut times=0;let mut dashed=[None;2];let mut dashed_count=0;
        for token in grid_components(item)?{
            let keyword=decoded_keyword(token);
            // Defer ambiguous dashed names: an unambiguous keyframes name
            // elsewhere in this unordered shorthand can claim its own slot.
            if keyword.as_deref().is_some_and(|value|value.starts_with("--"))&&timeline(token,None).is_some(){
                animation_name(token)?;
                if dashed_count==dashed.len(){return None;}
                dashed[dashed_count]=Some(token);dashed_count+=1;continue;
            }
            let slot=if keyword.as_deref()==Some("auto")&&!seen[1]{1}
                else if numeric_control(token,false,false,None).is_some(){
                    if times>=2{return None;}let slot=if times==0{1}else{2};
                    let value=numeric_control(token,false,slot==1,None)?;
                    if seen[slot]{return None;}values[slot]=value;seen[slot]=true;times+=1;continue;
                }else if let Some(easing)=transition_controls::easing(token).filter(|_|!seen[3]){
                    if seen[3]{return None;}values[3]=easing;seen[3]=true;continue;
                }else if !seen[4]&&(keyword.as_deref()==Some("infinite")||numeric_control(token,true,true,None).is_some()){4}
                else if keyword.as_deref().is_some_and(|value|matches!(value,"normal"|"reverse"|"alternate"|"alternate-reverse"))&&!seen[5]{5}
                else if keyword.as_deref().is_some_and(|value|matches!(value,"none"|"forwards"|"backwards"|"both"))&&!seen[6]{6}
                else if keyword.as_deref().is_some_and(|value|matches!(value,"running"|"paused"))&&!seen[7]{7}
                else if !seen[0]&&animation_name(token).is_some(){0}
                else if !seen[8]&&timeline(token,None).is_some(){8}

                else{return None;};
            if seen[slot]{return None;}
            values[slot]=match slot{0=>animation_name(token)?,8=>timeline(token,None)?,4 if keyword.as_deref()!=Some("infinite")=>numeric_control(token,true,true,None)?,_=>keyword?};
            if slot==1{times=1;}seen[slot]=true;
        }
        for token in dashed.into_iter().flatten(){
            let slot=if !seen[0]{0}else if !seen[8]{8}else{return None;};
            values[slot]=if slot==0{animation_name(token)?}else{timeline(token,None)?};seen[slot]=true;
        }
        for(index,value)in values.into_iter().enumerate(){columns[index].push(value);}
    }
    let mut result=columns.into_iter().enumerate().map(|(index,values)|(if index==8{206}else{163+index},values.join(", "))).collect::<Vec<_>>();
    result.extend([(171,String::from("replace")),(222,String::from("normal")),(223,String::from("normal")),(224,String::from("0s"))]);
    Some(result)
}

fn range_name(raw: &str) -> Option<&'static str> {
    ["cover","contain","entry","exit","entry-crossing","exit-crossing"]
        .into_iter().find(|name|raw.eq_ignore_ascii_case(name))
}

fn endpoint(tokens: &[&str], start: bool) -> Option<String> {
    let (name, offset) = match tokens {
        [value] if value.eq_ignore_ascii_case("normal") => return Some("normal".into()),
        [value] => match range_name(value) {
            Some(name) => return Some(name.into()), None => (None,*value),
        },
        [name,offset] => (Some(range_name(name)?),*offset),
        _ => return None,
    };
    contextual_length(offset,Some(LengthContext {percent:Some(100.0),..static_length_context()}))?;
    let offset=if offset=="0"{String::from("0px")}else{
        let mut expression=typed_numeric::parse_numeric_expression(offset)?;
        expression.simplify_absolute_units();
        expression.serialize()?
    };

    Some(match name {
        Some(name) if offset==if start {"0%"}else{"100%"} => name.into(),
        Some(name) => alloc::format!("{name} {offset}"),
        None => offset,
    })
}

pub(super) fn endpoints(raw: &str, start: bool) -> Option<String> {
    let mut result=Vec::new();
    for value in top_level_split(raw,b',',64)? {
        result.push(endpoint(&grid_components(value)?,start)?);
    }
    Some(result.join(", "))
}

pub(super) fn range_values(raw: &str) -> Option<Vec<(usize,String)>> {
    let mut starts=Vec::new();let mut ends=Vec::new();
    for item in top_level_split(raw,b',',64)? {
        let tokens=grid_components(item)?;
        let mut split=None;
        // A named range consumes its optional offset before the second endpoint.
        for count in (1..=tokens.len().min(2)).rev() {
            let Some(start)=endpoint(&tokens[..count],true) else{continue;};
            let end=if count==tokens.len() {
                if let Some(name)=range_name(tokens[0]) {String::from(name)}else{String::from("normal")}
            } else {let Some(end)=endpoint(&tokens[count..],false) else{continue;};end};
            split=Some((start,end));break;
        }
        let(start,end)=split?;starts.push(start);ends.push(end);
    }
    Some(vec![(222,starts.join(", ")),(223,ends.join(", "))])
}

pub(super) fn range_shorthand(values: &[&str]) -> Option<String> {
    let &[start,end]=values else{return None;};
    let starts=top_level_split(start,b',',64)?;let ends=top_level_split(end,b',',64)?;
    if starts.len()!=ends.len(){return None;}
    let mut result=Vec::new();
    for(start,end)in starts.into_iter().zip(ends) {
        let tokens=grid_components(start)?;
        let implicit=if tokens.first().is_some_and(|name|range_name(name).is_some()) {tokens[0]}else{"normal"};
        result.push(if end==implicit {String::from(start)}else{alloc::format!("{start} {end}")});
    }
    Some(result.join(", "))
}

pub(super) fn compute_endpoints(raw: &str,start: bool,context:LengthContext,query:ContainerUnitContext)->Option<String> {
    let mut result=Vec::new();
    for value in top_level_split(raw,b',',64)? {
        let tokens=grid_components(value)?;
        let (name,offset)=match tokens.as_slice() {
            [value] if *value=="normal"=>{result.push(String::from("normal"));continue;},
            [value] if range_name(value).is_some()=>{result.push(String::from(*value));continue;},
            [offset]=>(None,*offset),[name,offset]=>(Some(range_name(name)?),*offset),_=>return None,
        };
        let offset=if offset=="0"{String::from("0px")}else{computed_percentage_expression(offset,context,query)?.to_string()};

        result.push(match name {
            Some(name) if offset==if start {"0%"}else{"100%"}=>String::from(name),
            Some(name)=>alloc::format!("{name} {offset}"),None=>offset,
        });
    }
    Some(result.join(", "))
}

pub(super) fn project_range(style:&mut Style) {
    if style.animation[15].is_none()&&style.animation[16].is_none(){return;}
    let starts=top_level_split(style.animation[15].as_deref().unwrap_or("normal"),b',',64).unwrap();
    let ends=top_level_split(style.animation[16].as_deref().unwrap_or("normal"),b',',64).unwrap();
    let mut values=Vec::new();
    for index in 0..starts.len().max(ends.len()) {
        values.push(range_shorthand(&[starts[index%starts.len()],ends[index%ends.len()]]).unwrap());
    }
    style.animation[10]=Some(Arc::from(values.join(", ")));
}

pub(super) fn delay_values(raw:&str)->Option<Vec<(usize,String)>> {
    let mut starts=Vec::new();let mut ends=Vec::new();
    for item in top_level_split(raw,b',',64)? {
        let tokens=grid_components(item)?;
        let [start,end @ ..]=tokens.as_slice() else{return None;};
        if end.len()>1{return None;}
        let serialize=|raw,nonnegative|numeric_control(raw,false,nonnegative,None);
        starts.push(serialize(start,false)?);ends.push(serialize(end.first().copied().unwrap_or("0s"),true)?);

    }
    Some(vec![(165,starts.join(", ")),(224,ends.join(", "))])
}

pub(super) fn delay_shorthand(values:&[&str])->Option<String> {
    let &[start,end]=values else{return None;};
    let starts=top_level_split(start,b',',64)?;let ends=top_level_split(end,b',',64)?;
    if starts.len()!=ends.len(){return None;}
    Some(starts.into_iter().zip(ends).map(|(start,end)|if css_time_ms(end)==Some(0.0){String::from(start)}else{alloc::format!("{start} {end}")}).collect::<Vec<_>>().join(", "))
}

// Reset-only properties must be initial to serialize a shorthand which cannot
// express their noninitial values. List lengths of expressed values must agree.
pub(super) fn shorthand(values:&[&str])->Option<String> {
    shorthand_with_resolution(values,false)
}

// Remove a component only when the canonical grammar reproduces every
// expressed value. This also retains default tokens which disambiguate names
// such as `ease`, `auto`, or `reverse`, without a second shorthand grammar.
pub(super) fn shorthand_with_resolution(values:&[&str],resolved_duration:bool)->Option<String> {
    let &[name,duration,delay,timing,iterations,direction,fill,play,composition,timeline,range_start,range_end,delay_end]=values else{return None;};
    if composition!="replace"||range_start!="normal"||range_end!="normal"||delay_end!="0s"{return None;}
    let mut lists=Vec::new();
    for value in [duration,timing,delay,iterations,direction,fill,play,name,timeline]{lists.push(top_level_split(value,b',',64)?);}
    let count=lists[0].len();
    if lists[..8].iter().any(|list|list.len()!=count)||!(timeline=="auto"||lists[8].len()==count){return None;}
    let slots=[164,166,165,167,168,169,170,163,206];
    let mut result=Vec::new();
    for index in 0..count {
        let expected=core::array::from_fn::<_,9,_>(|column|lists[column][if column==8&&timeline=="auto"{0}else{index}]);
        let mut included=[true;9];
        let serialize=|included:&[bool;9]| {
            let mut parts=Vec::new();
            for column in 0..7 {if included[column]{parts.push(expected[column]);}}
            if included[8]{parts.push(expected[8]);}
            if included[7]{parts.push(expected[7]);}


            if parts.is_empty(){String::from("none")}else{parts.join(" ")}
        };
        let equivalent=|candidate:&str| {
            let Some(parsed)=parse_shorthand(candidate) else{return false;};
            slots.iter().zip(expected).all(|(slot,expected)| {
                parsed.iter().find(|(target,_)|target==slot).is_some_and(|(_,actual)| {
                    actual==expected||(resolved_duration&&*slot==164&&actual=="auto"&&expected=="0s")
                })
            })
        };
        // At most nine bounded parser passes per item; no matching-rule pool or
        // permanent serialization cache is retained.
        let defaults=["auto","ease","0s","1","normal","none","running","none","auto"];
        for column in [7,8,6,5,4,3,2,1,0] {
            if expected[column]!=defaults[column]&&!(column==0&&resolved_duration&&expected[column]=="0s"){continue;}
            included[column]=false;

            if !equivalent(&serialize(&included)){included[column]=true;}
        }
        let item=serialize(&included);
        if !equivalent(&item){return None;}
        result.push(item);
    }
    Some(result.join(", "))
}

pub(crate) fn parsed_progress_endpoint(raw:&str,start:bool)->Option<(crate::animation::ProgressRangeName,decoration_lengths::DecorationLength)> {
    use crate::animation::ProgressRangeName as N;
    let tokens=grid_components(raw)?;
    let(name,offset)=match tokens.as_slice(){
        ["normal"]=>return Some((N::Cover,decoration_lengths::DecorationLength::Length(TransformLength {pixels:0.0,percent:if start{0.0}else{100.0}}))),
        [name] if range_name(name).is_some()=>(*name,if start{"0%"}else{"100%"}),
        [offset]=>("cover",*offset),[name,offset]=>(*name,*offset),_=>return None,
    };
    let name=match name{"cover"=>N::Cover,"contain"=>N::Contain,"entry"=>N::Entry,"exit"=>N::Exit,"entry-crossing"=>N::EntryCrossing,"exit-crossing"=>N::ExitCrossing,_=>return None};
    let length=decoration_lengths::decoration_length_with_context(offset,static_length_context(),ContainerUnitContext::default())?;
    Some((name,length))
}

pub(crate) fn progress_range_parts(raw:&str)->Option<[(crate::animation::ProgressRangeName,decoration_lengths::DecorationLength);2]> {
    let values=range_values(raw)?;
    if top_level_split(&values[0].1,b',',64)?.len()!=1{return None;}
    Some([parsed_progress_endpoint(&values[0].1,true)?,parsed_progress_endpoint(&values[1].1,false)?])
}

#[cfg(test)]
mod tests{
    use super::*;
    #[test]
    fn specification_animation_time_lists_share_complete_math_and_owner_context(){
        let context=LengthContext{font:32.0,..static_length_context()};let query=ContainerUnitContext::default();
        assert_eq!(numeric_list("calc(progress(1,0,1) * 1s), 250ms",164,None).as_deref(),Some("calc(1s), 250ms"));
        assert_eq!(numeric_list("calc(1s * (1em / 16px)), -1s",165,Some((context,query))).as_deref(),Some("2s, -1s"));
        assert_eq!(numeric_list("calc(-1s)",164,Some((context,query))).as_deref(),Some("0s"));
        assert_eq!(numeric_list("calc(0 / 0 * 1s)",165,Some((context,query))).as_deref(),Some("0s"));
        assert!(numeric_list("calc(progress(5%,0deg,8deg) * 1s)",165,None).is_none());
    }

    #[test]
    fn specification_animation_shorthand_ambiguous_dashed_name_and_timeline(){
        let check=|raw:&str,name:&str,timeline:&str|{let values=parse_shorthand(raw).unwrap();assert_eq!(values.iter().find(|(slot,_)|*slot==163).map(|(_,v)|v.as_str()),Some(name));assert_eq!(values.iter().find(|(slot,_)|*slot==206).map(|(_,v)|v.as_str()),Some(timeline));};
        check("--anim 1000s step-end","--anim","auto");check("1000s step-end --anim","--anim","auto");
        check("--timeline spin 1s","spin","--timeline");check("spin 1s --timeline","spin","--timeline");
        check("--anim --timeline 1s","--anim","--timeline");
        let values=parse_shorthand("spin 1s scroll()").unwrap();assert!(values.iter().any(|(slot,v)|*slot==206&&v.starts_with("scroll(")));
        assert!(parse_shorthand("--a --b --c 1s").is_none());
    }

    #[test]
    fn specification_animation_controls_canonical_names_shorthand_and_range_units() {
        for (raw,expected) in [("NONE","none"),("\"something\"","something"),("\"multi word\"",r"multi\ word"),("\"NoNe\"","\"NoNe\"")] {
            assert_eq!(animation_name(raw).as_deref(),Some(expected));
        }
        for raw in ["\"\"", "''", "\"\\\n\""] {
            assert!(animation_name(raw).is_none(), "empty decoded name {raw:?}");
            assert!(super::super::normalize_animation_name(raw).is_none());
            assert!(parse_shorthand(&alloc::format!("1s {raw}")).is_none());
            assert!(super::super::parse_keyframes_rule(&alloc::format!("@keyframes {raw} {{ from {{ opacity: 0 }} }}")).is_err());
        }
        for raw in ["\"none\"", "\"initial\"", "\"inherit\"", "\"unset\"", "\"revert\"", "\"revert-layer\"", "\"default\""] {
            assert!(animation_name(raw).is_some());
            assert_eq!(super::super::parse_stylesheet(&alloc::format!("@keyframes {raw} {{ from {{ opacity: 0 }} }}")).unwrap().keyframes.len(), 1);
        }
        let recovered=super::super::parse_stylesheet("@keyframes \"\"{to{opacity:0}} @keyframes \"none\"{to{opacity:1}} #target{animation-name:retained;animation-name:''}").unwrap();
        assert_eq!(recovered.keyframes.len(),1);
        assert_eq!(recovered.keyframes[0].name,"none");
        let base=["none","auto","0s","ease","1","normal","none","running","replace","auto","normal","normal","0s"];
        assert_eq!(shorthand(&base).as_deref(),Some("none"));
        for (index,value,expected) in [(1,"1s","1s"),(2,"-3s","auto -3s"),(3,"ease-in","ease-in"),(4,"4","4"),(5,"reverse","reverse"),(6,"both","both"),(7,"paused","paused"),(0,"spin","spin")] {
            let mut values=base;values[index]=value;
            assert_eq!(shorthand(&values).as_deref(),Some(expected));
        }
        for name in ["ease","auto","reverse","none","\"none\"","--named"] {
            let mut values=base;values[0]=name;values[9]="--timeline";
            let serialized=shorthand(&values).unwrap();
            let reparsed=parse_shorthand(&serialized).unwrap();
            assert_eq!(reparsed.iter().find(|(slot,_)|*slot==163).unwrap().1,name);
            assert_eq!(reparsed.iter().find(|(slot,_)|*slot==206).unwrap().1,"--timeline");
        }
        assert_eq!(endpoints("cover 0%, 0, 120%, calc(20% - 20%)",true).as_deref(),Some("cover, 0px, 120%, calc(0%)"));
        assert_eq!(endpoints("cover 100%",false).as_deref(),Some("cover"));
        assert!(numeric_list("-1s",224,None).is_none());
        assert!(delay_values("1s -2s").is_none());
        assert_eq!(numeric_list("calc(-1s)",224,Some((static_length_context(),ContainerUnitContext::default()))).as_deref(),Some("0s"));

        let context=LengthContext{font:24.0,..static_length_context()};
        assert_eq!(compute_endpoints("0%, 120%, calc(1em + 10%), calc(20% - 20%)",true,context,ContainerUnitContext::default()).as_deref(),Some("0%, 120%, calc(10% + 24px), 0%"));
        // Bare percentages do not carry a named-range implicit end.
        assert_eq!(range_shorthand(&["0%","100%"]).as_deref(),Some("0% 100%"));
    }

    #[test]
    fn specification_animation_controls_contextual_numbers_ranges_and_timeline_grammar(){
        let context=LengthContext{font:24.0,..static_length_context()};
        let query=ContainerUnitContext{width:ContainerUnitBasis::Size(200.0),..ContainerUnitContext::default()};
        assert_eq!(numeric_list("calc(2 * 3s)",164,Some((context,query))).as_deref(),Some("6s"));
        assert_eq!(numeric_list("calc(10s + sign(2cqw - 10px) * 5s)",164,Some((context,query))).as_deref(),Some("5s"));
        assert_eq!(numeric_list("calc(10 + sign(1em - 20px) * 5)",167,Some((context,query))).as_deref(),Some("15"));
        assert_eq!(numeric_list("calc(-2s)",164,Some((context,query))).as_deref(),Some("0s"));
        assert!(numeric_list("-2s",164,None).is_none());
        assert!(numeric_list("calc(1px)",164,None).is_none());
        assert_eq!(compute_endpoints("entry calc(1em + 10%)",true,context,query).as_deref(),Some("entry calc(10% + 24px)"));
        assert_eq!(compute_endpoints("calc(70% + 10% * sign(100em - 1px))",false,context,query).as_deref(),Some("80%"));
        assert_eq!(compute_endpoints("calc(70% + 10% * sign(2cqw - 10px))",false,context,query).as_deref(),Some("60%"));
        let wider=ContainerUnitContext{width:ContainerUnitBasis::Size(1000.0),..query};
        assert_eq!(compute_endpoints("calc(70% + 10% * sign(2cqw - 10px))",false,context,wider).as_deref(),Some("80%"));
        assert_eq!(compute_endpoints("calc(100% - 100% + 1em)",false,context,query).as_deref(),Some("calc(0% + 24px)"));
        assert_eq!(compute_endpoints("min(10%, 20%)",false,context,query).as_deref(),Some("min(10%, 20%)"));
        assert_eq!(timeline("VIEW(inline 1em 10%)",Some((context,query))).as_deref(),Some("view(inline 24px 10%)"));
        for invalid in ["scroll(x y)","scroll(self root)","view(10px inline 20px)","view(10px 20px 30px)"]{assert!(timeline(invalid,None).is_none(),"{invalid}");}
        let range=crate::animation::ProgressRange::parse("20px calc(100% - 10px)").unwrap();
        assert_eq!(range.scroll_bounds(0.0,200.0),Some((20.0,190.0)));
        assert_eq!(range.view_bounds(300.0,100.0,200.0),Some((120.0,390.0)));
        let values=parse_shorthand("fade auto linear view(inline 10px 20px), 2s ease-in 1s 3 reverse both paused spin scroll(self x)").unwrap();
        assert_eq!(values.iter().find(|(slot,_)|*slot==206).unwrap().1,"view(inline 10px 20px), scroll(self x)");
        assert_eq!(values.iter().find(|(slot,_)|*slot==222).unwrap().1,"normal");
    }
}

pub(super) fn timeline_shorthand(values:&[&str])->Option<String>{
    let names=top_level_split(values.first()?,b',',64)?;let axes=top_level_split(values.get(1)?,b',',64)?;
    let insets=match values.get(2){Some(value)=>Some(top_level_split(value,b',',64)?),None=>None};
    if names.len()!=axes.len()||insets.as_ref().is_some_and(|values|values.len()!=names.len()){return None;}
    Some(names.into_iter().zip(axes).enumerate().map(|(index,(name,axis))|{
        let mut value=String::from(name);
        if axis!="block"{value.push(' ');value.push_str(axis);}
        if let Some(insets)=&insets{if insets[index]!="auto"&&insets[index]!="auto auto"{value.push(' ');value.push_str(insets[index]);}}
        value
    }).collect::<Vec<_>>().join(", "))
}

/// Conservative retained heap payload for sampled progress offsets.
pub fn checked_progress_offset_bytes(offsets:&[DecorationLength])->Option<usize>{
    offsets.iter().try_fold(0usize,|bytes,value|bytes.checked_add(value.retained_bytes()?))
}
