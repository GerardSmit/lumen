//! CSS syntax adapter for the shared, unclipped color authority.
use super::*;
use lumen_common::color::{Color, ColorSpace, HueInterpolation, InterpolationMethod, PreparedPair};

pub(super) fn from_rgba(value:Rgba)->Color { Color::rgba8([value.r,value.g,value.b,value.a]) }
pub(super) fn rgba(value:Color)->Rgba { let [r,g,b,a]=value.to_rgba8(); Rgba{r,g,b,a} }
pub(super) fn identifier(raw:&str)->Option<alloc::borrow::Cow<'_,str>> {
    if !raw.contains('\\') {return Some(ascii_lower(raw));}
    let mut end=0; let name=consume_selector_identifier(raw,&mut end)?;
    (end==raw.len()).then(||alloc::borrow::Cow::Owned(name.to_ascii_lowercase()))
}
fn missing(raw:&str)->bool { decoded_css_keyword(raw,"none") }
fn is_percentage(raw:&str)->Option<bool> {
    let kind=typed_numeric::parse_numeric_expression(raw)?.numeric_type()?;
    if kind==typed_numeric::NumericType::from_unit(typed_numeric::NumericUnit::Percent) {Some(true)}
    else if kind==typed_numeric::NumericType::from_unit(typed_numeric::NumericUnit::Number) {Some(false)}
    else {None}
}
fn number(raw:&str,percentage_scale:f32,context:LengthContext,query:ContainerUnitContext)->Option<f32> {
    if missing(raw) {return Some(0.0);}
    let result=css_scalar_with_percentage_basis(raw,true,context,query,percentage_scale)?.0;
    result.is_finite().then_some(result)
}
/// Absolute Color 4 functions retain source coordinates. Named/hex/system and
/// existing relative-color syntax are handled by the established color grammar.
pub(super) fn absolute(raw:&str)->Option<Color> {
    let context=static_length_context();absolute_with_context(raw,context,ContainerUnitContext::no_container(context.viewport))
}
fn hue(raw:&str,context:LengthContext,query:ContainerUnitContext)->Option<f32> {
    if missing(raw) {return Some(0.0);}
    let expression=typed_numeric::parse_numeric_expression(raw)?;
    let degrees=if expression.numeric_type()?==typed_numeric::NumericType::from_unit(typed_numeric::NumericUnit::Number) {
        css_scalar_with_length_context(raw,false,context,query)?.0
    } else {conic_angle_value(raw,false,context,query)?.1};
    Some((degrees/360.0).rem_euclid(1.0))
}
pub(super) fn absolute_with_context(raw:&str,context:LengthContext,query:ContainerUnitContext)->Option<Color> {
    let (name,body)=raw.trim().split_once('(')?; let name=identifier(name)?;
    let body=body.strip_suffix(')')?;
    if components(body)?.first().is_some_and(|value|decoded_css_keyword(value,"from")) {return None;}
    let mut parts=color_components(body)?;
    let space=match name.as_ref() {
        "rgb" | "rgba"=>ColorSpace::Srgb,"hsl" | "hsla"=>ColorSpace::Hsl,"hwb"=>ColorSpace::Hwb,
        "lab"=>ColorSpace::Lab,"lch"=>ColorSpace::Lch,"oklab"=>ColorSpace::Oklab,"oklch"=>ColorSpace::Oklch,
        "color"=> {let profile=identifier(parts.first()?)?; let space=ColorSpace::named(profile.as_ref())?;
            if matches!(space,ColorSpace::Lab|ColorSpace::Lch|ColorSpace::Oklab|ColorSpace::Oklch|ColorSpace::Hsl|ColorSpace::Hwb) {return None;}
            parts.remove(0);space},
        _=>return None,
    };
    if !(3..=4).contains(&parts.len()) {return None;}
    let legacy=top_level_split(body,b',',4)?.len()>1;
    if !legacy && (parts.len()==4)!=(top_level_split(body,b'/',2)?.len()==2) {return None;}
    if legacy && (!matches!(name.as_ref(),"rgb"|"rgba"|"hsl"|"hsla") || parts.iter().any(|v|missing(v))) {return None;}
    if legacy && space==ColorSpace::Hsl && parts[1..3].iter().any(|value|is_percentage(value)!=Some(true)) {return None;}
    if legacy && matches!(space,ColorSpace::Srgb) {
        let percentage=is_percentage(parts[0])?;
        if parts[1..3].iter().any(|value|is_percentage(value)!=Some(percentage)) {return None;}
    }
    let mut values=[0.0;3]; let mut flags=0;
    for i in 0..3 {
        if missing(parts[i]) {flags|=1<<i;}
        values[i]=if space.hue()==Some(i) {hue(parts[i],context,query)?} else {
            let scale=match space {
                ColorSpace::Srgb if name.as_ref()!="color"=>255.0,
                ColorSpace::Hsl | ColorSpace::Hwb=>100.0,
                ColorSpace::Lab if i==0=>100.0,ColorSpace::Lab=>125.0,
                ColorSpace::Lch if i==0=>100.0,ColorSpace::Lch=>150.0,
                ColorSpace::Oklab | ColorSpace::Oklch if i==0=>1.0,
                ColorSpace::Oklab | ColorSpace::Oklch=>0.4,
                _=>1.0,
            };
            let value=number(parts[i],scale,context,query)?;
            match space {
                ColorSpace::Srgb if name.as_ref()!="color"=>value.clamp(0.0,255.0)/255.0,
                ColorSpace::Hsl if i==2=>value/100.0,
                ColorSpace::Hsl | ColorSpace::Hwb=>value.max(0.0)/100.0,
                ColorSpace::Lab | ColorSpace::Lch if i==0=>value.clamp(0.0,100.0),
                ColorSpace::Oklab | ColorSpace::Oklch if i==0=>value.clamp(0.0,1.0),
                ColorSpace::Lch | ColorSpace::Oklch if i==1=>value.max(0.0),
                _=>value,
            }
        };
    }
    let alpha=if let Some(raw)=parts.get(3) {if missing(raw) {flags|=8;} number(raw,1.0,context,query)?.clamp(0.0,1.0)} else {1.0};
    Some(Color::new(space,values,alpha,flags))
}
/// Resolve a color origin without a quantized intermediate. Context is supplied
/// by the actual cascade or sampling caller, not reconstructed from text.
pub(super) fn endpoint_with_current(raw:&str,depth:u8,current:Color,context:LengthContext,query:ContainerUnitContext)->Option<Color> {
    if depth>=8 {return None;}
    let raw=raw.trim();
    if decoded_css_keyword(raw,"currentcolor") {return Some(current);}
    if let Some(value)=identifier(raw).and_then(|name|color_scheme::system_color(&name,context.viewport.color_schemes.page().scheme)) {return Some(from_rgba(value));}
    if let Some(value)=absolute_with_context(raw,context,query) {return Some(value);}
    if let Some((name,body))=raw.split_once('(') {
        let name=identifier(name)?;let body=body.strip_suffix(')')?;
        if name=="color-mix" {return mix_with_current(body,depth+1,current,context,query);}
        if name=="alpha" {
            let parts=top_level_split(body,b'/',2)?;let origin=components(parts[0])?;
            let [from,color]=origin.as_slice() else {return None;};
            if !decoded_css_keyword(from,"from") {return None;}
            let mut source=endpoint_with_current(color,depth+1,current,context,query)?;
            if let Some(alpha)=parts.get(1) {
                if missing(alpha.trim()) {source.alpha=0.0;source.missing|=8;}
                else if !decoded_css_keyword(alpha.trim(),"alpha") {
                    let named=[("alpha",typed_numeric::NumericValue{value:f64::from(if source.missing&8!=0 {0.0}else {source.alpha}),unit:typed_numeric::NumericUnit::Number})];
                    source.alpha=channel_expression(alpha.trim(),&named,1.0,context,query)?.clamp(0.0,1.0);source.missing&=!8;
                }
            }
            return Some(source);
        }
        if name=="light-dark" {
            let parts=top_level_split(body,b',',2)?;let [light,dark]=parts.as_slice() else{return None;};
            let light=endpoint_with_current(light,depth+1,current,context,query)?;
            let dark=endpoint_with_current(dark,depth+1,current,context,query)?;
            return Some(if context.viewport.color_schemes.page().scheme==UsedColorScheme::Dark {dark}else{light});
        }
        if components(body)?.first().is_some_and(|part|decoded_css_keyword(part,"from")) {
            return relative(body,name.as_ref(),depth+1,current,context,query);
        }
    }
    Some(from_rgba(color_depth(raw,depth)?))
}
fn weighted_color<'a>(tokens:&[&'a str])->Option<(&'a str,Option<&'a str>)> {
    match tokens {
        [color]=>Some((*color,None)),
        [a,b] if is_percentage(a)==Some(true)=>Some((*b,Some(*a))),
        [a,b] if is_percentage(b)==Some(true)=>Some((*a,Some(*b))),
        _=>None,
    }
}
fn mix_with_current(body:&str,depth:u8,current:Color,context:LengthContext,query:ContainerUnitContext)->Option<Color> {
    if depth>=8 {return None;}
    let parts=top_level_split(body,b',',65)?;
    let first=components(parts.first()?)?;
    let (method,start)=if first.first().is_some_and(|token|decoded_css_keyword(token,"in")) {
        let (method,end)=method(&first,0)?;if end!=first.len() {return None;}(method,1)
    } else {(InterpolationMethod::default(),0)};
    if parts.len()==start || parts.len()-start>64 {return None;}
    let mut items=Vec::new();items.try_reserve(parts.len()-start).ok()?;
    let mut specified=0.0f64;let mut omitted=0;
    for raw in &parts[start..] {
        let tokens=components(raw)?;
        let (color,weight)=weighted_color(&tokens)?;
        let weight=if let Some(raw)=weight {Some(number(raw,1.0,context,query)?)}else {None};
        if let Some(weight)=weight {if !(0.0..=1.0).contains(&weight) {return None;}specified+=f64::from(weight);}
        else {omitted+=1;}
        items.push((endpoint_with_current(color,depth,current,context,query)?,weight));
    }
    let missing=if omitted==0 {0.0}else {((1.0-specified.min(1.0))/f64::from(omitted)) as f32};
    let total=specified+f64::from(missing)*f64::from(omitted);
    let alpha_multiplier=total.min(1.0) as f32;
    let mut iter=items.into_iter();let (mut color,weight)=iter.next()?;
    let mut accumulated=f64::from(weight.unwrap_or(missing));
    if iter.len()==0 {color=color.to(method.space);}
    else {for (next,weight) in iter {
        let weight=f64::from(weight.unwrap_or(missing));let combined=accumulated+weight;
        // CSS Color5 §3.3 specifies this ordered stack reduction; polar
        // interpolation is deliberately order dependent. Zero weights use .5.
        color=PreparedPair::new(color,next,method).sample(if combined>0.0 {(weight/combined) as f32}else {0.5});
        accumulated=combined;
    }}
    color.alpha*=alpha_multiplier;
    Some(if matches!(color.space,ColorSpace::Hsl|ColorSpace::Hwb) && color.missing==0 {color.to(ColorSpace::Srgb)}else {color})
}
fn channel_expression(raw:&str,names:&[(&str,typed_numeric::NumericValue)],scale:f32,context:LengthContext,query:ContainerUnitContext)->Option<f32> {
    let mut values=LengthValueBuilder {query,context:Some(LengthContext{percent:Some(scale),..context}),
        allow_viewport:true,scalar:true,sign_input_depth:0,context_dependent:false};
    let mut builder=typed_numeric::NamedNumericBuilder{inner:&mut values,values:names};
    let (value,_)=typed_numeric::parse_numeric_expression_with(raw,&mut builder)?;
    value.is_finite().then_some(value)
}
fn relative(body:&str,name:&str,depth:u8,current:Color,context:LengthContext,query:ContainerUnitContext)->Option<Color> {
    let parts=top_level_split(body,b'/',2)?;let head=components(parts[0])?;
    if head.len()<5 || !decoded_css_keyword(head[0],"from") {return None;}
    let origin=endpoint_with_current(head[1],depth,current,context,query)?;
    let (space,start,rgb_units)=match name {
        "rgb"|"rgba"=>(ColorSpace::Srgb,2,true),"hsl"|"hsla"=>(ColorSpace::Hsl,2,false),
        "hwb"=>(ColorSpace::Hwb,2,false),"lab"=>(ColorSpace::Lab,2,false),"lch"=>(ColorSpace::Lch,2,false),
        "oklab"=>(ColorSpace::Oklab,2,false),"oklch"=>(ColorSpace::Oklch,2,false),
        "color"=>{let space=ColorSpace::named(identifier(head[2])?.as_ref())?;
            if matches!(space,ColorSpace::Lab|ColorSpace::Lch|ColorSpace::Oklab|ColorSpace::Oklch|ColorSpace::Hsl|ColorSpace::Hwb) {return None;}
            (space,3,false)},_=>return None,
    };
    if head.len()!=start+3 {return None;}
    let source=origin.to_with_missing(space);
    let keywords=match space {ColorSpace::Hsl=>["h","s","l"],ColorSpace::Hwb=>["h","w","b"],
        ColorSpace::Lab|ColorSpace::Oklab=>["l","a","b"],ColorSpace::Lch|ColorSpace::Oklch=>["l","c","h"],
        ColorSpace::XyzD65|ColorSpace::XyzD50=>["x","y","z"],_=>["r","g","b"]};
    let scale=|index:usize|if rgb_units {255.0} else {match space {
        ColorSpace::Hsl|ColorSpace::Hwb if index==0=>360.0,ColorSpace::Hsl|ColorSpace::Hwb=>100.0,
        ColorSpace::Lab if index==0=>100.0,ColorSpace::Lab=>125.0,
        ColorSpace::Lch if index==0=>100.0,ColorSpace::Lch if index==1=>150.0,
        ColorSpace::Lch|ColorSpace::Oklch if index==2=>360.0,
        ColorSpace::Oklab|ColorSpace::Oklch if index==0=>1.0,ColorSpace::Oklab|ColorSpace::Oklch=>0.4,_=>1.0}};
    let multiplier=|index:usize|if rgb_units {255.0} else if space.hue()==Some(index) {360.0}
        else if matches!(space,ColorSpace::Hsl|ColorSpace::Hwb) {100.0} else {1.0};
    let named=core::array::from_fn::<_,4,_>(|index|if index==3 {("alpha",typed_numeric::NumericValue{value:f64::from(if source.missing&8!=0 {0.0}else {source.alpha}),unit:typed_numeric::NumericUnit::Number})}
        else {(keywords[index],typed_numeric::NumericValue{value:f64::from(if source.missing&(1<<index)!=0 {0.0}else {source.components[index]*multiplier(index)}),unit:typed_numeric::NumericUnit::Number})});
    let mut coordinates=[0.0;3];let mut mask=0;
    for index in 0..3 {
        let raw=head[start+index];
        let direct=identifier(raw).and_then(|name|keywords.iter().position(|key|name==*key));
        if missing(raw) {mask|=1<<index;}
        else if let Some(source_index)=direct {coordinates[index]=source.components[source_index]*multiplier(source_index)/multiplier(index);
            if source.missing&(1<<source_index)!=0 {mask|=1<<index;}}
        else {coordinates[index]=if let Some(value)=channel_expression(raw,&named,scale(index),context,query) {value/multiplier(index)}
            else if space.hue()==Some(index) {hue(raw,context,query)?} else {return None;};}
        if space.hue()==Some(index) {coordinates[index]=coordinates[index].rem_euclid(1.0);}
    }
    let alpha=if let Some(raw)=parts.get(1) {let raw=raw.trim();
        if missing(raw) {mask|=8;0.0} else if decoded_css_keyword(raw,"alpha") {mask|=source.missing&8;source.alpha}
        else {channel_expression(raw,&named,1.0,context,query)?.clamp(0.0,1.0)}
    } else {mask|=source.missing&8;source.alpha};
    let color=Color::new(space,coordinates,alpha.clamp(0.0,1.0),mask);
    // Color5 preserves HSL/HWB notation when a component is missing; otherwise
    // their relative results use unclipped sRGB coordinates for round-tripping.
    Some(if matches!(space,ColorSpace::Hsl|ColorSpace::Hwb) && mask==0 {color.to(ColorSpace::Srgb)}else {color})
}
pub(super) fn endpoint(raw:&str,depth:u8)->Option<Color> {
    let context=static_length_context();endpoint_with_context(raw,depth,context,ContainerUnitContext::no_container(context.viewport))
}
pub(super) fn endpoint_with_context(raw:&str,depth:u8,context:LengthContext,query:ContainerUnitContext)->Option<Color> {
    endpoint_with_current(raw,depth,from_rgba(Style::initial().color),context,query)
}
pub(super) fn method(tokens:&[&str],index:usize)->Option<(InterpolationMethod,usize)> {
    if !decoded_css_keyword(tokens.get(index)?,"in") {return None;}
    let space=ColorSpace::named(identifier(tokens.get(index+1)?)?.as_ref())?;
    let mut result=InterpolationMethod{space,hue:HueInterpolation::Shorter};let mut end=index+2;
    if let Some(raw)=tokens.get(end) {
        result.hue=match identifier(raw)?.as_ref() {
            "shorter"=>HueInterpolation::Shorter,"longer"=>HueInterpolation::Longer,
            "increasing"=>HueInterpolation::Increasing,"decreasing"=>HueInterpolation::Decreasing,
            _=>return Some((result,end)),
        };
        if space.hue().is_none() || !decoded_css_keyword(tokens.get(end+1)?,"hue") {return None;}
        end+=2;
    }
    Some((result,end))
}
pub(super) fn parse_mix(raw:&str,depth:u8)->Option<Color> {
    let context=static_length_context();
    let (_,body)=raw.split_once('(')?;
    mix_with_current(body.strip_suffix(')')?,depth,from_rgba(Style::initial().color),context,ContainerUnitContext::no_container(context.viewport))
}

