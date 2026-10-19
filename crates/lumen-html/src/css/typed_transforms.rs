//! Bounded source-preserving transform function tokens for Typed OM and CSS.
use super::*;

pub struct TransformFunction<'a> {pub name:String,pub arguments:Vec<&'a str>}

/// Function names, escaped identifiers and argument block boundaries use the
/// canonical CSS Syntax cursor. No used lengths or matrices are guessed here.
pub fn function_tokens(input:&str)->Option<Vec<TransformFunction<'_>>>{
    if input.len()>8192{return None;}
    let mut cursor=syntax::Cursor::new(input,0).ok()?;
    let mut functions=Vec::new();
    while let Some(token)=cursor.next(){
        if matches!(token.kind,syntax::TokenKind::Comment{closed:true}) ||
            matches!(token.kind,syntax::TokenKind::Other)&&is_css_whitespace(input.as_bytes()[token.start]) {continue;}
        if functions.len()>=32{return None;}
        let mut end=token.start;
        let name=consume_selector_identifier(input,&mut end)?.to_ascii_lowercase();
        if end!=token.end || input.as_bytes().get(end)!=Some(&b'('){return None;}
        let block=syntax::block(input,end).ok()?;
        if !block.closed{return None;}
        let arguments=top_level_split(&input[end+1..block.content_end],b',',16)?;
        if arguments.is_empty()||arguments.iter().any(|argument|argument.trim().is_empty()){return None;}
        functions.push(TransformFunction{name,arguments});
        cursor.position=block.after;
    }
    (!functions.is_empty()).then_some(functions)
}

