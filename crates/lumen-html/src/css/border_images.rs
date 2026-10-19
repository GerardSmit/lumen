use super::*;

/// Rare border image state. Ordinary styles keep no allocation; longhand
/// cascade updates preserve all unrelated components through the shared Arc.
#[derive(Clone, Debug, PartialEq)]
pub struct BorderImage {
    pub source: BackgroundImage,
    pub slice: [BorderImageSlice; 4],
    pub fill: bool,
    pub width: [BorderImageDimension; 4],
    pub outset: [BorderImageDimension; 4],
    pub repeat: [BorderImageRepeat; 2],
}
#[derive(Clone, Copy, Debug, PartialEq)]
/// Percentage values retain percentage points; source geometry converts units once.
pub struct BorderImageSlice { pub value: f32, pub percentage: bool }
#[derive(Clone, Debug, PartialEq)]
pub enum BorderImageDimension { Number(f32), Length(DecorationLength), Auto }
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BorderImageRepeat { Stretch, Repeat, Round, Space }
fn identifier(raw:&str)->Option<String>{let mut at=0;let value=consume_selector_identifier(raw,&mut at)?;(at==raw.len()).then_some(value)}
impl BorderImageRepeat {
    pub fn as_str(self)-> &'static str {match self {Self::Stretch=>"stretch",Self::Repeat=>"repeat",Self::Round=>"round",Self::Space=>"space"}}
    fn parse(raw:&str)->Option<Self> {Some(match identifier(raw)?.to_ascii_lowercase().as_str(){"stretch"=>Self::Stretch,"repeat"=>Self::Repeat,"round"=>Self::Round,"space"=>Self::Space,_=>return None})}
}
impl Default for BorderImage {
    fn default()->Self {Self {source:BackgroundImage::None,slice:[BorderImageSlice{value:100.0,percentage:true};4],fill:false,
        width:core::array::from_fn(|_|BorderImageDimension::Number(1.0)),outset:core::array::from_fn(|_|BorderImageDimension::Number(0.0)),repeat:[BorderImageRepeat::Stretch;2]}}
}
impl BorderImageDimension {
    pub fn used(&self,border:f32,area:f32,automatic:f32)->f32 {match self {Self::Number(value)=>value*border,Self::Length(value)=>value.used(area).unwrap_or(0.0).max(0.0),Self::Auto=>automatic}}
    fn serialize(&self)->String {match self {Self::Number(value)=>computed_values::number(*value),Self::Length(value)=>value.computed(),Self::Auto=>"auto".into()}}
}
fn quad<T:Clone>(values:&[T])->Option<[T;4]> {Some(match values {[a]=>[a.clone(),a.clone(),a.clone(),a.clone()],[a,b]=>[a.clone(),b.clone(),a.clone(),b.clone()],[a,b,c]=>[a.clone(),b.clone(),c.clone(),b.clone()],[a,b,c,d]=>[a.clone(),b.clone(),c.clone(),d.clone()],_=>return None})}
fn scalar(raw:&str,context:LengthContext,query:ContainerUnitContext)->Option<BorderImageSlice> {
    use typed_numeric::{NumericType,NumericUnit};
    let ty=typed_numeric::parse_numeric_expression(raw)?.numeric_type()?;
    let percentage=ty==NumericType::from_unit(NumericUnit::Percent);
    if !percentage && ty!=NumericType::default(){return None;}
    let value=css_scalar_with_percentage_basis(raw,percentage,context,query,100.0)?.0;
    if !value.is_finite() || value<0.0 && !math_function(raw){return None;}
    Some(BorderImageSlice{value:value.max(0.0),percentage})
}
fn dimension(raw:&str,context:LengthContext,query:ContainerUnitContext,width:bool)->Option<BorderImageDimension> {
    if width && identifier(raw).is_some_and(|word|word.eq_ignore_ascii_case("auto")){return Some(BorderImageDimension::Auto);}
    if let Some(number)=scalar(raw,context,query).filter(|value|!value.percentage){return Some(BorderImageDimension::Number(number.value));}
    let value=decoration_length_with_context(raw,context,query)?;
    if !matches!(value,DecorationLength::Length(_)|DecorationLength::Expression(_)){return None;}
    if !width && typed_numeric::parse_numeric_expression(raw)?.numeric_type()?.percent!=0{return None;}
    if negative_length_primitive(raw){return None;}
    Some(BorderImageDimension::Length(value))
}
fn fill_keyword(raw:&str)->bool {identifier(raw).is_some_and(|word|word.eq_ignore_ascii_case("fill"))}
fn parse_slice(tokens:&[&str],context:LengthContext,query:ContainerUnitContext)->Option<([BorderImageSlice;4],bool)> {
    let (tokens,fill)=if tokens.first().is_some_and(|token|fill_keyword(token)) {(&tokens[1..],true)}
        else if tokens.last().is_some_and(|token|fill_keyword(token)) {(&tokens[..tokens.len()-1],true)}else{(tokens,false)};
    if tokens.is_empty()||tokens.len()>4{return None;}
    let numbers=tokens.iter().map(|token|scalar(token,context,query)).collect::<Option<Vec<_>>>()?;
    Some((quad(&numbers)?,fill))
}
pub(super) fn parse_component(slot:usize,raw:&str,context:LengthContext,query:ContainerUnitContext,color:Rgba)->Option<BorderImage> {
    let mut result=BorderImage::default();let tokens=components(raw)?;
    match slot {
        227=>{result.source=if identifier(raw).is_some_and(|name|name.eq_ignore_ascii_case("none")){BackgroundImage::None}else{background_image_part_with_query(raw,color,Some(context),0,false,query)?};},
        228=>{(result.slice,result.fill)=parse_slice(&tokens,context,query)?;},
        229|230=>{let values=tokens.iter().map(|token|dimension(token,context,query,slot==229)).collect::<Option<Vec<_>>>()?;
            if slot==229 {result.width=quad(&values)?;}else{result.outset=quad(&values)?;}},
        231=>{result.repeat=match tokens.as_slice(){[a]=>[BorderImageRepeat::parse(a)?;2],[a,b]=>[BorderImageRepeat::parse(a)?,BorderImageRepeat::parse(b)?],_=>return None};},
        _=>return None,
    }
    Some(result)
}
pub(super) fn shorthand(raw:&str)->Option<Vec<Value>> {
    let parts=top_level_split(raw,b'/',3)?;let mut tokens=Vec::new();
    for (index,part) in parts.iter().enumerate(){if index!=0{tokens.push("/");}tokens.extend(components(part)?);}
    let context=static_length_context();let query=ContainerUnitContext::no_container(context.viewport);let color=Style::initial().color;
    let mut source=None;let mut repeat=None;let mut slice=None;let mut width=None;let mut outset=None;let mut at=0;
    while let Some(&token)=tokens.get(at) {
        if parse_component(227,token,context,query,color).is_some() {
            if source.replace(token).is_some(){return None;}at+=1;continue;
        }
        if BorderImageRepeat::parse(token).is_some() {
            if repeat.is_some(){return None;}let start=at;at+=1;
            if tokens.get(at).is_some_and(|token|BorderImageRepeat::parse(token).is_some()){at+=1;}
            repeat=Some(tokens[start..at].join(" "));continue;
        }
        if slice.is_some(){return None;}
        let start=at;let leading_fill=fill_keyword(token);if leading_fill{at+=1;}
        let numbers=at;
        while tokens.get(at).is_some_and(|token|scalar(token,context,query).is_some()) && at-numbers<4{at+=1;}
        if at==numbers{return None;}
        if !leading_fill && tokens.get(at).is_some_and(|token|fill_keyword(token)){at+=1;}
        slice=Some(tokens[start..at].join(" "));
        if tokens.get(at)==Some(&"/") {
            at+=1;let start=at;
            while tokens.get(at).is_some_and(|token|dimension(token,context,query,true).is_some()) && at-start<4{at+=1;}
            if at>start{width=Some(tokens[start..at].join(" "));}
            if tokens.get(at)==Some(&"/") {
                at+=1;let start=at;
                while tokens.get(at).is_some_and(|token|dimension(token,context,query,false).is_some()) && at-start<4{at+=1;}
                if at==start{return None;}outset=Some(tokens[start..at].join(" "));
            }else if width.is_none(){return None;}
        }
    }
    if source.is_none()&&slice.is_none()&&repeat.is_none(){return None;}
    let parts=[source.unwrap_or("none").to_string(),slice.unwrap_or_else(||"100%".into()),width.unwrap_or_else(||"1".into()),outset.unwrap_or_else(||"0".into()),repeat.unwrap_or_else(||"stretch".into())];
    parts.into_iter().enumerate().map(|(index,raw)|{
        let slot=227+index;parse_component(slot,&raw,context,query,color)?;
        Some(Value::BorderImageRaw(slot,raw))
    }).collect()
}
pub(super) fn apply(style:&mut Style,slot:usize,value:&BorderImage) {
    if style.border_image.is_none() {let initial=BorderImage::default();let same=match slot{227=>value.source==initial.source,228=>value.slice==initial.slice&&value.fill==initial.fill,229=>value.width==initial.width,230=>value.outset==initial.outset,231=>value.repeat==initial.repeat,_=>true};if same{return;}}
    let current=style.border_image.get_or_insert_with(||Arc::new(BorderImage::default()));let target=Arc::make_mut(current);
    match slot {227=>target.source=value.source.clone(),228=>{target.slice=value.slice;target.fill=value.fill;},229=>target.width=value.width.clone(),230=>target.outset=value.outset.clone(),231=>target.repeat=value.repeat,_=>{}}
    if *target==BorderImage::default(){style.border_image=None;}
}
pub(super) fn copy(to:&mut Style,from:&Style,slot:usize) {let initial=BorderImage::default();apply(to,slot,from.border_image.as_deref().unwrap_or(&initial));}
pub(super) fn serialize(value:&BorderImage,slot:usize)->String {match slot {
    227=>computed_values::image(&value.source,0).expect("validated border image"),
    228=>{let mut result=computed_values::four(value.slice.map(|item|if item.percentage{alloc::format!("{}%",computed_values::number(item.value))}else{computed_values::number(item.value)}));if value.fill{result.push_str(" fill");}result},
    229=>computed_values::four(core::array::from_fn(|index|value.width[index].serialize())),230=>computed_values::four(core::array::from_fn(|index|value.outset[index].serialize())),231=>if value.repeat[0]==value.repeat[1]{value.repeat[0].as_str().into()}else{alloc::format!("{} {}",value.repeat[0].as_str(),value.repeat[1].as_str())},_=>String::new()
}}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn specification_border_image_typed_grammar_shorthand_and_cascade_components() {
        let context=static_length_context();let query=ContainerUnitContext::no_container(context.viewport);let color=Style::initial().color;
        let declarations=shorthand("linear-gradient(red, blue) fill 10% 20 / 2 auto 30% / 1 2px round space").unwrap();
        assert_eq!(declarations.len(),5);
        let mut style=Style::initial();
        for value in declarations {
            let Value::BorderImageRaw(slot,raw)=value else{panic!("canonical border image declaration");};
            apply(&mut style,slot,&parse_component(slot,&raw,context,query,color).unwrap());
        }
        let image=style.border_image.as_ref().unwrap();
        assert_eq!(image.slice,[BorderImageSlice{value:10.0,percentage:true},BorderImageSlice{value:20.0,percentage:false},BorderImageSlice{value:10.0,percentage:true},BorderImageSlice{value:20.0,percentage:false}]);
        assert_eq!(image.repeat,[BorderImageRepeat::Round,BorderImageRepeat::Space]);
        assert_eq!(serialize(image,229),"2 auto 30%");
        let mut child=Style::initial();copy(&mut child,&style,228);
        assert_eq!(child.border_image.as_ref().unwrap().slice,image.slice);
        assert_eq!(child.border_image.as_ref().unwrap().source,BackgroundImage::None);
        apply(&mut child,228,&BorderImage::default());assert!(child.border_image.is_none());
        for (slot,raw) in [(228,"-1"),(228,"fill"),(228,"1 fill fill"),(229,"-1px"),(230,"1%"),(230,"auto"),(231,"repeat space stretch")] {
            assert!(parse_component(slot,raw,context,query,color).is_none(),"invalid {slot}: {raw}");
        }
        for raw in ["1 /","1 //","1 / 2 / 3 / 4","none none","1 fill fill"] {assert!(shorthand(raw).is_none(),"invalid shorthand: {raw}");}
        let computed=parse_component(229,"calc(2px + 10%)",context,query,color).unwrap();
        assert_eq!(computed.width[0].used(5.0,100.0,0.0),12.0);
    }
}

