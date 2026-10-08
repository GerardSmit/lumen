//! Shared explicit intrinsic-size overrides; remembered sizes belong to the
//! element's ResizeObserver/content-skipping owner, never to computed Style.
use super::*;
#[derive(Clone,Copy,Debug,Default,PartialEq)]
pub struct IntrinsicOverride {pub automatic:bool,pub size:Option<f32>}
impl IntrinsicOverride {
    pub(super) fn serialize(self)->String {
        let size=self.size.map_or_else(||String::from("none"),|size|alloc::format!("{}px",computed_values::number(size)));
        if self.automatic {alloc::format!("auto {size}")}else{size}
    }
}
pub(super) fn numeric_input(raw:&str)->Option<&str> {
    let parts=components(raw)?;
    match parts.as_slice(){[size]=>Some(*size),[auto,size]if decoded_css_keyword(auto,"auto")=>Some(*size),_=>None}
}
fn computed(raw:&str,context:LengthContext,query:ContainerUnitContext)->Option<IntrinsicOverride> {
    let size=numeric_input(raw)?;let automatic=components(raw)?.len()==2;
    if decoded_css_keyword(size,"none"){return Some(IntrinsicOverride{automatic,size:None});}
    let expression=typed_numeric::parse_numeric_expression(size)?;
    if expression.numeric_type()?!=typed_numeric::NumericType::from_unit(typed_numeric::NumericUnit::Px)
        && !typed_numeric::parse_numeric_value(size).is_some_and(|value|value.unit==typed_numeric::NumericUnit::Number&&value.value==0.0){return None;}
    let value=contextual_length_with_query(size,Some(context),true,query)?;
    if value<0.0&&!math_function(size){return None;}
    Some(IntrinsicOverride{automatic,size:Some(value.max(0.0))})
}
pub(super) fn resolve(slot:usize,raw:&str,context:LengthContext,query:ContainerUnitContext)->Option<Value> {
    computed(raw,context,query).map(|value|Value::ContainIntrinsic(slot,value))
}
pub(super) fn value(slot:usize,raw:&str)->Option<Value> {
    let context=static_length_context();let resolved=resolve(slot,raw,context,ContainerUnitContext::no_container(context.viewport))?;
    let size=numeric_input(raw)?;
    if decoded_css_keyword(size,"none")||length_independent(size){Some(resolved)}else{Some(Value::ContextLength(slot,Box::from(raw),true))}
}
fn axis_inputs(raw:&str)->Option<Vec<String>> {
    let parts=components(raw)?;let mut at=0;let mut axes=Vec::new();
    while at<parts.len() {
        let start=at;at+=1;if decoded_css_keyword(parts[start],"auto"){at+=1;}
        if at>parts.len()||axes.len()==2{return None;}
        let axis=parts[start..at].join(" ");axes.push(axis);
    }
    (!axes.is_empty()).then_some(axes)
}
pub(super) fn shorthand(raw:&str)->Option<Vec<Value>> {
    let axes=axis_inputs(raw)?;
    match axes.as_slice(){[one]=>Some(alloc::vec![value(234,one)?,value(235,one)?]),[one,two]=>Some(alloc::vec![value(234,one)?,value(235,two)?]),_=>None}
}
pub(super) fn specified(raw:&str)->Option<String> {
    let size=numeric_input(raw)?;let automatic=components(raw)?.len()==2;
    let size=if decoded_css_keyword(size,"none"){String::from("none")}else{
        value(234,raw)?;
        if typed_numeric::parse_numeric_value(size).is_some_and(|value|value.unit==typed_numeric::NumericUnit::Number&&value.value==0.0){typed_numeric::serialize_numeric_value(0.0,typed_numeric::NumericUnit::Px)}else{typed_numeric::parse_numeric_expression(size)?.serialize()?}
    };
    Some(if automatic{alloc::format!("auto {size}")}else{size})
}
pub(super) fn specified_component(origin:&str,target:&str,raw:&str)->Option<String> {
    if origin=="contain-intrinsic-size" {
        let axes=axis_inputs(raw)?;let index=usize::from(target=="contain-intrinsic-height").min(axes.len()-1);specified(&axes[index])
    }else{specified(raw)}
}
pub(super) fn specified_property(name:&str,raw:&str)->Option<String> {
    for keyword in ["initial","inherit","unset","revert","revert-layer"] {if decoded_css_keyword(raw,keyword){return Some(keyword.into());}}
    if name=="contain-intrinsic-size" {
        let axes=axis_inputs(raw)?;let a=specified(&axes[0])?;let b=specified(axes.get(1).unwrap_or(&axes[0]))?;
        Some(if a==b{a}else{alloc::format!("{a} {b}")})
    }else if matches!(name,"contain-intrinsic-width"|"contain-intrinsic-height"|"contain-intrinsic-inline-size"|"contain-intrinsic-block-size"){specified(raw)}else{None}
}
impl Style {
    pub(super) fn stored_intrinsic_override(&self,slot:usize)->IntrinsicOverride {
        self.contain_intrinsic.as_ref().map_or(IntrinsicOverride::default(),|axes|axes[slot-234])
    }
    pub(super) fn set_intrinsic_override(&mut self,slot:usize,value:IntrinsicOverride) {
        let mut axes=self.contain_intrinsic.as_deref().copied().unwrap_or_default();
        axes[slot-234]=value;
        if axes.iter().all(|axis|*axis==IntrinsicOverride::default()) {if self.contain_intrinsic.is_some(){self.contain_intrinsic=None;}}
        else if self.contain_intrinsic.as_deref()!=Some(&axes){self.contain_intrinsic=Some(Arc::new(axes));}
    }
    /// Explicit fallback under ordinary size containment. The auto flag does
    /// not substitute a remembered size without active content skipping.
    pub(crate) fn contained_intrinsic_size(&self,horizontal:bool)->f32 {
        self.contained_intrinsic_override(horizontal).unwrap_or(0.0)
    }
    pub(crate) fn contained_intrinsic_override(&self,horizontal:bool)->Option<f32> {
        self.stored_intrinsic_override(if horizontal{234}else{235}).size
    }
    pub(super) fn computed_intrinsic_override(&self,slot:usize)->String {
        self.stored_intrinsic_override(logical_physical_slot(self,slot).unwrap_or(slot)).serialize()
    }
}
