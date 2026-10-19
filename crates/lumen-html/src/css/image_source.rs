//! Declared image serialization consumes the existing bounded component and
//! numeric grammar. It does not resolve font/container lengths or paint colors.
use super::*;
use lumen_common::color::{ColorSpace,HueInterpolation,InterpolationMethod};

#[derive(Clone, Copy)]
struct Computation<'a> { context: LengthContext, query: ContainerUnitContext, current: &'a SourceColor, base: Option<&'a str>, independent: bool, preserve_current: bool }

fn numeric_expression(raw:&str,computed:Option<Computation<'_>>)->Option<typed_numeric::NumericExpression>{
    let mut value=typed_numeric::parse_numeric_expression(raw)?;
    // Fold source-unit identities before computed leaf conversion introduces
    // independently rounded canonical factors; contextual leaves stay retained.
    value.simplify_absolute_units();
    // CSSOM angle components use the canonical angle unit even in declared
    // values. Keep contextual lengths and percentages in their source units.
    value.map_numeric_values(|mut leaf| {
        if let Some(computed) = computed {
            if computed.independent {if !typed_numeric::computationally_independent_unit(leaf.unit){return None;}}
            else {return typed_numeric::computed_numeric_value(leaf,computed.context,computed.query);}
        }
        if let Some((typed_numeric::NumericUnit::Deg,factor))=leaf.unit.canonical_unit_and_factor() {
            leaf.unit=typed_numeric::NumericUnit::Deg;leaf.value*=factor;
            if !leaf.value.is_finite(){return None;}
        }
        Some(leaf)
    })?;
    value.simplify_absolute_units();Some(value)
}
fn numeric(raw:&str,computed:Option<Computation<'_>>)->Option<String>{numeric_expression(raw,computed)?.serialize()}
fn numeric_unit(raw:&str,zero_unit:&str,computed:Option<Computation<'_>>)->Option<String>{
    if typed_numeric::parse_numeric_value(raw).is_some_and(|value|value.unit==typed_numeric::NumericUnit::Number&&value.value==0.0){return Some(alloc::format!("0{zero_unit}"));}
    numeric(raw,computed)
}
fn keyword(raw:&str)->Option<String>{color_values::identifier(raw).map(|value|value.into_owned())}
fn zero(raw:&str)->bool {typed_numeric::parse_numeric_expression(raw).is_some_and(|mut value|{value.simplify_absolute_units();value.single_numeric_value().is_some_and(|value|value.value==0.0)})}
fn center(tokens:&[&str])->bool {
    !tokens.is_empty()&&tokens.iter().all(|token|decoded_css_keyword(token,"center")||typed_numeric::parse_numeric_value(token).is_some_and(|value|value.unit==typed_numeric::NumericUnit::Percent&&value.value==50.0))
}
fn position(tokens:&[&str],computed:Option<Computation<'_>>)->Option<String>{
    let mut output=Vec::new();output.try_reserve_exact(tokens.len()).ok()?;
    for token in tokens {output.push(match keyword(token).as_deref(){Some("left"|"right"|"top"|"bottom"|"center")=>keyword(token)?,_=>numeric_unit(token,"px",computed)?});}
    if output.len()==1 {if matches!(output[0].as_str(),"top"|"bottom"){output.insert(0,"center".into());}else{output.push("center".into());}}
    if output.len()==3 {
        // Three-value positions have one omitted edge offset. The canonical
        // specified representation expands it to the four-value form.
        if matches!(output[1].as_str(),"left"|"right"|"top"|"bottom"){output.insert(1,"0px".into());}
        else{output.push("0px".into());}
    }
    if output.len()==4&&matches!(output[0].as_str(),"top"|"bottom") {output.swap(0,2);output.swap(1,3);}
    // Keyword pairs are unordered but serialize horizontal then vertical.
    if output.len()==2&&matches!(output[0].as_str(),"top"|"bottom")&&matches!(output[1].as_str(),"left"|"right"|"center"){output.swap(0,1);}
    Some(output.join(" "))
}
fn method(method:InterpolationMethod)->String {
    let mut output=alloc::format!("in {}",method.space.name());
    if method.hue!=HueInterpolation::Shorter {output.push(' ');output.push_str(method.hue.name());output.push_str(" hue");}
    output
}
fn linear_header(tokens:&[&str],computed:Option<Computation<'_>>)->Option<String>{
    if tokens.is_empty(){return Some(String::new());}
    if decoded_css_keyword(tokens[0],"to"){
        let mut horizontal=None;let mut vertical=None;
        for token in &tokens[1..]{match keyword(token)?.as_str(){"left"=>horizontal=Some("left"),"right"=>horizontal=Some("right"),"top"=>vertical=Some("top"),"bottom"=>vertical=Some("bottom"),_=>return None,}}
        if horizontal.is_none()&&vertical==Some("bottom"){return Some(String::new());}
        let mut output=String::from("to");for part in [horizontal,vertical].into_iter().flatten(){output.push(' ');output.push_str(part);}return Some(output);
    }
    let [angle]=tokens else{return None;};
    let expression=numeric_expression(angle,computed)?;
    if expression.single_numeric_value().is_some_and(|value|value.unit==typed_numeric::NumericUnit::Deg&&value.value==180.0){return Some(String::new());}
    if typed_numeric::parse_numeric_value(angle).is_some_and(|value|value.unit==typed_numeric::NumericUnit::Number&&value.value==0.0){return Some("0deg".into());}
    expression.serialize()
}
fn radial_header(tokens:&[&str],computed:Option<Computation<'_>>)->Option<String>{
    let at=tokens.iter().position(|token|decoded_css_keyword(token,"at"));
    let geometry=&tokens[..at.unwrap_or(tokens.len())];
    let mut shape=None;let mut size=None;let mut radii=Vec::new();
    for token in geometry {match keyword(token).as_deref(){
        Some("circle")=>shape=Some("circle"),Some("ellipse")=>shape=Some("ellipse"),
        Some("closest-side")=>size=Some("closest-side"),Some("farthest-side")=>size=Some("farthest-side"),
        Some("closest-corner")=>size=Some("closest-corner"),Some("farthest-corner")=>size=Some("farthest-corner"),
        _=>radii.push(numeric_unit(token,"px",computed)?),
    }}
    let mut output=Vec::new();
    // One/two radii imply circle/ellipse. The initial ending shape is ellipse.
    if radii.is_empty()&&shape==Some("circle"){output.push("circle".into());}
    if let Some(size)=size.filter(|size|*size!="farthest-corner"){output.push(size.into());}
    output.extend(radii);
    if let Some(at)=at {if !center(&tokens[at+1..]){output.push(alloc::format!("at {}",position(&tokens[at+1..],computed)?));}}
    Some(output.join(" "))
}
fn conic_header(tokens:&[&str],computed:Option<Computation<'_>>)->Option<String>{
    let mut from=None;let mut at=None;let mut index=0;
    while index<tokens.len(){
        if decoded_css_keyword(tokens[index],"from"){
            let angle=*tokens.get(index+1)?;if !zero(angle){from=Some(alloc::format!("from {}",numeric_unit(angle,"deg",computed)?));}index+=2;
        }else if decoded_css_keyword(tokens[index],"at"){
            let start=index+1;index=start;
            while index<tokens.len()&&!decoded_css_keyword(tokens[index],"from"){index+=1;}
            if !center(&tokens[start..index]){at=Some(alloc::format!("at {}",position(&tokens[start..index],computed)?));}
        }else{return None;}
    }
    Some([from,at].into_iter().flatten().collect::<Vec<_>>().join(" "))
}
fn color(raw:&str,computed:Option<Computation<'_>>)->Option<String>{
    if let Some(computed)=computed { if computed.preserve_current && color_values::uses_current_color(raw){return color_values::serialize_computed(raw,computed.context,computed.query);} color_values::serialize(color_values::endpoint_with_current(raw,0,computed.current.value,computed.context,computed.query)?, color_values::color_function_in_scheme(raw,computed.context.viewport.color_schemes.page().scheme,computed.current.color_function)) }
    else { color_values::serialize_declared_endpoint(raw) }
}
fn gradient(raw:&str,name:&str,body:&str,computed:Option<Computation<'_>>)->Option<String>{
    let parts=comma_components(body,64)?;let mut first=components(parts.first()?)?;
    let (interpolation,explicit)=color_values::take_method(&mut first)?;
    let first_color=first.first().is_some_and(|token|color(token,computed).is_some());
    let header=explicit||!first_color;
    let start=usize::from(header);
    let mut stops=Vec::new();stops.try_reserve_exact(parts.len().checked_sub(start)?).ok()?;
    let mut legacy=true;let mut unknown_current=false;
    for part in &parts[start..]{
        let tokens=components(part)?;let first=*tokens.first()?;
        if let Some(color)=color(first,computed){
            legacy&=color_values::legacy(first);unknown_current|=color_values::uses_current_color(first);
            let mut stop=color;for point in &tokens[1..]{stop.push(' ');stop.push_str(&numeric_unit(point,if name.contains("conic"){"deg"}else{"px"},computed)?);}stops.push(stop);
        }else{let [hint]=tokens.as_slice() else{return None;};stops.push(numeric_unit(hint,if name.contains("conic"){"deg"}else{"px"},computed)?);}
    }
    let mut head=if header{match name{"linear-gradient"|"repeating-linear-gradient"=>linear_header(&first,computed)?,"radial-gradient"|"repeating-radial-gradient"=>radial_header(&first,computed)?,"conic-gradient"|"repeating-conic-gradient"=>conic_header(&first,computed)?,_=>return None}}else{String::new()};
    let default=if legacy{ColorSpace::Srgb}else{ColorSpace::Oklab};
    if explicit&&(unknown_current||interpolation.space!=default||interpolation.hue!=HueInterpolation::Shorter){if !head.is_empty(){head.push(' ');}head.push_str(&method(interpolation));}
    let mut output=alloc::format!("{name}(");if !head.is_empty(){output.push_str(&head);output.push_str(", ");}output.push_str(&stops.join(", "));output.push(')');
    (output.len()<=MAX_CSS_BYTES&&raw.len()<=MAX_CSS_BYTES).then_some(output)
}
fn image(raw:&str,depth:u8,computed:Option<Computation<'_>>)->Option<String>{
    if depth>=8||raw.len()>MAX_CSS_BYTES{return None;}
    if let Some(computed)=computed {
        if let Some(url)=background_url(raw) {
            let url=resolve_css_url(&url,computed.base)?;
            return Some(alloc::format!("url({})",serialize_css_string(&url)));
        }
    }
    if decoded_css_keyword(raw,"none"){return Some("none".into());}
    let Some((name,body))=raw.split_once('(') else{return Some(raw.to_owned());};
    let name=keyword(name)?;let body=body.strip_suffix(')')?;
    if matches!(name.as_str(),"linear-gradient"|"repeating-linear-gradient"|"radial-gradient"|"repeating-radial-gradient"|"conic-gradient"|"repeating-conic-gradient"){return gradient(raw,&name,body,computed);}
    if name=="cross-fade" {
        let mut items=Vec::new();let mut specified=0.0;let mut missing=0usize;
        for part in comma_components(body,16)? {
            let mut operand=None;let mut weight=None;let mut declared_weight=None;
            for token in components(part)? {
                let percentage=typed_numeric::parse_numeric_expression(token).is_some_and(|value|
                    value.numeric_type()==Some(typed_numeric::NumericType::from_unit(typed_numeric::NumericUnit::Percent)));
                if percentage {
                    if declared_weight.is_some(){return None;}
                    // Preserve computational-independence and query freezing
                    // through the same numeric codec used by gradient geometry.
                    declared_weight=Some(numeric(token,computed)?);
                    if let Some(computed)=computed {
                        let value=cross_fade_percentage(token,computed.context,computed.query)?;
                        specified+=value;weight=Some(value);
                    }
                }else{
                    if operand.is_some(){return None;}
                    operand=Some(if let Some(color)=color(token,computed){color}else{image(token,depth+1,computed)?});
                }
            }
            if declared_weight.is_none(){missing+=1;}
            items.push((operand?,weight,declared_weight));
        }
        let omitted=if missing==0 {0.0}else{(1.0-specified).max(0.0)/missing as f64};
        let mut output=Vec::new();
        for (operand,weight,declared_weight) in items {
            let percentage=if computed.is_some() {Some(typed_numeric::serialize_numeric_value(weight.unwrap_or(omitted)*100.0,typed_numeric::NumericUnit::Percent))}else{declared_weight};
            output.push(if let Some(percentage)=percentage {alloc::format!("{operand} {percentage}")}else{operand});
        }
        return Some(alloc::format!("cross-fade({})",output.join(", ")));
    }
    if matches!(name.as_str(),"image-set"|"-webkit-image-set") {
        let mut items=Vec::new();
        for part in comma_components(body,16)? {
            let tokens=components(part)?;let first=*tokens.first()?;
            let operand=if let Some(url)=background_url(first).or_else(||css_string(first).map(Arc::from)) {
                let url=if let Some(computed)=computed {resolve_css_url(&url,computed.base)?}else{url};
                alloc::format!("url({})",serialize_css_string(&url))
            }else{image(first,depth+1,computed)?};
            let mut resolution=None;let mut mime=None;
            for token in &tokens[1..] {
                if let Some(arguments)=image_function_args(token,"type(") {
                    if mime.is_some(){return None;}
                    mime=Some(alloc::format!("type({})",serialize_css_string(&css_string(arguments.trim())?)));
                }else{
                    if resolution.is_some(){return None;}
                    let expression=numeric_expression(token,computed)?;
                    if expression.numeric_type()? != typed_numeric::NumericType::from_unit(typed_numeric::NumericUnit::Dppx){return None;}
                    resolution=Some(if let Some(computed)=computed {
                        typed_numeric::serialize_numeric_value(image_set_resolution(token,computed.context,computed.query)?,typed_numeric::NumericUnit::Dppx)
                    }else if math_function(token){expression.serialize()?}else{token.to_string()});
                }
            }
            let mut item=alloc::format!("{operand} {}",resolution.unwrap_or_else(||if computed.is_some(){"1dppx".into()}else{"1x".into()}));
            if let Some(mime)=mime {item.push(' ');item.push_str(&mime);}
            items.push(item);
        }
        return Some(alloc::format!("image-set({})",items.join(", ")));
    }
    if name=="-webkit-cross-fade" {
        let mut items=Vec::new();
        for part in comma_components(body,64)?{
            let mut output=Vec::new();
            for token in components(part)? {
                if token.contains('(')&&!token.starts_with('"')&&!token.starts_with('\''){
                    let nested=token.split_once('(').and_then(|(name,_)|keyword(name));
                    if nested.as_ref().is_some_and(|name|matches!(name.as_str(),"linear-gradient"|"repeating-linear-gradient"|"radial-gradient"|"repeating-radial-gradient"|"conic-gradient"|"repeating-conic-gradient"|"cross-fade"|"-webkit-cross-fade"|"image-set"|"-webkit-image-set")){output.push(image(token,depth+1,computed)?);continue;}
                }
                if let Some(computed)=computed {
                    if background_url(token).is_some() { output.push(image(token,depth+1,Some(computed))?);continue; }
                    if let Some(color)=color(token,Some(computed)) {output.push(color);continue;}
                }
                output.push(token.to_owned());
            }
            items.push(output.join(" "));
        }
        return Some(alloc::format!("{}({})",&name,items.join(", ")));
    }
    if name=="image" {
        if let Some(computed)=computed {
            let mut output=Vec::new();
            for part in comma_components(body,8)? {
                let mut item=Vec::new();
                for token in components(part)? {
                    item.push(if background_url(token).is_some(){image(token,depth+1,Some(computed))?}
                        else if let Some(color)=color(token,Some(computed)){color}else{token.to_owned()});
                }
                output.push(item.join(" "));
            }
            return Some(alloc::format!("image({})",output.join(", ")));
        }
    }
    Some(raw.to_owned())
}
pub(super) fn serialize(raw:&str)->Option<String>{
    let mut layers=Vec::new();
    for layer in comma_components(raw,8)?{layers.push(image(layer,0,None)?);}
    let result=layers.join(", ");(result.len()<=MAX_CSS_BYTES).then_some(result)
}

/// Computed image serialization keeps every source candidate and delegates
/// dimensions, colors, and URL resolution to their shared canonical codecs.
pub(super) fn computed(raw:&str,context:LengthContext,query:ContainerUnitContext,current:&SourceColor,base:Option<&str>)->Option<String>{
    let selected=color_scheme::select_branches(raw,context.viewport.color_schemes.page().scheme)?;
    let value=image(&selected,0,Some(Computation{context,query,current,base,independent:false,preserve_current:false}))?;
    (value.len()<=MAX_VARIABLE_BYTES).then_some(value)
}

pub(super) fn independent(raw:&str)->bool {
    !color_values::uses_current_color(raw) && image(raw,0,Some(Computation {context:static_length_context(),query:ContainerUnitContext::default(),current:&legacy_source_color(Style::initial().color),base:None,independent:true,preserve_current:false})).is_some()
}

/// Images4 §2.4 prohibits image-set descendants of another image-set,
/// including intervening image functions. Reuse CSS Syntax token boundaries;
/// strings, comments, and URL tokens cannot manufacture nested functions.
pub(super) fn nested_image_set(raw:&str)->bool {
    let Some(mut cursor)=syntax::Cursor::new(raw,0).ok() else{return true;};
    let mut blocks=[false;syntax::MAX_COMPONENT_DEPTH];
    let (mut depth,mut sets,mut pending)=(0usize,0usize,false);
    while let Some(token)=cursor.next() {
        match token.kind {
            syntax::TokenKind::Other=>{
                pending=raw.as_bytes().get(token.end)==Some(&b'(')
                    && ["image-set","-webkit-image-set"].iter().any(|name|decoded_css_keyword(&raw[token.start..token.end],name));
                if pending&&sets!=0{return true;}
            },
            syntax::TokenKind::Open(_)=>{
                if depth==blocks.len(){return true;}
                blocks[depth]=pending;sets+=usize::from(pending);depth+=1;pending=false;
            },
            syntax::TokenKind::Close(_)=>{
                if depth==0{return true;}
                depth-=1;sets-=usize::from(blocks[depth]);pending=false;
            },
            _=>pending=false,
        }
    }
    depth!=0
}

/// Candidate/composition source is computed before resource negotiation. Use
/// the shared lexer so URL/string contents cannot manufacture source carriers.
pub(super) fn retains_candidates(raw:&str)->bool {
    let Some(mut cursor)=syntax::Cursor::new(raw,0).ok() else{return false;};
    while let Some(token)=cursor.next() {
        if token.kind==syntax::TokenKind::Other && raw.as_bytes().get(token.end)==Some(&b'(')
            && ["image-set","-webkit-image-set","cross-fade","-webkit-cross-fade"].iter().any(|name|decoded_css_keyword(&raw[token.start..token.end],name)){return true;}
    }
    false
}
pub(super) fn computed_layers(raw:&str,context:LengthContext,query:ContainerUnitContext,current:&SourceColor,base:Option<&str>)->Option<String> {
    let mut output=Vec::new();
    for layer in comma_components(raw,MAX_BACKGROUND_LAYERS)? {output.push(computed(layer,context,query,current,base)?);}
    let output=output.join(", ");(output.len()<=MAX_CSS_BYTES).then_some(output)
}

/// Freeze dimensions and resource bases at the selected container while keeping
/// currentColor expressions live for subsequent inheritance/color refresh.
pub(super) fn freeze_layers(raw:&str,context:LengthContext,query:ContainerUnitContext,current:&SourceColor,base:Option<&str>)->Option<String> {
    let selected=color_scheme::select_branches(raw,context.viewport.color_schemes.page().scheme)?;
    let computation=Computation{context,query,current,base,independent:false,preserve_current:true};
    let mut output=Vec::new();
    for layer in comma_components(&selected,MAX_BACKGROUND_LAYERS)? {output.push(image(layer,0,Some(computation))?);}
    let output=output.join(", ");(output.len()<=MAX_CSS_BYTES).then_some(output)
}