#[cfg(test)]
mod contextual_tests {
    use super::*;
    #[test]
    fn specification_border_image_slice_contextual_math_uses_real_font_and_query_units() {
        let color=Style::initial().color;
        let query=ContainerUnitContext::no_container(MediaEnvironment{width:200.0,height:100.0,..MediaEnvironment::default()});
        let low=LengthContext{font:5.0,..static_length_context()};let high=LengthContext{font:20.0,..low};
        for (raw,expected_low,expected_high) in [
            ("calc(10% + 5% * sign(1em - 10px))",5.0,15.0),
            ("calc(10 + 5 * sign(1em - 10px))",5.0,15.0),
        ] {
            let a=parse_component(228,raw,low,query,color).unwrap();let b=parse_component(228,raw,high,query,color).unwrap();
            assert!((a.slice[0].value-expected_low).abs()<0.00001);
            assert!((b.slice[0].value-expected_high).abs()<0.00001);
            assert!(serialize_specified(228,raw).unwrap().contains("1em"),"specified source is not frozen to dummy font metrics");
        }
        let number=parse_component(228,"calc(10 + 5 * sign(10% - 5%))",high,query,color).unwrap();
        assert_eq!(number.slice[0],BorderImageSlice{value:15.0,percentage:false});
        let narrow=ContainerUnitContext::no_container(MediaEnvironment{width:200.0,height:100.0,..MediaEnvironment::default()});
        let wide=ContainerUnitContext::no_container(MediaEnvironment{width:1000.0,height:100.0,..MediaEnvironment::default()});
        let raw="calc(10% + 5% * sign(1cqw - 5px))";
        assert!((parse_component(228,raw,high,narrow,color).unwrap().slice[0].value-5.0).abs()<0.00001);
        assert!((parse_component(228,raw,high,wide,color).unwrap().slice[0].value-15.0).abs()<0.00001);
    }
}