// Interpolation maps use the existing Syntax splitters, numeric AST and easing
// authority. No length or reference box is consulted while validating syntax.
struct InterpolationStop<'a> { position:&'a str, value:&'a str, easing:&'a str }
struct TransformInterpolation<'a> { progress:&'a str, global_easing:&'a str, stops:Vec<InterpolationStop<'a>> }
fn coordinate_expression(raw:&str)->Option<typed_numeric::NumericExpression> {
    use typed_numeric::{NumericType as T,NumericUnit as U};
    let expression=typed_numeric::parse_numeric_expression(raw)?;
    let kind=expression.numeric_type()?;
    [U::Number,U::Percent,U::Px,U::Deg,U::S,U::Hz,U::Dppx,U::Fr].into_iter()
        .any(|unit|kind==T::from_unit(unit)).then_some(expression)
}
fn coordinate_type(raw:&str)->Option<Option<typed_numeric::NumericType>> {
    use typed_numeric::{NumericType as T,NumericUnit as U};
    let kind=coordinate_expression(raw)?.numeric_type()?;
    Some(if kind==T::from_unit(U::Number)||kind==T::from_unit(U::Percent){None}else{Some(kind)})
}
impl<'a> TransformInterpolation<'a> {
    fn parse(arguments:&[&'a str])->Option<Self> {
        if arguments.len()<2{return None;}
        let header=components(arguments[0])?;
        let mut progress=None;let mut global_easing="linear";let mut default_easing="linear";
        let mut saw_global=false;let mut saw_default=false;let mut i=0;
        while i<header.len(){
            let value=header[i];
            if decoded_css_keyword(value,"by") {
                if saw_global{return None;} i+=1;global_easing=*header.get(i)?;
                if crate::animation::ease(global_easing,0.5).is_none(){transition_controls::easing(global_easing)?;}saw_global=true;
            }else if coordinate_expression(value).is_some()||animation_controls::interpolation_timeline(value).is_some(){
                if progress.replace(value).is_some(){return None;}
            }else{
                if saw_default{return None;}if crate::animation::ease(value,0.5).is_none(){transition_controls::easing(value)?;}
                default_easing=value;saw_default=true;
            }
            i+=1;
        }
        let progress=progress?;let timeline=animation_controls::interpolation_timeline(progress).is_some();let mut absolute=if timeline{None}else{coordinate_type(progress)?};
        let mut has_absolute_stop=false;let mut stops=Vec::new();let mut easing=default_easing;let mut between=false;
        for argument in &arguments[1..]{
            let pair=top_level_split(argument,b':',2)?;
            if pair.len()==1 {
                if stops.is_empty()||between{return None;}
                if crate::animation::ease(argument,0.5).is_none(){transition_controls::easing(argument)?;}easing=argument;between=true;continue;
            }
            if pair.len()!=2{return None;}
            let positions=components(pair[0])?;
            if positions.is_empty()||positions.len()>2{return None;}
            if !decoded_css_keyword(pair[1].trim(),"none"){validated_function_tokens(pair[1])?;}
            for position in positions {
                if let Some(kind)=coordinate_type(position)? {
                    if timeline&&kind!=typed_numeric::NumericType::from_unit(typed_numeric::NumericUnit::Px)&&kind!=typed_numeric::NumericType::from_unit(typed_numeric::NumericUnit::S){return None;}
                    if absolute.is_some_and(|old|old!=kind){return None;}
                    absolute=Some(kind);has_absolute_stop=true;
                }
                stops.push(InterpolationStop{position,value:pair[1].trim(),easing});
                // Both positions of a double stop have the same value. Their
                // internal easing is immaterial; the next segment resets below.
            }
            easing=default_easing;between=false;
        }
        if stops.is_empty()||between||!timeline&&coordinate_type(progress)?.is_some()&&!has_absolute_stop{return None;}
        Some(Self{progress,global_easing,stops})
    }
    fn selected(&self,mut resolve:impl FnMut(typed_numeric::NumericValue)->Option<typed_numeric::NumericValue>)->Option<(&'a str,&'a str,f64)> {
        use typed_numeric::{NumericValue,NumericUnit as U};
        let mut scalar=|raw:&str|->Option<NumericValue>{
            let mut expression=coordinate_expression(raw)?;
            expression.map_numeric_values(|mut value|{
                if value.unit==U::Percent{value.value/=100.0;value.unit=U::Number;Some(value)}else{resolve(value)}
            })?;
            expression.simplify_absolute_units();let value=expression.single_numeric_value()?;
            value.value.is_finite().then_some(value)
        };
        let progress=scalar(self.progress)?;
        let values=self.stops.iter().map(|stop|scalar(stop.position)).collect::<Option<Vec<_>>>()?;
        let first=values.iter().find(|value|value.unit!=U::Number);
        let last=values.iter().rev().find(|value|value.unit!=U::Number);
        let normalized=|value:NumericValue|->Option<f64>{
            if value.unit==U::Number{return Some(value.value);}
            let start=first?;let end=last?;
            if value.unit!=start.unit||value.unit!=end.unit{return None;}
            let extent=end.value-start.value;
            // A coincident absolute range is kept deferred rather than inventing
            // a proportional coordinate for an undefined division.
            if extent==0.0{return None;}
            let value=(value.value-start.value)/extent;value.is_finite().then_some(value)
        };
        let mut positions=Vec::new();let mut previous=f64::NEG_INFINITY;
        for value in values.iter().copied(){let position=normalized(value)?.max(previous);positions.push(position);previous=position;}
        let progress=crate::animation::ease(self.global_easing,normalized(progress)?)?;
        // Stops at the same input position jump to the last matching stop.
        if progress<positions[0]{return Some((self.stops[0].value,self.stops[0].value,0.0));}
        let left=positions.iter().rposition(|position|*position<=progress).unwrap_or(0);
        if left+1==positions.len(){return Some((self.stops[left].value,self.stops[left].value,0.0));}
        let amount=(progress-positions[left])/(positions[left+1]-positions[left]);
        let amount=crate::animation::ease(self.stops[left+1].easing,amount)?;
        Some((self.stops[left].value,self.stops[left+1].value,amount))
    }
}

/// The rare deferred carrier is recognized by decoded canonical function names.
pub(super) fn has_deferred_interpolation(input:&str)->bool {
    if !input.contains(':'){return false;}
    function_tokens(input).is_some_and(|functions|functions.iter().any(|function|function.name=="transform-interpolate"))
}

/// Detect only canonical map progress sources. Numeric maps and ordinary
/// transform functions keep the existing allocation-free caller gate.
pub(super) fn has_timeline_progress(input:&str)->bool {
    if !has_deferred_interpolation(input){return false;}
    let Some(functions)=validated_function_tokens(input) else{return false;};
    functions.iter().filter(|function|function.name=="transform-interpolate").any(|function|
        TransformInterpolation::parse(&function.arguments).is_some_and(|map|
            animation_controls::interpolation_timeline(map.progress).is_some()
            ||map.stops.iter().any(|stop|has_timeline_progress(stop.value))))
}

/// Initial registrations use the same numeric unit authority recursively
/// through modern map coordinates and transform values.
pub(super) fn computationally_independent(input:&str)->bool {
    let Some(functions)=validated_function_tokens(input) else{return false;};
    let independent=|expression:typed_numeric::NumericExpression|!expression.contains_tree_functions()&&!expression.contains_unit(|unit|!typed_numeric::computationally_independent_unit(unit));
    functions.iter().all(|function| {
        if function.name=="transform-interpolate" {
            return TransformInterpolation::parse(&function.arguments).is_some_and(|map|
                coordinate_expression(map.progress).is_some_and(independent)&&transition_controls::computationally_independent_easing(map.global_easing)&&map.stops.iter().all(|stop|
                    transition_controls::computationally_independent_easing(stop.easing)&&coordinate_expression(stop.position).is_some_and(independent)&&
                    (decoded_css_keyword(stop.value,"none")||computationally_independent(stop.value))));
        }
        function.arguments.iter().enumerate().all(|(index,raw)|
            function.name=="perspective"&&decoded_css_keyword(raw,"none")||
            argument_kind(&function.name,function.arguments.len(),index).and_then(|kind|argument_expression(raw,kind)).is_some_and(independent))
    })
}

/// Argument roles shared by registered-property computation and Typed OM.
/// The canonical Syntax tokenizer remains the sole transform function scanner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransformArgument { Number, Scale, Angle, Length, LengthPercentage }

pub fn argument_kind(name: &str, count: usize, at: usize) -> Option<TransformArgument> {
    use TransformArgument::*;
    if at >= count { return None; }
    match name {
        "matrix" if count == 6 => Some(Number),
        "matrix3d" if count == 16 => Some(Number),
        "translate" if (1..=2).contains(&count) => Some(LengthPercentage),
        "translatex" | "translatey" if count == 1 => Some(LengthPercentage),
        "translatez" if count == 1 => Some(Length),
        "translate3d" if count == 3 => Some(if at == 2 { Length } else { LengthPercentage }),
        "scale" if (1..=2).contains(&count) => Some(Scale),
        "scalex" | "scaley" | "scalez" if count == 1 => Some(Scale),
        "scale3d" if count == 3 => Some(Scale),
        "rotate" | "rotatex" | "rotatey" | "rotatez" if count == 1 => Some(Angle),
        "rotate3d" if count == 4 => Some(if at == 3 { Angle } else { Number }),
        "skew" if (1..=2).contains(&count) => Some(Angle),
        "skewx" | "skewy" if count == 1 => Some(Angle),
        "perspective" if count == 1 => Some(Length),
        _ => None,
    }
}

pub fn argument_expression(raw: &str, kind: TransformArgument) -> Option<typed_numeric::NumericExpression> {
    use typed_numeric::{NumericExpression as E, NumericUnit as U, NumericType as T, NumericDimension as D};
    let mut expression = typed_numeric::parse_numeric_expression(raw.trim())?;
    if kind==TransformArgument::Scale{expression.percentages_as_numbers()?;}
    if let E::Value(value) = &mut expression {
        if value.unit == U::Number && value.value == 0.0 {
            value.unit = match kind { TransformArgument::Number | TransformArgument::Scale => U::Number, TransformArgument::Angle => U::Deg, _ => U::Px };
        }
    }
    let actual = expression.numeric_type()?;
    let expected = T::from_unit(match kind { TransformArgument::Number | TransformArgument::Scale => U::Number, TransformArgument::Angle => U::Deg, _ => U::Px });
    let accepted = actual == expected || kind == TransformArgument::LengthPercentage &&
        (actual == T::from_unit(U::Percent) || actual == T { percent_hint: Some(D::Length), ..expected });
    accepted.then_some(expression)
}

/// The renderer's compact primitive array stores finite 2D operations. Modern
/// interpolation maps and 3D operations use the same sparse slot source carrier
/// and canonical matrix codec, preserving percentages and owner dependencies.
pub(super) fn requires_source_carrier(input:&str)->bool {
    function_tokens(input).is_some_and(|functions|functions.iter().any(|function| {
        if matches!(function.name.as_str(),"transform-interpolate"|"matrix3d"|"translatez"|"translate3d"|"scalez"|"scale3d"|"rotatex"|"rotatey"|"rotatez"|"rotate3d"|"perspective"){return true;}
        if function.name!="rotate" || function.arguments.len()!=1{return false;}
        let raw=function.arguments[0];
        let Some(expression)=argument_expression(raw,TransformArgument::Angle) else{return false;};
        let Some(degrees)=resolved_argument_scalar(&expression,None) else{return true;};
        let legacy=angle(raw).unwrap_or((degrees*core::f64::consts::PI/180.0)as f32);
        let legacy_quarter=lumen_common::dom_geometry::exact_quarter_turn_radians_f32(legacy);
        // Compare the actual quadrant as well as integral-quarter admission:
        // large authored angles can lose whole quadrants in f32 conversion.
        // Near-quarter collisions likewise retain canonical f64 evaluation.
        if degrees.rem_euclid(90.0)==0.0 {
            let precise=lumen_common::dom_geometry::sin_cos_degrees(degrees);
            legacy_quarter.is_none_or(|(s,c)|(f64::from(s),f64::from(c))!=precise)
        }else{legacy_quarter.is_some()}
    }))
}

pub fn validated_function_tokens(input: &str) -> Option<Vec<TransformFunction<'_>>> {
    let functions = function_tokens(input)?;
    for function in &functions {
        if function.name=="transform-interpolate"{TransformInterpolation::parse(&function.arguments)?;continue;}
        argument_kind(&function.name,function.arguments.len(),0)?;
        for (at, raw) in function.arguments.iter().enumerate() {
            let kind = argument_kind(&function.name, function.arguments.len(), at)?;
            if function.name == "perspective" && decoded_css_keyword(raw.trim(), "none") { continue; }
            argument_expression(raw, kind)?;
        }
    }
    Some(functions)
}

