//! Media conditions reuse the cascade's bounded component lexer and typed math.
use super::*;
#[derive(Clone,Copy)]
enum Truth { Known(bool),Unknown }
impl Truth {
    fn and(self,other:Self)->Self {match(self,other){(Self::Known(false),_)|(_,Self::Known(false))=>Self::Known(false),(Self::Known(true),Self::Known(true))=>Self::Known(true),_=>Self::Unknown}}
    fn or(self,other:Self)->Self {match(self,other){(Self::Known(true),_)|(_,Self::Known(true))=>Self::Known(true),(Self::Known(false),Self::Known(false))=>Self::Known(false),_=>Self::Unknown}}
    fn not(self)->Self {match self{Self::Known(value)=>Self::Known(!value),Self::Unknown=>Self::Unknown}}
    fn matches(self)->bool {matches!(self,Self::Known(true))}
}
fn length(raw:&str,environment:MediaEnvironment)->Option<f64> {
    let mut context=static_length_context();context.viewport=environment;
    let expression=typed_numeric::parse_numeric_expression(raw)?;
    if expression.numeric_type()? != typed_numeric::NumericType::from_unit(typed_numeric::NumericUnit::Px) && !typed_numeric::parse_numeric_value(raw).is_some_and(|value|value.unit==typed_numeric::NumericUnit::Number&&value.value==0.0){return None;}
    let value=expression.evaluate(&mut FontAngleContext{length:Some(context),query:ContainerUnitContext::no_container(environment),percent_scale:0.01})?;
    value.is_finite().then_some(value)
}
fn actual(name:&str,environment:MediaEnvironment)->Option<f64> {
    match name {"width"=>Some(f64::from(environment.width)),"height"=>Some(f64::from(environment.height)),"resolution"=>Some(f64::from(environment.resolution)),"aspect-ratio"=>Some(f64::from(environment.width)/f64::from(environment.height)),_=>None}
}
fn expected(name:&str,raw:&str,environment:MediaEnvironment)->Option<f64> {
    match name {
        "width"|"height"=>length(raw,environment),
        "resolution"=>{
            let mut expression=typed_numeric::parse_numeric_expression(raw)?;
            expression.simplify_absolute_units();
            if expression.numeric_type()?!=typed_numeric::NumericType::from_unit(typed_numeric::NumericUnit::Dppx){return None;}
            let mut context=static_length_context();context.viewport=environment;
            let value=expression.evaluate(&mut FontAngleContext{length:Some(context),query:ContainerUnitContext::no_container(environment),percent_scale:0.01})?;
            value.is_finite().then_some(value)
        },
        "aspect-ratio"=>{let (a,b)=raw.split_once('/').unwrap_or((raw,"1"));let a=typed_numeric::parse_numeric_value(a.trim())?;let b=typed_numeric::parse_numeric_value(b.trim())?;(a.unit==typed_numeric::NumericUnit::Number&&b.unit==typed_numeric::NumericUnit::Number&&a.value>=0.0&&b.value>0.0).then_some(a.value/b.value)},
        _=>None,
    }
}
fn compare(a:f64,op:&str,b:f64)->Option<bool> {Some(match op {"="=>a==b,"<"=>a<b,"<="=>a<=b,">"=>a>b,">="=>a>=b,_=>return None})}
fn feature(body:&str,environment:MediaEnvironment)->Option<Truth> {
    let mut cursor=syntax::Cursor::new(body,0).ok()?;let mut operators=Vec::new();let mut colon=None;
    while let Some(token)=cursor.next(){
        match token.kind {
            syntax::TokenKind::Open(_)=>cursor.position=syntax::block(body,token.start).ok()?.after,
            syntax::TokenKind::Close(_)|syntax::TokenKind::BadString|syntax::TokenKind::BadUrl=>return None,
            syntax::TokenKind::Other=>match &body[token.start..token.end] {
                ":"=>{if colon.replace(token.start).is_some(){return None;}},
                "<"|">"|"="=>{let mut end=token.end;if body.as_bytes().get(end)==Some(&b'=')&&&body[token.start..token.end]!="="{end+=1;cursor.position=end;}operators.push((token.start,end));if operators.len()>2{return None;}},
                _=>{},
            },
            _=>{},
        }
    }
    if let Some(colon)=colon {
        if !operators.is_empty(){return None;}
        let name=keyword_value(body[..colon].trim())?;let value=body[colon+1..].trim();
        if name=="prefers-color-scheme" {return Some(match keyword_value(value).as_deref(){Some("light")=>Truth::Known(environment.color_schemes.page().scheme==UsedColorScheme::Light),Some("dark")=>Truth::Known(environment.color_schemes.page().scheme==UsedColorScheme::Dark),_=>Truth::Unknown});}
        if name=="orientation" {return Some(match keyword_value(value).as_deref(){Some("portrait")=>Truth::Known(environment.height>=environment.width),Some("landscape")=>Truth::Known(environment.width>environment.height),_=>Truth::Unknown});}
        let (name,op)=if let Some(name)=name.strip_prefix("min-"){(name,">=")}else if let Some(name)=name.strip_prefix("max-"){(name,"<=")}else{(name.as_str(),"=")};
        return Some(match actual(name,environment).zip(expected(name,value,environment)){Some((a,b))=>Truth::Known(compare(a,op,b)?),None=>Truth::Unknown});
    }
    if operators.is_empty(){
        let Some(name)=keyword_value(body)else{return Some(Truth::Unknown);};
        return Some(actual(&name,environment).map_or_else(||if matches!(name.as_str(),"orientation"|"prefers-color-scheme"){Truth::Known(true)}else{Truth::Unknown},|value|Truth::Known(value!=0.0)));
    }
    let first=operators[0];let left=body[..first.0].trim();let middle=body[first.1..operators.get(1).map_or(body.len(),|op|op.0)].trim();
    let op=&body[first.0..first.1];
    if operators.len()==1 {
        if let Some(name)=keyword_value(left).filter(|name|actual(name,environment).is_some()) {
            return Some(expected(&name,middle,environment).map_or(Truth::Unknown,|value|Truth::Known(compare(actual(&name,environment).unwrap(),op,value).unwrap())));
        }
        if let Some(name)=keyword_value(middle).filter(|name|actual(name,environment).is_some()) {
            return Some(expected(&name,left,environment).map_or(Truth::Unknown,|value|Truth::Known(compare(value,op,actual(&name,environment).unwrap()).unwrap())));
        }
        return Some(Truth::Unknown);
    }
    let second=operators[1];let op2=&body[second.0..second.1];
    if !(op.starts_with('<')&&op2.starts_with('<')||op.starts_with('>')&&op2.starts_with('>')){return None;}
    let Some(name)=keyword_value(middle).filter(|name|actual(name,environment).is_some())else{return Some(Truth::Unknown);};
    Some(match expected(&name,left,environment).zip(expected(&name,body[second.1..].trim(),environment)){Some((a,b))=>Truth::Known(compare(a,op,actual(&name,environment).unwrap())?&&compare(actual(&name,environment).unwrap(),op2,b)?),None=>Truth::Unknown})
}
fn keyword_value(raw:&str)->Option<String> {let mut at=0;let value=consume_selector_identifier(raw.trim(),&mut at)?;(at==raw.trim().len()).then(||value.to_ascii_lowercase())}
fn condition(raw:&str,environment:MediaEnvironment,depth:usize)->Option<Truth> {
    if depth>16{return None;}
    let parts=components(raw)?;if parts.is_empty(){return None;}
    if decoded_css_keyword(parts[0],"not") {if parts.len()!=2{return None;}return condition(parts[1],environment,depth+1).map(Truth::not);}
    if parts.len()==1 {
        let inner=parts[0].strip_prefix('(')?.strip_suffix(')')?;
        if let Some(value)=condition(inner,environment,depth+1){return Some(value);}
        return feature(inner,environment);
    }
    if parts.len()%2==0{return None;}
    let and=decoded_css_keyword(parts[1],"and");if !and&&!decoded_css_keyword(parts[1],"or"){return None;}
    let mut value=condition(parts[0],environment,depth+1)?;
    for pair in parts[1..].chunks_exact(2){
        if !decoded_css_keyword(pair[0],if and{"and"}else{"or"}){return None;}
        let next=condition(pair[1],environment,depth+1)?;value=if and{value.and(next)}else{value.or(next)};
    }
    Some(value)
}
pub(super) fn condition_matches(raw:&str,environment:MediaEnvironment)->bool {condition(raw,environment,0).is_some_and(Truth::matches)}
pub(super) fn query_matches(raw:&str,environment:MediaEnvironment)->bool {
    if raw.trim().is_empty(){return true;}
    let Some(alternatives)=top_level_split(raw,b',',64)else{return false;};
    alternatives.iter().any(|query|{
        let Some(parts)=components(query)else{return false;};if parts.is_empty(){return false;}
        if parts[0].starts_with('(')||decoded_css_keyword(parts[0],"not")&&parts.get(1).is_some_and(|value|value.starts_with('(')){return condition_matches(query,environment);}
        let modified=decoded_css_keyword(parts[0],"not")||decoded_css_keyword(parts[0],"only");let start=usize::from(modified);let Some(ty)=parts.get(start).and_then(|value|keyword_value(value))else{return false;};
        if matches!(ty.as_str(),"not"|"only"|"and"|"or"|"layer"){return false;}
        let mut value=Truth::Known(match ty.as_str(){"all"=>true,"screen"=>!environment.print,"print"=>environment.print,_=>false});
        if parts.len()>start+1 {
            if !decoded_css_keyword(parts[start+1],"and"){return false;}
            let rest=parts[start+2..].join(" ");
            // A media-type condition uses the without-or grammar at its top level.
            if parts[start+2..].iter().skip(1).step_by(2).any(|token|decoded_css_keyword(token,"or")){return false;}
            let Some(next)=condition(&rest,environment,0)else{return false;};value=value.and(next);
        }
        if modified&&decoded_css_keyword(parts[0],"not"){value=value.not();}
        value.matches()
    })
}