/// Specified lengths retain their CSS units; computing them with a dummy font
/// would incorrectly turn e.g. authored em values into pixel values in CSSOM.
pub(super) fn serialize_specified(slot:usize,raw:&str)->Option<String> {
    let value=parse_component(slot,raw,static_length_context(),ContainerUnitContext::no_container(static_length_context().viewport),Style::initial().color)?;
    if slot==227 {return match &value.source {BackgroundImage::None|BackgroundImage::Url(_)|BackgroundImage::UrlResolution{..}=>computed_values::image(&value.source,0),_=>Some(raw.to_string())};}
    if slot==231 {return Some(serialize(&value,slot));}
    let mut values=Vec::new();let mut fill=false;
    for token in components(raw)? {
        if identifier(token).is_some_and(|name|name.eq_ignore_ascii_case("fill")){fill=true;continue;}
        if identifier(token).is_some_and(|name|name.eq_ignore_ascii_case("auto")){values.push(String::from("auto"));}
        else {values.push(typed_numeric::parse_numeric_expression(token)?.serialize()?);}
    }
    let mut result=computed_values::four(quad(&values)?);
    if fill {result.push_str(" fill");}
    Some(result)
}

/// CSSOM omits initial shorthand components wherever the border-image grammar
/// permits it. A noninitial width/outset still requires a slice before '/'.
pub(super) fn serialize_shorthand(source:&str,slice:&str,width:&str,outset:&str,repeat:&str)->String {
    let mut value=String::new();
    if source!="none" {value.push_str(source);}
    if slice!="100%" || width!="1" || outset!="0" {
        if !value.is_empty(){value.push(' ');}value.push_str(slice);
    }
    if width!="1" {value.push_str(" / ");value.push_str(width);}
    if outset!="0" {
        if width=="1" {value.push_str(" / /");}else{value.push_str(" /");}
        value.push(' ');value.push_str(outset);
    }
    if repeat!="stretch" {if !value.is_empty(){value.push(' ');}value.push_str(repeat);}
    if value.is_empty(){value.push_str("none");}
    value
}