fn canonical_name(name: &str) -> &str {
    match name {
        "translatex" => "translateX", "translatey" => "translateY", "translatez" => "translateZ",
        "scalex" => "scaleX", "scaley" => "scaleY", "scalez" => "scaleZ",
        "rotatex" => "rotateX", "rotatey" => "rotateY", "rotatez" => "rotateZ",
        "skewx" => "skewX", "skewy" => "skewY", _ => name,
    }
}

/// Specified transform functions share argument roles with computed codecs;
/// math uses the canonical simplifier while bare authored dimensions retain units.
pub(super) fn specified(input:&str)->Option<String>{
    if decoded_css_keyword(input.trim(),"none"){return Some("none".into());}
    let functions=validated_function_tokens(input)?;let mut output=String::new();
    for function in functions {
        if function.name=="transform-interpolate"{append_function(&mut output,&alloc::format!("transform-interpolate({})",function.arguments.join(", ")))?;continue;}
        let mut arguments=Vec::new();arguments.try_reserve_exact(function.arguments.len()).ok()?;
        for(index,raw)in function.arguments.iter().enumerate(){
            if function.name=="perspective"&&decoded_css_keyword(raw,"none"){arguments.push("none".into());continue;}
            let kind=argument_kind(&function.name,function.arguments.len(),index)?;
            let expression=argument_expression(raw,kind)?;
            arguments.push(if matches!(expression,typed_numeric::NumericExpression::Value(_)){
                let value=expression.single_numeric_value()?;typed_numeric::serialize_numeric_value(value.value,value.unit)
            }else{typed_numeric::parse_numeric_expression(raw)?.serialize_specified()?});
        }
        append_function(&mut output,&alloc::format!("{}({})",canonical_name(&function.name),arguments.join(", ")))?;
    }
    Some(output)
}

/// Registered transforms preserve function identity and percentage bases.
/// The owner resolves numeric leaves against its computed font/query context.
pub(super) fn computed(input: &str, mut resolve: impl FnMut(typed_numeric::NumericValue) -> Option<typed_numeric::NumericValue>) -> Option<String> {
    computed_with(input,&mut resolve)
}
fn computed_with(input:&str,resolve:&mut dyn FnMut(typed_numeric::NumericValue)->Option<typed_numeric::NumericValue>)->Option<String> {
    let functions = validated_function_tokens(input)?;
    let deferred=functions.iter().any(|function|function.name=="transform-interpolate");
    let mut output = String::new();
    for function in functions {
        if !output.is_empty() { output.push(' '); }
        if function.name=="transform-interpolate" {
            let value=computed_interpolation(&function.arguments,resolve)?;
            if output.len().checked_add(value.len()).is_none_or(|bytes|bytes>MAX_VARIABLE_BYTES){return None;}
            output.push_str(if value=="none"{"matrix(1, 0, 0, 1, 0, 0)"}else{&value});continue;
        }
        output.push_str(canonical_name(&function.name)); output.push('(');
        for (at, raw) in function.arguments.iter().enumerate() {
            if at != 0 { output.push_str(", "); }
            if function.name == "perspective" && decoded_css_keyword(raw.trim(), "none") { output.push_str("none"); continue; }
            let kind = argument_kind(&function.name, function.arguments.len(), at)?;
            let mut expression = argument_expression(raw, kind)?;
            expression.map_numeric_values(&mut *resolve)?;
            expression.simplify_absolute_units();
            let value = expression.single_numeric_value().map(|value| typed_numeric::serialize_numeric_value(value.value, value.unit))
                .or_else(|| expression.serialize())?;
            if output.len().checked_add(value.len()).is_none_or(|bytes| bytes > MAX_VARIABLE_BYTES) { return None; }
            output.push_str(&value);
        }
        output.push(')');
    }
    if deferred{function_tokens(&output)?;}
    Some(output)
}

fn computed_interpolation(arguments:&[&str],resolve:&mut dyn FnMut(typed_numeric::NumericValue)->Option<typed_numeric::NumericValue>)->Option<String> {
    let mut map=TransformInterpolation::parse(arguments)?;
    // CSS progress sources here name scroll/view timelines. Their coordinate
    // is a length; a time-typed absolute map is grammatically valid but cannot
    // compute against that coordinate (Values 5 interpolation progress).
    if animation_controls::interpolation_timeline(map.progress).is_some()
        &&map.stops.iter().filter_map(|stop|coordinate_type(stop.position).flatten()).any(|kind|kind==typed_numeric::NumericType::from_unit(typed_numeric::NumericUnit::S)){return None;}
    // Literal timings retain the existing borrowed sampling path. Only a
    // calculated or decoded timing needs source computation before selection.
    let contextual_easing=core::iter::once(map.global_easing).chain(map.stops.iter().map(|stop|stop.easing))
        .any(|raw|crate::animation::ease(raw,0.5).is_none());
    let easings=if contextual_easing {
        Some(core::iter::once(map.global_easing).chain(map.stops.iter().map(|stop|stop.easing))
            .map(|raw|transition_controls::computed_easing_with(raw,&mut *resolve)).collect::<Option<Vec<_>>>()?)
    }else{None};
    if let Some(easings)=&easings {
        map.global_easing=&easings[0];
        for (stop,easing) in map.stops.iter_mut().zip(&easings[1..]){stop.easing=easing;}
    }
    let transform=|raw:&str,resolve:&mut dyn FnMut(typed_numeric::NumericValue)->Option<typed_numeric::NumericValue>| {
        if decoded_css_keyword(raw,"none"){Some("none".into())}else{computed_with(raw,resolve)}
    };
    if let Some((from,to,progress))=map.selected(&mut *resolve) {
        let from=transform(from,resolve)?;let to=transform(to,resolve)?;
        if let Some(value)=combine_computed(&from,&to,registered_properties::ComputedValueOperation::Interpolate(progress),true,None){return Some(value);}
    }
    // Contextual coordinates or easing overshoot can leave the complete map
    // deferred. Each numeric leaf and transform argument still computes once
    // against the owning element, rather than against its future consumer.
    let mut result=Vec::new();
    let freeze=|raw:&str,resolve:&mut dyn FnMut(typed_numeric::NumericValue)->Option<typed_numeric::NumericValue>|->Option<String>{
        let mut expression=coordinate_expression(raw)?;
        expression.map_numeric_values(resolve)?;expression.simplify_absolute_units();expression.serialize()
    };
    let mut header=Vec::new();
    for value in components(arguments[0])? {
        header.push(if coordinate_expression(value).is_some(){freeze(value,resolve)?}else if animation_controls::interpolation_timeline(value).is_some(){animation_controls::computed_interpolation_timeline(value,resolve)?}else if decoded_css_keyword(value,"by"){String::from("by")}else{transition_controls::computed_easing_with(value,&mut *resolve)?});
    }
    result.push(header.join(" "));
    for argument in &arguments[1..] {
        let pair=top_level_split(argument,b':',2)?;
        if pair.len()==1{result.push(transition_controls::computed_easing_with(argument,&mut *resolve)?);continue;}
        let positions=components(pair[0])?.into_iter().map(|raw|freeze(raw,resolve)).collect::<Option<Vec<_>>>()?;
        result.push(alloc::format!("{}: {}",positions.join(" "),transform(pair[1].trim(),resolve)?));
    }
    let value=alloc::format!("transform-interpolate({})",result.join(", "));
    if value.len()>MAX_VARIABLE_BYTES{return None;}
    function_tokens(&value)?;
    Some(value)
}

