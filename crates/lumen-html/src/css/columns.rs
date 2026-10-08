//! Multicol declarations retain the shared numeric AST until their used inline
//! basis is known. Source parsing, query freezing, CSSOM and layout share it.
use super::*;
#[derive(Clone,Debug,Default,PartialEq)]
pub enum ColumnWidth {
    #[default] Auto,
    Length(Arc<typed_numeric::NumericExpression>),
    Intrinsic(IntrinsicSizing),
    Stretch,
    FitContent(Arc<typed_numeric::NumericExpression>),
}
impl ColumnWidth {
    pub(crate) fn serialize(&self)->Option<String> {Some(match self {
        Self::Auto=>"auto".into(),Self::Length(value)=>value.serialize()?,
        Self::Intrinsic(value)=>value.as_str().into(),Self::Stretch=>"stretch".into(),
        Self::FitContent(value)=>alloc::format!("fit-content({})",value.serialize()?),
    })}
    pub(crate) fn checked_retained_bytes(&self)->Option<usize> {match self {
        Self::Length(value)|Self::FitContent(value)=>core::mem::size_of::<usize>().checked_mul(2)?.checked_add(core::mem::size_of::<typed_numeric::NumericExpression>())?.checked_add(value.checked_retained_bytes()?),
        _=>Some(0),
    }}
    fn numeric(&self,basis:Option<f32>)->Option<f32> {
        let value=match self {Self::Length(value)|Self::FitContent(value)=>value,_=>return None};
        // Match the shared length-percentage evaluator's indefinite-basis rule.
        if basis.is_none() && value.contains_unit(|unit|unit==typed_numeric::NumericUnit::Percent){return None;}
        let result=value.evaluate(&mut FontAngleContext{length:None,query:ContainerUnitContext::default(),percent_scale:basis.map_or(0.0,|basis|f64::from(basis)/100.0)})?;
        Some(typed_numeric::computed_f32(result).max(0.0))
    }
    /// Optimal width precedes the canonical used-count algorithm; intrinsic
    /// keywords consume measured content contributions, never tag guesses.
    pub(crate) fn optimal(&self,available:Option<f32>,minimum:f32,maximum:f32)->Option<f32> {match self {
        Self::Auto=>None,Self::Length(_)=>self.numeric(available),
        Self::Stretch=>available,Self::Intrinsic(IntrinsicSizing::MinContent)=>Some(minimum),
        Self::Intrinsic(IntrinsicSizing::MaxContent)=>Some(maximum),
        Self::Intrinsic(IntrinsicSizing::FitContent)=>Some(minimum.max(available.unwrap_or(maximum)).min(maximum)),
        Self::FitContent(_)=>Some(minimum.max(self.numeric(available).unwrap_or(maximum)).min(maximum)),
    }}
}
fn expression(raw:&str,computed:Option<(LengthContext,ContainerUnitContext)>)->Option<Arc<typed_numeric::NumericExpression>> {
    use typed_numeric::{NumericUnit as Unit,NumericType,NumericValue};
    let mut value=typed_numeric::parse_numeric_expression(raw)?;
    if let Some(leaf)=value.single_numeric_value() {
        if leaf.unit==Unit::Number && leaf.value==0.0 && !math_function(raw) {value=typed_numeric::NumericExpression::Value(NumericValue{unit:Unit::Px,value:0.0});}
        else if leaf.value<0.0 && !math_function(raw){return None;}
    }
    let compatible=value.numeric_type()?.add(NumericType::from_unit(Unit::Px))?;
    if compatible.length!=1||compatible.angle!=0||compatible.time!=0||compatible.frequency!=0||compatible.resolution!=0||compatible.flex!=0||compatible.percent!=0{return None;}
    if let Some((context,query))=computed {
        value.simplify_absolute_units();
        value.map_numeric_values(|leaf|typed_numeric::computed_numeric_value(leaf,context,query))?;
        value.simplify_absolute_units();
        // A fully computed primitive has no remaining used percentage basis.
        // Remove its authored calc() boundary and apply the property's range
        // once, without clamping intermediate leaves in mixed calculations.
        if let Some(mut leaf)=value.single_numeric_value() {
            leaf.value=if leaf.value.is_finite(){leaf.value.clamp(0.0,f64::from(f32::MAX))}
                else{f64::from(typed_numeric::computed_f32(leaf.value)).max(0.0)};
            value=typed_numeric::NumericExpression::Value(leaf);
        }
    }
    Some(Arc::new(value))
}
fn width(raw:&str,computed:Option<(LengthContext,ContainerUnitContext)>)->Option<ColumnWidth> {
    if decoded_css_keyword(raw,"auto"){return Some(ColumnWidth::Auto);}
    if decoded_css_keyword(raw,"stretch"){return Some(ColumnWidth::Stretch);}
    if let Some(value)=intrinsic_sizing(raw){return Some(ColumnWidth::Intrinsic(value));}
    if let Some((_,body))=generated_content_function(raw).filter(|(name,_)|decoded_css_keyword(name,"fit-content")){return expression(body,computed).map(ColumnWidth::FitContent);}
    expression(raw,computed).map(ColumnWidth::Length)
}
fn width_declaration(raw:&str)->Option<Value> {
    let parsed=width(raw,None)?;
    Some(if matches!(parsed,ColumnWidth::Auto|ColumnWidth::Intrinsic(_)|ColumnWidth::Stretch){Value::ColumnWidth(parsed)}
        else{Value::ContextLength(238,raw.into(),true)})
}
pub(super) fn resolve(raw:&str,context:LengthContext,query:ContainerUnitContext)->Option<Value> {width(raw,Some((context,query))).map(Value::ColumnWidth)}
pub(super) fn declarations(name:&str,raw:&str,important:bool)->Option<Vec<Declaration>> {
    if name=="column-width"{return Some(alloc::vec![Declaration{value:width_declaration(raw)?,important}]);}
    let (width,count)=parts(raw)?;
    Some(alloc::vec![Declaration{value:width_declaration(width.unwrap_or("auto"))?,important},Declaration{value:column_count_value(count.unwrap_or("auto"))?,important}])
}
fn parts(raw:&str)->Option<(Option<&str>,Option<&str>)> {
    let tokens=components(raw).filter(|tokens|(1..=2).contains(&tokens.len()))?;
    let mut w=None;let mut c=None;let mut autos=0;
    for token in tokens {if decoded_css_keyword(token,"auto"){autos+=1;continue;}
        if c.is_none() && column_count_value(token).is_some(){c=Some(token);}
        else if w.is_none() && width(token,None).is_some(){w=Some(token);}
        else{return None;}
    }
    (autos+usize::from(w.is_some())+usize::from(c.is_some())<=2).then_some((w,c))
}
pub(super) fn numeric_input(raw:&str)->&str {generated_content_function(raw).filter(|(name,_)|decoded_css_keyword(name,"fit-content")).map_or(raw,|(_,body)|body)}
fn source_width(raw:&str)->Option<String> {Some(match width(raw,None)? {
    ColumnWidth::Length(value)=>(*value).clone().serialize_specified()?,
    ColumnWidth::FitContent(value)=>alloc::format!("fit-content({})",(*value).clone().serialize_specified()?),
    value=>value.serialize()?,
})}
pub(super) fn shorthand_value(width:&str,count:&str)->String {
    if width=="auto" {count.into()}else if count=="auto"{width.into()}else{alloc::format!("{width} {count}")}
}
pub(super) fn specified(name:&str,raw:&str)->Option<String> {
    if let Some(keyword)=["initial","inherit","unset","revert","revert-layer"].into_iter().find(|keyword|decoded_css_keyword(raw,keyword)){return Some(keyword.into());}
    if name=="column-width"{return source_width(raw);}
    if name=="column-count"{return count_source(raw);}
    let (w,c)=parts(raw)?;
    Some(shorthand_value(&source_width(w.unwrap_or("auto"))?,&count_source(c.unwrap_or("auto"))?))
}
fn count_source(raw:&str)->Option<String> {
    if decoded_css_keyword(raw,"auto"){Some("auto".into())}
    else {column_count_value(raw)?;typed_numeric::parse_numeric_expression(raw)?.serialize_specified()}
}
pub(super) fn specified_component(origin:&str,target:&str,raw:&str)->Option<String> {
    if origin!="columns"{return specified(target,raw);}
    let (w,c)=parts(raw)?;
    if target=="column-width"{source_width(w.unwrap_or("auto"))}else{count_source(c.unwrap_or("auto"))}
}
/// CSS Multicol §3.4. Cap only the renderer's used work, preserving valid
/// computed declarations and the source-selected explicit count.
pub(crate) fn used(style:&Style,available:f32,minimum:f32,maximum:f32,gap:f32)->(usize,f32) {
    let gap=gap.max(0.0);
    let n=match style.column_width.optimal(Some(available),minimum,maximum) {
        Some(optimal)=>{let fit=((available+gap)/(optimal.max(1.0)+gap)).floor().max(1.0) as usize;style.column_count.map_or(fit,|count|count.min(fit))},
        None=>style.column_count.unwrap_or(1),
    }.clamp(1,64);
    (n,((available+gap)/n as f32-gap).max(0.0))
}

/// The properties apply to block containers. Flex, grid and table layout
/// retain their own formatting contexts even if they carry these values.
pub(crate) fn active(style:&Style)->bool {
    matches!(style.display,Display::Block|Display::FlowRoot|Display::InlineBlock)
        && (style.column_count.is_some() || !matches!(style.column_width,ColumnWidth::Auto))
}
pub(crate) fn clear(style:&mut Style) {
    style.column_count=None;
    if !matches!(style.column_width,ColumnWidth::Auto){style.column_width=ColumnWidth::Auto;}
}
pub(crate) fn intrinsic_inline(style:&Style,minimum:f32,maximum:f32,is_minimum:bool)->f32 {
    let count=style.column_count.unwrap_or(1).max(1) as f32;
    let gap=style.column_gap.unwrap_or(style.font_size).max(0.0);
    let width=style.column_width.optimal(None,minimum,maximum);
    let column=if is_minimum {width.map_or(minimum,|width|minimum.min(width))}
        else {width.map_or(maximum,|width|maximum.max(width))};
    typed_numeric::computed_f32(f64::from(column)*f64::from(count)+f64::from(gap)*f64::from(count-1.0))
}