/// HTML sizes: evaluate the first valid matching item, then default to 100vw.
/// auto_width is supplied only for an image that allows auto-sizes, using its
/// concrete object width or the owner-retained last auto-sizes width.
pub(super) fn source_size(raw:&str,environment:MediaEnvironment,auto_width:Option<f32>)->f32 {
    let fallback=environment.width.max(0.0);
    let Some(items)=top_level_split(raw,b',',64)else{return fallback;};
    for item in items {
        let Some(parts)=components(item)else{continue;};let Some(last)=parts.last()else{continue;};
        let size=if decoded_css_keyword(last,"auto"){auto_width.filter(|width|width.is_finite()&&*width>=0.0)}else{length(last,environment).filter(|value|*value>=0.0&&*value<=f64::from(f32::MAX)).map(|value|value as f32)};
        let Some(size)=size else{continue;};
        let Some(start)=(last.as_ptr() as usize).checked_sub(item.as_ptr() as usize)else{continue;};
        let Some(condition)=item.get(..start).map(str::trim)else{continue;};
        if condition.is_empty()||condition_matches(condition,environment){return size;}
    }
    fallback
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn specification_responsive_sizes_share_media_ranges_and_initial_units() {
        let environment=MediaEnvironment{width:800.0,height:600.0,resolution:2.0,..MediaEnvironment::default()};
        for (raw,expected) in [("(width <= 30em) 100vw, (30em < width < 60em) calc(50vw - 20px), 100px",380.0),("(orientation: landscape) 20em, 100vw",320.0),("screen 10px, 25vw",200.0),("not (unknown: value) 10px, 25vw",200.0),("(width) and ((resolution >= 2dppx) or (height < 1px)) 30vw, 100px",240.0),("auto, 20vw",160.0),("10%, 50vw",400.0)] {
            assert_eq!(source_size(raw,environment,None),expected,"{raw}");
        }
        assert_eq!(source_size("auto, 100vw",environment,Some(137.0)),137.0);
        for raw in ["(width > 799px)","(aspect-ratio > 1)","(aspect-ratio: 4/3)","(resolution: calc(100dpi - 4dpi)) or (resolution: 2dppx)","(400px <= width <= 800px)","screen and (min-width: 50em)","not print", "(unknown: value) or (width: 800px)"]{assert!(query_matches(raw,environment),"{raw}");}
        for raw in ["not (unknown: value)","(width: 800px) and (height: 600px) or (resolution: 2dppx)",",", "screen or (width: 800px)"]{assert!(!query_matches(raw,environment),"{raw}");}
    }
}