/// Resolve owner-qualified progress sources only when the caller can supply
/// actual timeline coordinates. Other computed arguments and source tokens
/// stay in the canonical transform representation until reference boxes exist.
pub(crate) fn resolve_timeline_progress(input:&str,progress:&mut dyn FnMut(&str,Option<typed_numeric::NumericType>)->Option<typed_numeric::NumericValue>)->Option<String>{
    if decoded_css_keyword(input.trim(),"none"){return Some("none".into());}
    let mut result=String::new();
    for function in validated_function_tokens(input)?{
        if function.name!="transform-interpolate"{
            append_function(&mut result,&ComputedFunction::from_token(function)?.serialize()?)?;continue;
        }
        let map=TransformInterpolation::parse(&function.arguments)?;
        let absolute=map.stops.iter().find_map(|stop|coordinate_type(stop.position).flatten());
        let sampled=if animation_controls::interpolation_timeline(map.progress).is_some(){Some(progress(map.progress,absolute)?)}else{None};
        let mut arguments=Vec::new();
        let mut header=Vec::new();
        for component in components(function.arguments[0])?{
            header.push(if component==map.progress&&sampled.is_some(){let value=sampled?;typed_numeric::serialize_numeric_value(value.value,value.unit)}else{component.to_string()});
        }
        arguments.push(header.join(" "));
        for argument in &function.arguments[1..]{
            let pair=top_level_split(argument,b':',2)?;
            arguments.push(if pair.len()==2{alloc::format!("{}: {}",pair[0],resolve_timeline_progress(pair[1].trim(),progress)?)}else{argument.to_string()});
        }
        let borrowed=arguments.iter().map(String::as_str).collect::<Vec<_>>();
        let mut identity=|value:typed_numeric::NumericValue|Some(value);
        let value=computed_interpolation(&borrowed,&mut identity)?;
        append_function(&mut result,&value)?;
    }
    Some(result)
}

// Animation extends the same validated transform token representation. Numeric
// arguments remain canonical CSS math trees until a matrix needs used geometry.
#[derive(Clone)]
struct ComputedFunction {name:String,arguments:Vec<String>}
impl ComputedFunction {
    fn from_token(value:TransformFunction<'_>)->Option<Self> {
        if value.name=="transform-interpolate" {
            TransformInterpolation::parse(&value.arguments)?;
            return Some(Self{name:value.name,arguments:value.arguments.iter().map(|argument|argument.to_string()).collect()});
        }
        let mut arguments=Vec::new();arguments.try_reserve_exact(value.arguments.len()).ok()?;
        for (index,raw) in value.arguments.iter().enumerate(){
            if value.name=="perspective"&&decoded_css_keyword(raw.trim(),"none"){arguments.push("none".into());continue;}
            let mut expression=argument_expression(raw,argument_kind(&value.name,value.arguments.len(),index)?)?;
            expression.simplify_absolute_units();arguments.push(expression.serialize()?);
        }
        Some(Self{name:value.name,arguments})
    }
    fn identity(&self)->Self {
        if self.name=="transform-interpolate"{return Self{name:"matrix".into(),arguments:["1","0","0","1","0","0"].into_iter().map(str::to_string).collect()};}
        let arguments=self.arguments.iter().enumerate().map(|(i,_)| {
            if self.name=="perspective" {"none".into()}
            else if self.name.starts_with("scale"){"1".into()}
            else if self.name=="matrix" {if i==0||i==3{"1".into()}else{"0".into()}}
            else if self.name=="matrix3d" {if i%5==0{"1".into()}else{"0".into()}}
            else if self.name=="rotate3d"&&i<3 {self.arguments[i].clone()}
            else{match argument_kind(&self.name,self.arguments.len(),i){Some(TransformArgument::Angle)=>"0deg".into(),Some(TransformArgument::Number|TransformArgument::Scale)=>"0".into(),_=>"0px".into()}}
        }).collect();Self{name:self.name.clone(),arguments}
    }
    fn family(&self)->Option<&'static str>{
        if self.name.starts_with("translate"){Some("translate")}else if self.name.starts_with("scale"){Some("scale")}else if self.name.starts_with("rotate"){Some("rotate")}else if self.name.starts_with("skew"){Some("skew")}else{None}
    }
    fn three_dimensional(&self)->bool{matches!(self.name.as_str(),"translatez"|"translate3d"|"scalez"|"scale3d"|"rotatex"|"rotatey"|"rotatez"|"rotate3d")}
    fn primitive(&self,three:bool)->Option<Self>{
        let family=self.family()?;let a=&self.arguments;
        let mut args=match family {
            "translate"=>vec!["0px".into(),"0px".into(),"0px".into()],
            "scale"=>vec!["1".into(),"1".into(),"1".into()],
            "rotate"=>vec!["0".into(),"0".into(),"1".into(),"0deg".into()],
            "skew"=>vec!["0deg".into(),"0deg".into()],_=>return None,
        };
        match self.name.as_str() {
            "translate"|"scale"|"skew"=>{args[0]=a[0].clone();if a.len()>1{args[1]=a[1].clone();}else if family=="scale"{args[1]=a[0].clone();}},
            "translatex"|"scalex"|"skewx"=>args[0]=a[0].clone(),
            "translatey"|"scaley"|"skewy"=>args[1]=a[0].clone(),
            "translatez"|"scalez"=>args[2]=a[0].clone(),
            "translate3d"|"scale3d"=>args.clone_from(a),
            "rotate"|"rotatez"=>args[3]=a[0].clone(),
            "rotatex"=>{args[0]="1".into();args[2]="0".into();args[3]=a[0].clone();},
            "rotatey"=>{args[1]="1".into();args[2]="0".into();args[3]=a[0].clone();},
            "rotate3d"=>args.clone_from(a),_=>return None,
        }
        let name=if family=="rotate" {if three{"rotate3d"}else{args=vec![args[3].clone()];"rotate"}}
            else if three {if family=="translate"{"translate3d"}else{"scale3d"}}
            else{if family!="skew"{args.truncate(2);}family};
        Some(Self{name:name.into(),arguments:args})
    }
    fn serialize(&self)->Option<String> {
        let size=self.arguments.iter().try_fold(self.name.len()+2,|total,arg|total.checked_add(arg.len()+2))?;
        if size>MAX_VARIABLE_BYTES{return None;}
        Some(alloc::format!("{}({})",canonical_name(&self.name),self.arguments.join(", ")))
    }
    fn reference_dependent(&self)->bool {
        if self.name=="transform-interpolate" {
            let arguments=self.arguments.iter().map(String::as_str).collect::<Vec<_>>();
            return TransformInterpolation::parse(&arguments).is_some_and(|map|map.stops.iter().any(|stop|computed_functions(stop.value).is_some_and(|functions|functions.iter().any(Self::reference_dependent))));
        }
        self.arguments.iter().enumerate().any(|(index,argument)| {
            argument_kind(&self.name,self.arguments.len(),index)==Some(TransformArgument::LengthPercentage)
                &&argument_expression(argument,TransformArgument::LengthPercentage).is_some_and(|expression|expression.contains_unit(|unit|unit==typed_numeric::NumericUnit::Percent))
        })
    }
    fn scalar(&self,index:usize,basis:Option<f64>)->Option<f64> {
        let expression=argument_expression(self.arguments.get(index)?,argument_kind(&self.name,self.arguments.len(),index)?)?;
        resolved_argument_scalar(&expression,basis)
    }
    fn matrix(&self,reference:Option<[f64;2]>)->Option<lumen_common::dom_geometry::Matrix> {
        use lumen_common::dom_geometry::{Matrix,Point};let identity=Matrix::default();
        if self.name=="transform-interpolate" {
            let arguments=self.arguments.iter().map(String::as_str).collect::<Vec<_>>();
            let map=TransformInterpolation::parse(&arguments)?;
            let(from,to,progress)=map.selected(Some)?;
            let value=combine_computed(from,to,registered_properties::ComputedValueOperation::Interpolate(progress),true,reference)?;
            if reference.is_none()&&has_deferred_interpolation(&value){return None;}
            return computed_matrix(&value,reference);
        }
        if self.name=="matrix" {let mut values=[0.0;6];for (i,value) in values.iter_mut().enumerate(){*value=self.scalar(i,None)?;}return Some(Matrix::from_affine(values));}
        if self.name=="matrix3d" {let mut values=[0.0;16];for(i,value)in values.iter_mut().enumerate(){*value=self.scalar(i,None)?;}let mut matrix=Matrix{values,is_2d:false};matrix.is_2d=matrix.has_2d_components();return Some(matrix);}
        if self.name=="perspective" {let mut matrix=identity;if self.arguments[0]!="none"{matrix.values[11]=-1.0/self.scalar(0,None)?.max(1.0);}matrix.is_2d=matrix.has_2d_components();return Some(matrix);}
        let normalized=self.primitive(self.family()!=Some("skew"))?;
        Some(match normalized.name.as_str(){
            "translate3d"=>identity.translated(normalized.scalar(0,reference.map(|r|r[0]))?,normalized.scalar(1,reference.map(|r|r[1]))?,normalized.scalar(2,None)?),
            "scale3d"=>identity.scaled(normalized.scalar(0,None)?,normalized.scalar(1,None)?,normalized.scalar(2,None)?,Point::default()),
            "rotate3d"=>identity.rotated_axis(normalized.scalar(0,None)?,normalized.scalar(1,None)?,normalized.scalar(2,None)?,normalized.scalar(3,None)?),
            "skew"=>identity.skewed(normalized.scalar(0,None)?,normalized.scalar(1,None)?),_=>return None,
        })
    }
}
pub(super) fn resolved_argument_scalar(expression:&typed_numeric::NumericExpression,basis:Option<f64>)->Option<f64> {
    use typed_numeric::{NumericType as T,NumericUnit as U};
    let kind=expression.numeric_type()?;
    if ![U::Number,U::Px,U::Deg,U::Percent].into_iter().any(|unit|kind==T::from_unit(unit))
        &&kind!=(T{percent_hint:Some(typed_numeric::NumericDimension::Length),..T::from_unit(U::Px)}){return None;}
    if expression.contains_unit(|unit|unit.canonical_unit_and_factor().is_none()){return None;}
    if basis.is_none()&&expression.contains_unit(|unit|unit==U::Percent){return None;}
    let value=expression.evaluate(&mut FontAngleContext{length:None,query:ContainerUnitContext::default(),percent_scale:basis.unwrap_or(100.0)/100.0})?;
    // CSS Values censors special IEEE values only at the final value boundary.
    Some(if value.is_finite(){if value==0.0{0.0}else{value}}else{f64::from(typed_numeric::computed_f32(value))})
}