fn serialize_channel(raw:&str,names:&[&'static str],computed:Option<(LengthContext,ContainerUnitContext)>)->Option<String> {
    if missing(raw) {return Some("none".into());}
    let mut expression=typed_numeric::parse_channel_expression(raw,names)?;
    if let Some((context,query))=computed {
        let mut evaluation=super::FontAngleContext{length:Some(context),query,percent_scale:1.0};
        expression.map_numeric_values(|value| {
            if value.unit==typed_numeric::NumericUnit::Percent {return Some(value);}
            if let Some((unit,factor))=value.unit.canonical_unit_and_factor() {
                let value=value.value*factor;return value.is_finite().then_some(typed_numeric::NumericValue{value,unit});
            }
            let value=typed_numeric::NumericExpressionContext::unit(&mut evaluation,value)?;
            value.is_finite().then_some(typed_numeric::NumericValue{value,unit:typed_numeric::NumericUnit::Px})
        })?;
    }
    if computed.is_none() {
        expression.map_numeric_values(|value| {
            if let Some((unit,factor))=value.unit.canonical_unit_and_factor() {
                let value=value.value*factor;value.is_finite().then_some(typed_numeric::NumericValue{value,unit})
            }else {Some(value)}
        })?;
    }
    expression.simplify_absolute_units();
    expression.serialize()
}
/// Declared and unresolved-currentColor forms retain their canonical structure.
/// Colors and channel math share the existing grammar and serializers.
pub(super) fn serialize_declared(raw:&str,depth:u8)->Option<String> {
    serialize_source(raw,depth,None)
}
/// Gradient/shadow declared endpoints preserve authored, unclipped channels.
pub(super) fn serialize_declared_endpoint(raw:&str)->Option<String> {
    serialize_origin(raw,0,None)
}
pub(super) fn serialize_computed(raw:&str,context:LengthContext,query:ContainerUnitContext)->Option<String> {
    serialize_source(raw,0,Some((context,query)))
}
fn serialize_source(raw:&str,depth:u8,computed:Option<(LengthContext,ContainerUnitContext)>)->Option<String> {
    if raw.len()>super::MAX_CSS_BYTES {return None;}
    if let Some((context,_))=computed {
        if let Some((name,body))=raw.trim().split_once('(') {
            if identifier(name).is_some_and(|name|name=="light-dark") {
                let parts=top_level_split(body.strip_suffix(')')?,b',',2)?;
                let [light,dark]=parts.as_slice() else{return None;};
                return serialize_source(if context.viewport.color_schemes.page().scheme==UsedColorScheme::Dark {dark}else{light},depth+1,computed);
            }
        }
    }
    if let Some((context,query))=computed.filter(|_|!uses_current_color(raw)) {
        return serialize(endpoint_with_current(raw,depth,from_rgba(super::Style::initial().color),context,query)?,!legacy(raw));
    }
    serialize_source_inner(raw,depth,computed).filter(|value|value.len()<=super::MAX_CSS_BYTES)
}
fn serialize_origin(raw:&str,depth:u8,computed:Option<(LengthContext,ContainerUnitContext)>)->Option<String> {
    if computed.is_some() {return serialize_source(raw,depth,computed);}
    let Some((name,body))=raw.trim().split_once('(') else{return serialize_source(raw,depth,None);};
    let name=identifier(name)?;
    let canonical=match name.as_ref() {"rgba"=>"rgb","hsla"=>"hsl","rgb"|"hsl"|"hwb"|"lab"|"lch"|"oklab"|"oklch"|"color"=>name.as_ref(),_=>return serialize_source(raw,depth,None)};
    let body=body.strip_suffix(')')?;
    if components(body)?.first().is_some_and(|token|decoded_css_keyword(token,"from")) {return serialize_source(raw,depth,None);}
    endpoint(raw,depth)?;
    let mut values=color_components(body)?;
    let mut result=alloc::format!("{canonical}(");
    if canonical=="color" {
        let profile=ColorSpace::named(identifier(values.first()?)?.as_ref())?;
        result.push_str(profile.name());result.push(' ');values.remove(0);
    }
    if !(3..=4).contains(&values.len()) {return None;}
    for (index,value) in values.iter().enumerate() {
        if index!=0 {result.push_str(if index==3 {" / "}else {" "});}
        result.push_str(&serialize_channel(value,&[],None)?);
    }
    result.push(')');Some(result)
}
fn serialize_source_inner(raw:&str,depth:u8,computed:Option<(LengthContext,ContainerUnitContext)>)->Option<String> {
    if depth>=8 {return None;}
    let raw=raw.trim();
    if decoded_css_keyword(raw,"currentcolor") {return Some("currentcolor".into());}
    let Some((name,body))=raw.split_once('(') else {
        let value=endpoint(raw,depth)?;
        // CSS Color 4 §16.2 preserves authored named keywords, while hex
        // colors use the shared CSS sRGB serialization for declared values.
        return if raw.starts_with('#') {Some(super::computed_values::color(rgba(value)))}else {Some(identifier(raw)?.into_owned())};
    };
    let name=identifier(name)?;let body=body.strip_suffix(')')?;
    if name=="color-mix" {
        let parts=top_level_split(body,b',',65)?;let first=components(parts.first()?)?;
        let (method,start)=if first.first().is_some_and(|raw|decoded_css_keyword(raw,"in")) {
            let (method,end)=method(&first,0)?;if end!=first.len(){return None;}(method,1)
        }else{(InterpolationMethod::default(),0)};
        if parts.len()==start || parts.len()-start>64 {return None;}
        let mut arguments=Vec::new();arguments.try_reserve(parts.len()-start).ok()?;
        let mut sum=0.0f64;let mut omitted=0;let mut unknown=false;
        for part in &parts[start..] {
            let tokens=components(part)?;let (color,weight)=weighted_color(&tokens)?;
            let value=if let Some(weight)=weight {
                let value=typed_numeric::parse_numeric_value(weight).filter(|value|value.unit==typed_numeric::NumericUnit::Percent);
                if let Some(value)=value {sum+=value.value;Some(value.value)}else {unknown=true;None}
            }else {omitted+=1;None};
            arguments.push((color,weight,value));
        }
        let omitted_weight=if omitted==0 {0.0}else {(100.0-sum.min(100.0))/f64::from(omitted)};
        let equal=arguments.first().map(|(_,_,weight)|weight.unwrap_or(omitted_weight))?;
        let omit_weights=!unknown && equal>=100.0/arguments.len() as f64 && arguments.iter().all(|(_,_,weight)|weight.unwrap_or(omitted_weight)==equal);
        let mut result=String::from("color-mix(");
        if method.space!=ColorSpace::Oklab {
            result.push_str("in ");result.push_str(method.space.name());
            if method.hue!=HueInterpolation::Shorter {result.push(' ');result.push_str(match method.hue {HueInterpolation::Longer=>"longer hue",HueInterpolation::Increasing=>"increasing hue",HueInterpolation::Decreasing=>"decreasing hue",HueInterpolation::Shorter=>unreachable!()});}
            result.push_str(", ");
        }
        for (index,(color,weight,value)) in arguments.into_iter().enumerate() {
            if index!=0 {result.push_str(", ");}
            result.push_str(&serialize_source(color,depth+1,computed)?);
            if !omit_weights {
                if let Some(raw)=weight {result.push(' ');result.push_str(&serialize_channel(raw,&[],computed)?);}
                else if !unknown {result.push(' ');result.push_str(&typed_numeric::serialize_numeric_value(value.unwrap_or(omitted_weight),typed_numeric::NumericUnit::Percent));}
            }
        }
        result.push(')');return Some(result);
    }
    let head_parts=top_level_split(body,b'/',2)?;let head=components(head_parts[0])?;
    if head.first().is_some_and(|part|decoded_css_keyword(part,"from")) {
        let origin=*head.get(1)?;
        let (name,start,channels):(&str,usize,&[&'static str])=match name.as_ref() {
            "rgb"|"rgba"=>("rgb",2,&["r","g","b","alpha"]),"hsl"|"hsla"=>("hsl",2,&["h","s","l","alpha"]),
            "hwb"=>("hwb",2,&["h","w","b","alpha"]),"lab"|"oklab"=>(name.as_ref(),2,&["l","a","b","alpha"]),
            "lch"|"oklch"=>(name.as_ref(),2,&["l","c","h","alpha"]),"color"=>{
                let profile=ColorSpace::named(identifier(head.get(2)?)?.as_ref())?;
                ("color",3,if matches!(profile,ColorSpace::XyzD65|ColorSpace::XyzD50) {&["x","y","z","alpha"]}else {&["r","g","b","alpha"]})
            },"alpha"=>("alpha",2,&["alpha"]),_=>return None,
        };
        if head.len()!=start+usize::from(name!="alpha")*3 {return None;}
        if name=="alpha" && !uses_current_color(raw) && super::image_numeric_dependencies(raw)==(false,false) {
            return serialize(endpoint(raw,depth)?,true);
        }
        let mut result=alloc::format!("{name}(from {}",serialize_origin(origin,depth+1,computed)?);
        if name=="color" {result.push(' ');result.push_str(ColorSpace::named(identifier(head[2])?.as_ref())?.name());}
        for channel in &head[start..] {result.push(' ');result.push_str(&serialize_channel(channel,channels,computed)?);}
        if let Some(alpha)=head_parts.get(1) {
            // Relative color's omitted alpha inherits the origin; explicit
            // unity is observable for a translucent or live currentColor origin.
            result.push_str(" / ");result.push_str(&serialize_channel(alpha.trim(),channels,computed)?);
        }
        result.push(')');return Some(result);
    }
    if name=="light-dark" {
        let parts=top_level_split(body,b',',2)?;let [light,dark]=parts.as_slice() else{return None;};
        if let Some((context,_))=computed {
            return serialize_source(if context.viewport.color_schemes.page().scheme==UsedColorScheme::Dark {dark}else{light},depth+1,computed);
        }
        return Some(alloc::format!("light-dark({}, {})",serialize_source(light,depth+1,computed)?,serialize_source(dark,depth+1,computed)?));
    }
    serialize(endpoint(raw,depth)?,!legacy(raw))
}

pub(super) fn take_method(tokens:&mut Vec<&str>)->Option<(InterpolationMethod,bool)> {
    let Some(index)=tokens.iter().position(|token|decoded_css_keyword(token,"in")) else {return Some((InterpolationMethod::default(),false));};
    let (method,end)=method(tokens,index)?;
    // The interpolation production is unordered with the complete geometry
    // production; it cannot split a direction, angle or position production.
    if index!=0 && end!=tokens.len() {return None;}
    tokens.drain(index..end);
    if tokens.iter().any(|token|decoded_css_keyword(token,"in")) {return None;}
    Some((method,true))
}
pub(super) fn retain_color(stops:&[GradientStop],colors:&mut Option<Vec<Color>>,endpoint:Color)->Option<()> {
    if let Some(colors)=colors {colors.try_reserve(1).ok()?;colors.push(endpoint);}
    else if endpoint!=from_rgba(stops.last()?.color) {
        let mut values=Vec::new();values.try_reserve(stops.len()).ok()?;
        values.extend(stops.iter().map(|stop|from_rgba(stop.color)));
        *values.last_mut()?=endpoint;*colors=Some(values);
    }
    Some(())
}
pub(super) fn metadata(method:InterpolationMethod,colors:Option<Vec<Color>>,hints:Vec<(usize,GradientPosition)>,color_functions:u32,single_stop:bool)->Option<Arc<crate::paint::GradientColorMetadata>> {
    if method.space==ColorSpace::Srgb && method.hue==HueInterpolation::Shorter && colors.is_none() && hints.is_empty() && color_functions==0 && !single_stop {return None;}
    Some(Arc::new(crate::paint::GradientColorMetadata{method,colors:colors.unwrap_or_default().into_boxed_slice(),hints:hints.into_boxed_slice(),color_functions,single_stop}))
}

pub(super) fn structured(raw:&str)->bool {
    let Some((name,body))=raw.split_once('(') else{return false;};
    identifier(name).is_some_and(|name|matches!(name.as_ref(),"color-mix"|"alpha"|"light-dark")) ||
        components(body.strip_suffix(')').unwrap_or(body)).is_some_and(|parts|parts.first().is_some_and(|part|decoded_css_keyword(part,"from")))
}
pub(super) fn uses_current_color(raw:&str)->bool {
    let Ok(mut cursor)=syntax::Cursor::new(raw,0) else {return false;};
    while let Some(token)=cursor.next() {
        if token.kind==syntax::TokenKind::Other && decoded_css_keyword(&raw[token.start..token.end],"currentcolor") {return true;}
    }
    false
}
pub(super) fn scheme_dependent(raw:&str)->bool {
    raw.contains("light-dark") || identifier(raw).is_some_and(|name|color_scheme::system_color(&name,UsedColorScheme::Light).is_some()) || raw.split_once('(').is_some_and(|(name,_)|identifier(name).is_some_and(|name|name=="light-dark"))
}
pub(super) fn color_function_in_scheme(raw:&str,scheme:UsedColorScheme,current:bool)->bool {
    fn selected(raw:&str,scheme:UsedColorScheme,current:bool,depth:u8)->bool {
        if depth>=8{return true;}
        if decoded_css_keyword(raw,"currentcolor"){return current;}
        if let Some((name,body))=raw.split_once('(') {
            if identifier(name).is_some_and(|name|name=="light-dark") {
                if let Some(parts)=body.strip_suffix(')').and_then(|body|top_level_split(body,b',',2)) {
                    if parts.len()==2{return selected(parts[usize::from(scheme==UsedColorScheme::Dark)],scheme,current,depth+1);}
                }
            }
        }
        !legacy(raw)
    }
    selected(raw,scheme,current,0)
}
pub(super) fn legacy(raw:&str)->bool {
    let Some((name,body))=raw.split_once('(') else {return true;};
    identifier(name).is_some_and(|name|matches!(name.as_ref(),"rgb"|"rgba"|"hsl"|"hsla"|"hwb"))
        && !components(body.strip_suffix(')').unwrap_or(body)).is_some_and(|parts|parts.first().is_some_and(|part|decoded_css_keyword(part,"from")))
}
pub(super) fn default_method(method:InterpolationMethod,explicit:bool,legacy:bool)->InterpolationMethod {
    if !explicit && legacy {InterpolationMethod{space:ColorSpace::Srgb,hue:HueInterpolation::Shorter}} else {method}
}
pub(super) fn serialize(value:Color,color_function:bool)->Option<String> {
    use super::computed_values::number;
    if !value.is_finite() {return None;}
    if matches!(value.space,ColorSpace::Srgb|ColorSpace::Hsl|ColorSpace::Hwb) && value.missing==0 && !color_function {
        let rgb=value.srgb();
        let channels=rgb.map(|v|number(v.clamp(0.0,1.0)*255.0));
        return Some(if value.alpha==1.0 {alloc::format!("rgb({}, {}, {})",channels[0],channels[1],channels[2])}
            else {alloc::format!("rgba({}, {}, {}, {})",channels[0],channels[1],channels[2],number(value.alpha))});
    }
    let direct=matches!(value.space,ColorSpace::Lab|ColorSpace::Lch|ColorSpace::Oklab|ColorSpace::Oklch|ColorSpace::Hsl|ColorSpace::Hwb);
    let mut result=if direct {alloc::format!("{}(",value.space.name())} else {alloc::format!("color({} ",value.space.name())};
    for index in 0..3 {
        if index!=0 {result.push(' ');}
        if value.missing&(1<<index)!=0 {result.push_str("none");continue;}
        let mut component=value.components[index];
        if value.space.hue()==Some(index) {component*=360.0;}
        if matches!(value.space,ColorSpace::Hsl|ColorSpace::Hwb) && index!=0 {component*=100.0;}
        result.push_str(&number(component));
        if matches!(value.space,ColorSpace::Hsl|ColorSpace::Hwb) && index!=0 {result.push('%');}
    }
    if value.alpha!=1.0 || value.missing&8!=0 {
        result.push_str(" / ");if value.missing&8!=0 {result.push_str("none");} else {result.push_str(&number(value.alpha));}
    }
    result.push(')');Some(result)
}

pub(super) fn function<'a>(raw:&'a str,expected:&str)->Option<(&'a str,bool)> {
    let (name,body)=raw.trim().split_once('(')?;let name=identifier(name)?;
    let repeating=if name==expected {false} else if name.strip_prefix("repeating-")==Some(expected) {true} else {return None;};
    Some((body.strip_suffix(')')?,repeating))
}