#[cfg(test)]
mod serialization_tests {
    use super::*;
    #[test]
    fn specification_border_image_shortest_shorthand_preserves_double_slash_and_required_slice() {
        for (parts,expected) in [
            (["none","100%","1","0","stretch"],"none"),
            (["none","100%","1","0","space"],"space"),
            ([r#"url("border.png")"#,"100%","1","0","stretch"],r#"url("border.png")"#),
            (["none","1","1","2px","stretch"],"1 / / 2px"),
            (["none","100%","2","0","stretch"],"100% / 2"),
            (["none","100%","2","3px","round space"],"100% / 2 / 3px round space"),
        ] {
            let value=serialize_shorthand(parts[0],parts[1],parts[2],parts[3],parts[4]);
            assert_eq!(value,expected);
            let declarations=shorthand(&value).unwrap();
            for (index,declaration) in declarations.iter().enumerate() {
                let Value::BorderImageRaw(slot,raw)=declaration else{panic!("canonical expanded shorthand");};
                assert_eq!(serialize_specified(*slot,raw).unwrap(),parts[index],"shortest form must roundtrip every component");
            }
        }
    }
}

#[cfg(test)]
mod group_tests {
    use super::*;
    #[test]
    fn specification_border_image_group_permutations_preserve_contiguous_slice_and_slash_components() {
        let source=r#"url("border.png")"#;let slices="fill 10% 20 / auto 2 / 1px 2";let repeat="round space";
        let values=|input:&str|shorthand(input).unwrap().iter().map(|value|match value{Value::BorderImageRaw(slot,raw)=>serialize_specified(*slot,raw).unwrap(),_=>panic!("expanded raw components")}).collect::<Vec<_>>();
        let canonical=values(&alloc::format!("{source} {slices} {repeat}"));
        for order in [[source,slices,repeat],[source,repeat,slices],[slices,source,repeat],[slices,repeat,source],[repeat,source,slices],[repeat,slices,source]] {
            let actual=values(&order.join(" "));
            assert_eq!(actual,canonical,"whole components may occur in any order");
        }
        for invalid in ["1% fill 2%","fill 1% fill","1 none 2","1 / 2 round 3","1 / 2 none / 3","repeat none round","1 / / 2px 3px 4px 5px 6px"] {
            assert!(shorthand(invalid).is_none(),"split or oversized grammar component: {invalid}");
        }
        let context=static_length_context();let query=ContainerUnitContext::no_container(context.viewport);let color=Style::initial().color;
        assert!(parse_component(228,"1% fill 2%",context,query,color).is_none());
        for valid in [r"\66 ill 1 2%",r"1 2% \66 ill",r"\72 ound 1 / / 2px none",r"1 / / 2px none round"] {
            assert!(shorthand(valid).is_some(),"canonical identifier escapes and grouped slash grammar: {valid}");
        }
    }
}