fn computed_functions(input:&str)->Option<Vec<ComputedFunction>> {
    if decoded_css_keyword(input.trim(),"none"){return Some(Vec::new());}
    validated_function_tokens(input)?.into_iter().map(ComputedFunction::from_token).collect()
}
fn combine_scalar(a:f64,b:f64,operation:registered_properties::ComputedValueOperation,identity:f64)->Option<f64> {
    use registered_properties::ComputedValueOperation as Op;
    let value=match operation{Op::Interpolate(progress)=>if progress.is_finite(){a+(b-a)*progress}else{return None;},Op::Add=>a+b,Op::Accumulate(count)=>if count.is_finite(){count*(a-identity)+b}else{return None;}};
    value.is_finite().then_some(value)
}
fn combine_function(a:&ComputedFunction,b:&ComputedFunction,operation:registered_properties::ComputedValueOperation)->Option<ComputedFunction> {
    use registered_properties::ComputedValueOperation as Op;
    if matches!(a.name.as_str(),"matrix"|"matrix3d"|"perspective"|"transform-interpolate")||matches!(b.name.as_str(),"matrix"|"matrix3d"|"perspective"|"transform-interpolate"){return None;}
    let (mut a,mut b)=if a.name==b.name&&a.arguments.len()==b.arguments.len(){(a.clone(),b.clone())}
        else if a.family()==b.family(){let three=a.three_dimensional()||b.three_dimensional();(a.primitive(three)?,b.primitive(three)?)}else{return None;};
    let start=if a.name=="rotate3d" {
        let left=[a.scalar(0,None)?,a.scalar(1,None)?,a.scalar(2,None)?,a.scalar(3,None)?];
        let right=[b.scalar(0,None)?,b.scalar(1,None)?,b.scalar(2,None)?,b.scalar(3,None)?];
        let(left,right)=matched_rotation_arguments(left,right)?;
        for i in 0..4 {let unit=if i==3{typed_numeric::NumericUnit::Deg}else{typed_numeric::NumericUnit::Number};a.arguments[i]=typed_numeric::serialize_numeric_value(left[i],unit);b.arguments[i]=typed_numeric::serialize_numeric_value(right[i],unit);}
        3
    }else{0};
    for i in start..a.arguments.len(){
        let primitive=|raw:&str|{let mut value=typed_numeric::parse_numeric_expression(raw)?;value.simplify_absolute_units();value.single_numeric_value()};
        let identity=if a.name.starts_with("scale"){1.0}else{0.0};
        if let (Some(left),Some(right))=(primitive(&a.arguments[i]),primitive(&b.arguments[i])) {
            if left.unit==right.unit {
                a.arguments[i]=typed_numeric::serialize_numeric_value(combine_scalar(left.value,right.value,operation,identity)?,left.unit);continue;
            }
        }
        a.arguments[i]=match operation {
            Op::Interpolate(progress)=>crate::animation::combine_numeric_values(&[(&a.arguments[i],1.0-progress),(&b.arguments[i],progress)]),
            Op::Add=>crate::animation::combine_numeric_values(&[(&a.arguments[i],1.0),(&b.arguments[i],1.0)]),
            Op::Accumulate(count)=>if a.name.starts_with("scale") {crate::animation::combine_numeric_values(&[(&a.arguments[i],count),(&b.arguments[i],1.0),("1",-count)])}else{crate::animation::combine_numeric_values(&[(&a.arguments[i],count),(&b.arguments[i],1.0)])},
        }?;
    }
    Some(a)
}
fn append_function(output:&mut String,value:&str)->Option<()> {
    let extra=value.len().checked_add(usize::from(!output.is_empty()))?;
    if output.len().checked_add(extra)?>MAX_VARIABLE_BYTES{return None;}
    output.try_reserve(extra).ok()?;if !output.is_empty(){output.push(' ');}output.push_str(value);Some(())
}
/// Source-preserving transform combination: compatible primitives keep their
/// percentages; the first incompatible suffix uses the shared matrix authority.
/// The caller supplies actual reference dimensions only for used-value sampling.
pub(crate) fn combine_computed(from:&str,to:&str,operation:registered_properties::ComputedValueOperation,list:bool,reference:Option<[f64;2]>)->Option<String> {
    use registered_properties::ComputedValueOperation as Op;
    let a=computed_functions(from)?;let b=computed_functions(to)?;
    let mut output=String::new();
    if matches!(operation,Op::Add)&&list {
        if a.len().checked_add(b.len())?>32{return None;}
        for function in a.iter().chain(&b){append_function(&mut output,&function.serialize()?)?;}
        return Some(if output.is_empty(){"none".into()}else{output});
    }
    let output=combine_sequences(&a,&b,operation,|function|function.identity(),
        |a,b|combine_function(a,b,operation)?.serialize(),
        |left,right|{
            let matrices=|functions:&[ComputedFunction]|functions.iter().try_fold(lumen_common::dom_geometry::Matrix::default(),|matrix,function|Some(matrix.multiply(function.matrix(reference)?)));
            if let (Some(a),Some(b))=(matrices(left),matrices(right)){return combine_matrices(a,b,operation);}
            if reference.is_none()&&left.iter().chain(right).any(ComputedFunction::reference_dependent) {
                if let Op::Interpolate(progress)=operation {
                    if (0.0..=1.0).contains(&progress) {
                        let source=|functions:&[ComputedFunction]|->Option<String>{let mut text=String::new();for function in functions{append_function(&mut text,&function.serialize()?)?;}Some(if text.is_empty(){"none".into()}else{text})};
                        let value=alloc::format!("transform-interpolate({}, 0: {}, 1: {})",typed_numeric::serialize_numeric_value(progress,typed_numeric::NumericUnit::Number),source(left)?,source(right)?);
                        if value.len()>MAX_VARIABLE_BYTES{return None;}
                        function_tokens(&value)?;
                        return Some(value);
                    }
                }
            }
            None
        })?;
    if has_deferred_interpolation(&output){function_tokens(&output)?;}
    Some(output)
}

/// Matching, identity padding and matrix suffix selection are shared by the
/// source-preserving and already resolved rendering representations.
fn combine_sequences<T>(a:&[T],b:&[T],operation:registered_properties::ComputedValueOperation,
    identity:impl Fn(&T)->T,pair:impl Fn(&T,&T)->Option<String>,suffix:impl Fn(&[T],&[T])->Option<String>)->Option<String> {
    let _=operation;
    let mut output=String::new();
    for i in 0..a.len().max(b.len()) {
        let left_identity=if a.get(i).is_none(){Some(identity(&b[i]))}else{None};
        let right_identity=if b.get(i).is_none(){Some(identity(&a[i]))}else{None};
        let left=a.get(i).or(left_identity.as_ref())?;let right=b.get(i).or(right_identity.as_ref())?;
        if let Some(value)=pair(left,right){append_function(&mut output,&value)?;continue;}
        append_function(&mut output,&suffix(a.get(i..).unwrap_or(&[]),b.get(i..).unwrap_or(&[]))?)?;
        break;
    }
    Some(if output.is_empty(){"none".into()}else{output})
}

pub(super) fn computed_matrix(input:&str,reference:Option<[f64;2]>)->Option<lumen_common::dom_geometry::Matrix> {
    computed_functions(input)?.iter().try_fold(lumen_common::dom_geometry::Matrix::default(),|matrix,function|Some(matrix.multiply(function.matrix(reference)?)))
}
pub(super) fn matrix_css_value(matrix:lumen_common::dom_geometry::Matrix)->Option<String> {
    if !matrix.values.iter().all(|value|value.is_finite()){return None;}
    let values=if matrix.has_2d_components(){vec![matrix.values[0],matrix.values[1],matrix.values[4],matrix.values[5],matrix.values[12],matrix.values[13]]}else{matrix.values.to_vec()};
    let args=values.into_iter().map(|value|typed_numeric::serialize_numeric_value(value,typed_numeric::NumericUnit::Number)).collect::<Vec<_>>();
    Some(alloc::format!("{}({})",if args.len()==6{"matrix"}else{"matrix3d"},args.join(", ")))
}
fn combine_matrices(left:lumen_common::dom_geometry::Matrix,right:lumen_common::dom_geometry::Matrix,operation:registered_properties::ComputedValueOperation)->Option<String> {
    use registered_properties::ComputedValueOperation as Op;
    matrix_css_value(match operation{Op::Interpolate(progress)=>left.interpolate(right,progress),Op::Add=>left.accumulate(right,1.0),Op::Accumulate(count)=>left.accumulate(right,count)}?)
}

/// Rendering values already contain resolved finite primitive parameters.
/// They never serialize and reparse source text to enter the shared combiner.
pub(crate) fn combine_resolved(from:&[Transform],to:&[Transform],operation:registered_properties::ComputedValueOperation,width:f32,height:f32)->Option<String> {
    use lumen_common::dom_geometry::Matrix;
    let zero=TransformLength{pixels:0.0,percent:0.0};
    let identity=|value:&Transform|match value {
        Transform::Matrix(_)=>Transform::Matrix(crate::paint::Affine::IDENTITY),Transform::Translate(_,_)=>Transform::Translate(zero,zero),
        Transform::Scale(_,_)=>Transform::Scale(1.0,1.0),Transform::Rotate(_)=>Transform::Rotate(0.0),Transform::Skew(_,_)=>Transform::Skew(0.0,0.0),
    };
    let scalar=|a:f32,b:f32,identity:f64|{let value=combine_scalar(a as f64,b as f64,operation,identity)? as f32;value.is_finite().then_some(value)};
    let length=|a:TransformLength,b:TransformLength|Some(TransformLength{pixels:scalar(a.pixels,b.pixels,0.0)?,percent:scalar(a.percent,b.percent,0.0)?});
    let pair=|a:&Transform,b:&Transform| {
        let result=match (*a,*b) {
            (Transform::Translate(ax,ay),Transform::Translate(bx,by))=>Transform::Translate(length(ax,bx)?,length(ay,by)?),
            (Transform::Scale(ax,ay),Transform::Scale(bx,by))=>Transform::Scale(scalar(ax,bx,1.0)?,scalar(ay,by,1.0)?),
            (Transform::Rotate(a),Transform::Rotate(b))=>Transform::Rotate(scalar(a,b,0.0)?),
            (Transform::Skew(ax,ay),Transform::Skew(bx,by))=>Transform::Skew(scalar(ax,bx,0.0)?,scalar(ay,by,0.0)?),_=>return None,
        };
        let resolved=result.matrix(width,height);
        [resolved.a,resolved.b,resolved.c,resolved.d,resolved.e,resolved.f].iter().all(|value|value.is_finite()).then(||crate::animation::serialize_transforms(&[result]))
    };
    combine_sequences(from,to,operation,identity,pair,|left,right|{
        let matrix=|functions:&[Transform]|functions.iter().try_fold(Matrix::default(),|matrix,function|{
            let affine=function.matrix(width,height);let values=[affine.a,affine.b,affine.c,affine.d,affine.e,affine.f];
            values.iter().all(|value|value.is_finite()).then(||matrix.multiply(Matrix::from_affine(values.map(f64::from))))
        });
        combine_matrices(matrix(left)?,matrix(right)?,operation)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn specification_transform_map_timings_share_context_computation_and_initial_independence() {
        use typed_numeric::{NumericUnit as U,NumericValue};
        let source="transform-interpolate(0.4 by StEpS(calc(1em / 5px), JuMp-EnD), 0: translateX(0px), 1: translateX(100px))";
        assert!(validated_function_tokens(source).is_some(),"map admission uses the same calculated steps grammar as transition timing");
        let owner=|em|move|value:NumericValue|Some(if value.unit==U::Em{NumericValue{value:value.value*em,unit:U::Px}}else{value});
        assert_eq!(computed(source,owner(20.0)).as_deref(),Some("translateX(25px)"));
        assert_eq!(computed(source,owner(40.0)).as_deref(),Some("translateX(37.5px)"));
        let default="transform-interpolate(0.4 steps(calc(4)), 0: translateX(0px), 1: translateX(100px))";
        let between="transform-interpolate(0.4, 0: translateX(0px), steps(calc(4)), 1: translateX(100px))";
        for source in [default,between]{assert_eq!(computed(source,Some).as_deref(),Some("translateX(25px)"));}
        assert!(!computationally_independent(source),"font-dependent global easing is not a valid computationally independent initial value");
        assert!(!computationally_independent("transform-interpolate(0.4 steps(calc(1cqw / 5px)), 0: scale(1), 1: scale(2))"));
        assert!(!computationally_independent("transform-interpolate(0.4, 0: scale(1), steps(sibling-index()), 1: scale(2))"));
        assert!(computationally_independent("transform-interpolate(0.4 by steps(calc(1vw / 5px)), 0: scale(1), 1: scale(2))"),"viewport dimensions remain computationally independent");
    }

    #[test]
    fn specification_transform_map_progress_uses_shared_numeric_math() {
        let context=static_length_context();
        assert_eq!(transform_list("scale(progress(no-clamp 100px, 0px, 50px))",context).unwrap().as_ref(),&[Transform::Scale(2.0,2.0)]);
        assert!(transform_list("rotate(calc(progress(150px, 100px, 200px) * 90deg))",context).is_some());
        assert_eq!(computed("transform-interpolate(progress(150px, 100px, 200px), 0: scale(1), 1: scale(3))",Some).as_deref(),Some("scale(2)"));
        assert_eq!(computed("transform-interpolate(progress(1%, 0%, 100%), 0: scale(1), 1: scale(101))",Some).as_deref(),Some("scale(2)"));
        assert_eq!(computed("transform-interpolate(progress(100px, 10px, 10px), 0: scale(1), 1: scale(3))",Some).as_deref(),Some("scale(1)"));
    }

    #[test]
    fn specification_deferred_transform_map_computed_and_used_reference_boxes() {
        use registered_properties::ComputedValueOperation as Op;
        let deferred=combine_computed("translateX(50%)","scale(4)",Op::Interpolate(0.5),true,None).unwrap();
        assert!(validated_function_tokens(&deferred).is_some());
        assert!(computed_matrix(&deferred,None).is_none());
        for (width,translation) in [(200.0,50.0),(400.0,100.0)] {
            let matrix=computed_matrix(&deferred,Some([width,100.0])).unwrap();
            assert_eq!(matrix.values[0],2.5);assert_eq!(matrix.values[5],2.5);assert_eq!(matrix.values[12],translation);
        }
        assert_eq!(computed(&deferred,Some).as_deref(),Some(deferred.as_str()));
        assert_eq!(computed("transform-interpolate(50%, 0: translateX(0px), 1: translateX(20px))",Some).as_deref(),Some("translateX(10px)"));
        assert_eq!(computed("transform-interpolate(50%, 0: scale(1), 0.25: scale(2), 1: scale(5))",Some).as_deref(),Some("scale(3)"));
        assert_eq!(computed("transform-interpolate(1, 0: scale(1), 0.5 1: scale(2), 1: scale(3))",Some).as_deref(),Some("scale(3)"));
        assert_eq!(computed("transform-interpolate(-1, 0: scale(1), 1: scale(3))",Some).as_deref(),Some("scale(1)"));
        assert_eq!(computed("transform-interpolate(2, 0: scale(1), 1: scale(3))",Some).as_deref(),Some("scale(3)"));
        assert_eq!(computed("transform-interpolate(150px, 100px: scale(1), 200px: scale(3))",Some).as_deref(),Some("scale(2)"));
        assert_eq!(computed("transform-interpolate(0.5, 0: scale(1), steps(2, end), 1: scale(3))",Some).as_deref(),Some("scale(2)"));
        assert_eq!(computed("transform-interpolate(0.5 by steps(1, end), 0: scale(1), 1: scale(3))",Some).as_deref(),Some("scale(1)"));
        let nested=combine_computed(&deferred,"rotate(90deg)",Op::Interpolate(0.5),true,None).unwrap();
        assert!(computed_matrix(&nested,None).is_none());
        assert!(computed_matrix(&nested,Some([200.0,100.0])).is_some());
        for invalid in ["transform-interpolate(0.5, 0: scale(1), 1s: scale(2), 2px: scale(3))","transform-interpolate(10px, 0: scale(1), 1: scale(2))","transform-interpolate(0.5, 0: scale(1), ease)"] {
            assert!(validated_function_tokens(invalid).is_none(),"{invalid}");
        }
        assert!(combine_computed("translateX(50%)","scale(4)",Op::Interpolate(1.5),true,None).is_none(),"a filling map cannot represent animation extrapolation");
        let mut nested="translateX(50%)".to_string();
        for _ in 0..syntax::MAX_COMPONENT_DEPTH {nested=alloc::format!("transform-interpolate(0.5, 0: {}, 1: scale(4))",nested);}
        assert!(validated_function_tokens(&nested).is_none(),"canonical block depth rejects nested maps before recursive validation");
        assert!(computed(&nested,Some).is_none());assert!(computed_matrix(&nested,Some([200.0,100.0])).is_none());
        assert!(computed(&"translateX(10px) ".repeat(33),Some).is_none());
    }

    #[test]
    fn specification_registered_transform_functions_lists_padding_and_matrix_suffix() {
        use registered_properties::ComputedValueOperation as Op;
        let combine=|a:&str,b:&str,op:Op,list:bool|combine_computed(a,b,op,list,None).unwrap();
        assert_eq!(combine("translateX(100px)","translateX(200px)",Op::Interpolate(0.5),false),"translateX(150px)");
        assert_eq!(registered_properties::combine_computed_values("<transform-function>#","translateX(10px), rotate(10deg)","translateX(20px), rotate(20deg)",Op::Interpolate(0.5)).as_deref(),Some("translateX(15px), rotate(15deg)"));
        let zero=TransformLength{pixels:0.0,percent:0.0};
        assert_eq!(combine_resolved(&[Transform::Translate(TransformLength{pixels:0.0,percent:10.0},zero)],&[Transform::Translate(TransformLength{pixels:0.0,percent:20.0},zero)],Op::Interpolate(0.5),200.0,100.0).as_deref(),Some("translate(15%, 0px)"));
        assert_eq!(combine_resolved(&[],&[Transform::Scale(2.0,2.0)],Op::Interpolate(0.5),200.0,100.0).as_deref(),Some("scale(1.5, 1.5)"));
        assert_eq!(combine("translateX(10%)","translateY(20%)",Op::Interpolate(0.5),true),"translate(5%, 10%)");
        assert_eq!(combine("translateX(100px)","translateX(250px)",Op::Add,true),"translateX(100px) translateX(250px)");
        assert_eq!(combine("translateX(100px)","translateX(250px)",Op::Add,false),"translateX(350px)");
        assert_eq!(combine("scale(2)","scale(3)",Op::Accumulate(2.0),true),"scale(5)");
        assert_eq!(combine("scale(100%)","scale(calc(100% + 2))",Op::Interpolate(0.5),true),"scale(2)");
        assert_eq!(combine("translateX(10px)","translateY(20px) rotate(90deg)",Op::Interpolate(0.5),true),"translate(5px, 10px) rotate(45deg)");
        assert_eq!(combine("translateZ(10px)","translateX(20px)",Op::Interpolate(0.5),true),"translate3d(10px, 0px, 5px)");
        assert_eq!(combine("rotate3d(0, 0, 2, 90deg)","rotate3d(0, 0, 3, 180deg)",Op::Interpolate(0.5),false),"rotate3d(0, 0, 1, 135deg)");
        assert_eq!(combine("rotate3d(0, 0, 0, 90deg)","rotate3d(1, 0, 0, 90deg)",Op::Interpolate(0.5),false),"rotate3d(1, 0, 0, 45deg)");
        let result=combine("translateX(10px) rotateX(90deg)","translateX(20px) rotateY(90deg)",Op::Interpolate(0.5),true);
        assert!(result.starts_with("translateX(15px) matrix3d("),"{result}");
        assert_eq!(combine_computed("translateX(50%)","scale(4)",Op::Interpolate(0.5),true,None).as_deref(),Some("transform-interpolate(0.5, 0: translateX(50%), 1: scale(4))"),"unresolved reference dimensions stay deferred");
        assert!(combine_computed("translateX(50%)","scale(4)",Op::Interpolate(0.5),true,Some([200.0,100.0])).is_some());
        assert!(combine_computed(&"translateX(1px) ".repeat(20),&"translateX(1px) ".repeat(20),Op::Add,true,None).is_none());
    }
    #[test]
    fn specification_deferred_transform_sparse_style_computed_vs_used() {
        let source="transform-interpolate(0.5, 0: translateX(50%), 1: scale(4))";
        let mut style=Style::initial();
        style.transforms=Some(Arc::from([Transform::Matrix(Affine::IDENTITY)]));
        style.transform_origin=[TransformLength{pixels:0.0,percent:0.0};2];
        style.relative_expressions.push(RelativeExpression{source_url:None,slot:59,raw:Arc::from(source),context:static_length_context(),nonnegative:false,query_dependent:false,parent_font_pending:false,current_font_pending:false});
        assert_eq!(style.computed_css_value("transform",computed_values::ComputedValueContext::default()).as_deref(),Some(source));
        let rect=Rect{x:0.0,y:0.0,width:200.0,height:100.0};
        let resolved=style.computed_css_value("transform",computed_values::ComputedValueContext{resolved:true,border_box:Some(rect),..Default::default()}).unwrap();
        let resolved=computed_matrix(&resolved,None).unwrap();
        for (actual,expected) in resolved.values.iter().zip(lumen_common::dom_geometry::Matrix::from_affine([2.5,0.0,0.0,2.5,50.0,0.0]).values) {assert!((*actual-expected).abs()<1e-12,"{actual} vs {expected}");}
        let used=style.transform_matrix(rect).unwrap();assert_eq!(used.a,2.5);assert_eq!(used.e,50.0);
        assert!(style.checked_private_payload_bytes().is_some());
        let mut copied=Style::initial();copy_slot_state(59,&style,&mut copied,false);
        assert_eq!(copied.relative_expressions.len(),1);
        copy_slot_state(59,&Style::initial(),&mut copied,false);
        assert!(copied.transforms.is_none());assert!(copied.relative_expressions.is_empty());
        assert!(Style::initial().extras.is_none(),"unrelated styles retain no deferred source allocation");
    }

    #[test]
    fn specification_transform_map_initial_independence_uses_shared_units() {
        assert!(computationally_independent("transform-interpolate(0.5, 0: translateX(50%), 1: scale(4))"));
        assert!(computationally_independent("transform-interpolate(50vw, 10vw: scale(1), 100vw: scale(4))"));
        assert!(!computationally_independent("transform-interpolate(1em, 10px: scale(1), 100px: scale(4))"));
        assert!(!computationally_independent("transform-interpolate(0.5, 0: translateX(1em), 1: scale(4))"));
    }

    #[test]
    fn specification_typed_transforms_token_boundaries_escapes_and_bounded_arguments() {
        let input=r"/*leading*/ tr\61 nslate(calc(1px + 1em), 10%)/*between*/rotate(1turn)";
        let functions=function_tokens(input).unwrap();
        assert_eq!(functions.len(),2);
        assert_eq!(functions[0].name,"translate");
        assert_eq!(functions[0].arguments,["calc(1px + 1em)","10%"]);
        assert_eq!(functions[1].name,"rotate");
        for invalid in ["translate(1px", "translate()", "translate(1px,)", "rotate(1deg)junk", "/*unterminated"] {
            assert!(function_tokens(invalid).is_none(),"{invalid}");
        }
        assert!(function_tokens(&"rotate(1deg)".repeat(33)).is_none());
    }
}

fn matched_rotation_arguments(mut a:[f64;4],mut b:[f64;4])->Option<([f64;4],[f64;4])>{
    let axis=|value:[f64;4]|{let length=libm::sqrt(value[0]*value[0]+value[1]*value[1]+value[2]*value[2]);if length==0.0{[0.0;3]}else{[value[0]/length,value[1]/length,value[2]/length]}};
    let aa=axis(a);let bb=axis(b);
    if aa==[0.0;3]{a[3]=0.0;}if bb==[0.0;3]{b[3]=0.0;}
    if aa!=bb&&a[3]!=0.0&&b[3]!=0.0{return None;}
    let chosen=if a[3]!=0.0{aa}else if b[3]!=0.0{bb}else{[0.0,0.0,1.0]};
    a[..3].copy_from_slice(&chosen);b[..3].copy_from_slice(&chosen);Some((a,b))
}
/// Individual rotate and transform rotate3d use the same primitive matching
/// and maintained homogeneous quaternion algorithms. Resolved individual
/// endpoints take the fixed-size numeric path without string round trips.
pub(super) fn combine_rotation_arguments(a:[f64;4],b:[f64;4],operation:registered_properties::ComputedValueOperation)->Option<[f64;4]>{
    use registered_properties::ComputedValueOperation as Op;
    if let Some((a,b))=matched_rotation_arguments(a,b){return Some([a[0],a[1],a[2],combine_scalar(a[3],b[3],operation,0.0)?]);}
    let matrix=|v:[f64;4]|lumen_common::dom_geometry::Matrix::default().rotated_axis(v[0],v[1],v[2],v[3]);
    let left=matrix(a);let right=matrix(b);
    match operation{Op::Interpolate(p)=>left.interpolate_rotation(right,p),Op::Add=>left.accumulated_rotation(right,1.0),Op::Accumulate(n)=>left.accumulated_rotation(right,n)}
}
