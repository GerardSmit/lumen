//! SVG filter source compilation. The caller supplies the ordinary computed
//! primitive style; this module never implements a parallel presentation cascade.
use alloc::{sync::Arc,vec::Vec};
use lumen_common::{color::{Color,ColorSpace},filter::resource::{self,Builder,Coordinate,Input,Node,Primitive,Program,Units,EdgeMode,Composite,Transfer,BlendMode}};
use crate::{Document,NodeId,NodeKind,Namespace,svg};

#[derive(Clone,Copy)]
pub(crate) struct PrimitiveStyle {pub space:ColorSpace,pub flood:Color,pub flood_opacity:f32}
impl PrimitiveStyle {
    pub(crate) fn computed(style:&crate::css::Style)->Self {
        let properties=style.svg_filter_properties();
        Self{space:properties.interpolation.space(),flood:style.resolved_source_color(252,properties.flood).value,flood_opacity:properties.opacity}
    }
}
/// A definition's ordinary computed style and actual query bases. The source
/// compiler borrows this snapshot only while evaluating that owner; it does not
/// attach another style or expression cache to rendered nodes.
pub(crate) struct DefinitionStyle {pub style:crate::css::Style,pub query:crate::css::ContainerUnitContext}
#[derive(Clone,Copy)]
pub(crate) struct CoordinateEnvironment<'a> {pub text:Option<&'a dyn crate::paint::TextShaper>,pub environment:crate::css::MediaEnvironment,pub viewport:[f32;2]}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub(crate) enum SourceError {Missing,Unsupported,InvalidTree,BudgetExceeded}
fn source_error(error:resource::BuildError)->SourceError {match error{resource::BuildError::Invalid=>SourceError::Unsupported,resource::BuildError::BudgetExceeded=>SourceError::BudgetExceeded}}
fn primitive_name<'a>(raw:Option<&'a str>,remaining:usize)->Result<Option<alloc::borrow::Cow<'a,str>>,SourceError> {
    let Some(raw)=raw else{return Ok(None);};
    // The canonical CSS identifier decoder can expand replacement scalars.
    // Admit its bounded temporary string before invoking that shared grammar.
    crate::css::css_identifier_peak_bytes_bound(raw).filter(|bytes|*bytes<=remaining).ok_or(SourceError::BudgetExceeded)?;
    Ok(crate::css::filter_primitive_name(raw))
}
fn coordinate(raw:&str,axis:usize,units:Units,context:&crate::css::SvgCoordinateContext<'_>,viewport:[f32;2],available:usize)->Result<Coordinate,SourceError> {
    if svg::supports_length(raw) {
        return Ok(Coordinate{value:svg::coordinate_value(raw,1.0).ok_or(SourceError::Unsupported)?,percentage:raw.trim().ends_with('%')});
    }
    crate::css::typed_numeric::numeric_expression_peak_bytes_bound().filter(|bytes|*bytes<=available).ok_or(SourceError::BudgetExceeded)?;
    let basis=if units==Units::ObjectBoundingBox{1.0}else{viewport[axis%2]};
    Ok(Coordinate{value:context.coordinate(raw,basis).ok_or(SourceError::Unsupported)?,percentage:false})
}

fn source_keyword(raw:Option<&str>,keywords:&[&'static str],available:usize)->Result<Option<&'static str>,SourceError> {
    let Some(raw)=raw else{return Ok(None);};
    if raw.contains('\\') || raw.contains("/*") {
        crate::css::css_identifier_peak_bytes_bound(raw).filter(|bytes|*bytes<=available).ok_or(SourceError::BudgetExceeded)?;
    }
    Ok(crate::css::css_enum_keyword(raw,keywords))
}
fn units(raw:Option<&str>,initial:Units,available:usize)->Result<Units,SourceError> {Ok(match source_keyword(raw,&["objectBoundingBox","userSpaceOnUse"],available)? {
    Some("objectBoundingBox")=>Units::ObjectBoundingBox,Some("userSpaceOnUse")=>Units::UserSpaceOnUse,_=>initial,
})}

fn fixed_numbers<const N:usize>(raw:&str)->Result<([f32;N],usize),SourceError> {
    let mut values=[0.0;N];let mut count=0;
    svg::number_list_each(raw,N,|value|{values[count]=value;count+=1;true}).ok_or(SourceError::Unsupported)?;
    Ok((values,count))
}
/// Literal number lists use the maintained SVG lexer without heap storage.
/// CSS calculation lists borrow canonical component boundaries and hold one
/// bounded numeric tree at a time, including escaped-name decoder storage.
fn number_list_temporary(raw:&str,available:usize)->Result<usize,SourceError> {
    if svg::number_list_each(raw,usize::MAX,|_|true).is_some(){return Ok(0);}
    crate::css::numeric_list_peak_bytes_bound(raw).filter(|bytes|*bytes<=available).ok_or(SourceError::BudgetExceeded)
}
fn numbers_each(raw:&str,limit:usize,context:&crate::css::SvgCoordinateContext<'_>,temporary:usize,
    mut push:impl FnMut(f32)->bool)->Result<(),SourceError> {
    let parsed=if temporary==0{svg::number_list_each(raw,limit,push)}else{
        crate::css::numeric_list_components_each(raw,limit,|component|context.number(component).is_some_and(&mut push))
    };
    parsed.ok_or(SourceError::Unsupported)
}
fn source_numbers<const N:usize>(raw:&str,context:&crate::css::SvgCoordinateContext<'_>,available:usize)->Result<([f32;N],usize),SourceError> {
    if let Ok(values)=fixed_numbers::<N>(raw){return Ok(values);}
    let temporary=number_list_temporary(raw,available)?;
    let mut values=[0.0;N];let mut count=0;
    numbers_each(raw,N,context,temporary,|value|{values[count]=value;count+=1;true})?;
    Ok((values,count))
}
/// Invalid source <number> attributes use their own initial value. Lengths and
/// percentages are rejected unless the canonical math type cancels to Number.
fn source_number(raw:Option<&str>,initial:f32,context:&crate::css::SvgCoordinateContext<'_>,available:usize)->Result<f32,SourceError> {
    let Some(raw)=raw else{return Ok(initial);};
    match source_numbers::<1>(raw,context,available){
        Ok((values,1))=>Ok(values[0]),Ok(_)|Err(SourceError::Unsupported)=>Ok(initial),Err(error)=>Err(error),
    }
}
fn pair(raw:Option<&str>,initial:f32,context:&crate::css::SvgCoordinateContext<'_>,available:usize)->Result<[f32;2],SourceError> {
    let Some(raw)=raw else{return Ok([initial;2]);};
    match source_numbers::<2>(raw,context,available){
        Ok((values,1))=>Ok([values[0];2]),Ok((values,2))=>Ok(values),
        Ok(_)|Err(SourceError::Unsupported)=>Ok([initial;2]),Err(error)=>Err(error),
    }
}
/// Syntax recovery must not swallow construction/lookup/resource errors.
fn recovered_coordinate(result:Result<Coordinate,SourceError>,initial:Coordinate)->Result<Coordinate,SourceError> {
    match result{Ok(value)=>Ok(value),Err(SourceError::Unsupported)=>Ok(initial),Err(error)=>Err(error)}
}

fn private_step<'a>(builder:&mut Builder<'a>,operation:Primitive,inputs:&[Input],
    space:ColorSpace,available:usize)->Result<Input,SourceError> {
    let remaining=builder.remaining(available).ok_or(SourceError::BudgetExceeded)?;
    inputs.len().checked_mul(core::mem::size_of::<Input>()).and_then(|bytes|bytes.checked_add(2*core::mem::size_of::<usize>()))
        .and_then(|bytes|bytes.checked_add(operation.payload_bytes()?)).and_then(|bytes|bytes.checked_add(core::mem::size_of::<Node>()))
        .filter(|bytes|*bytes<=remaining).ok_or(SourceError::BudgetExceeded)?;
    builder.push_anonymous(Node{operation,inputs:Arc::from(inputs),region:[None;4],color_space:space},available).map_err(source_error)
}

fn transfer(attributes:&[(crate::Name,alloc::string::String)],context:&crate::css::SvgCoordinateContext<'_>,available:usize)->Result<Transfer,SourceError> {
    let number=|key,initial|source_number(svg::attribute(attributes,key),initial,context,available);
    match source_keyword(svg::attribute(attributes,"type"),&["identity","linear","gamma","table","discrete"],available)? {
        None|Some("identity")=>Ok(Transfer::Identity),
        Some("linear")=>Ok(Transfer::Linear{slope:number("slope",1.0)?,intercept:number("intercept",0.0)?}),
        Some("gamma")=>Ok(Transfer::Gamma{amplitude:number("amplitude",1.0)?,exponent:number("exponent",1.0)?,offset:number("offset",0.0)?}),
        Some(kind @ ("table"|"discrete"))=>{
            let raw=svg::attribute(attributes,"tableValues").unwrap_or("");
            if raw.trim().is_empty(){return Ok(Transfer::Identity);}
            let temporary=number_list_temporary(raw,available)?;
            let table_available=available.checked_sub(temporary).ok_or(SourceError::BudgetExceeded)?;
            let mut table=Vec::<f32>::new();let mut budget_failed=false;
            let limit=usize::MAX;
            let parsed=numbers_each(raw,limit,context,temporary,|value|{
                if table.len()==table.capacity(){
                    let Some(capacity)=table.len().checked_add(1).and_then(|count|table.capacity().checked_mul(2).map(|grown|count.max(grown))) else{budget_failed=true;return false;};
                    if capacity.checked_add(table.capacity()).and_then(|count|count.checked_mul(core::mem::size_of::<f32>())).is_none_or(|bytes|bytes>table_available){budget_failed=true;return false;}
                    if table.try_reserve_exact(capacity-table.len()).is_err(){budget_failed=true;return false;}
                    if table.capacity().checked_mul(core::mem::size_of::<f32>()).is_none_or(|bytes|bytes>table_available){budget_failed=true;return false;}
                }
                table.push(value);true
            });
            if budget_failed{return Err(SourceError::BudgetExceeded);}
            if parsed.is_err(){return Ok(Transfer::Identity);}
            table.capacity().checked_add(table.len()).and_then(|count|count.checked_mul(core::mem::size_of::<f32>()))
                .and_then(|bytes|bytes.checked_add(2*core::mem::size_of::<usize>())).filter(|bytes|*bytes<=available).ok_or(SourceError::BudgetExceeded)?;
            let values=table.into();Ok(if kind=="table"{Transfer::Table(values)}else{Transfer::Discrete(values)})
        }
        _=>Ok(Transfer::Identity),
    }
}

pub(crate) fn compile(document:&Document,scope:NodeId,id:&str,available:usize,context:CoordinateEnvironment<'_>,
    style:impl FnMut(NodeId)->Result<DefinitionStyle,SourceError>)->Result<Arc<Program>,SourceError> {
    let target=crate::selector::get_element_by_id(document,scope,id).map_err(|_|SourceError::InvalidTree)?.ok_or(SourceError::Missing)?;
    compile_target(document,target,available,context,style)
}

/// Resolve local fragment names through the actual declaring tree chain. A
/// matching ID of the wrong resource type terminates resolution; it must not
/// expose a same-named filter in an ancestor tree.
pub(crate) fn compile_local(document:&Document,scopes:Option<&[NodeId]>,href:&str,available:usize,context:CoordinateEnvironment<'_>,
    style:impl FnMut(NodeId)->Result<DefinitionStyle,SourceError>)->Result<Arc<Program>,SourceError> {
    let fragment=href.strip_prefix('#').ok_or(SourceError::Missing)?;
    if fragment.len()>available{return Err(SourceError::BudgetExceeded);}
    let decoded=lumen_common::codec::percent_decode(fragment.as_bytes());
    let available=available.checked_sub(decoded.capacity()).ok_or(SourceError::BudgetExceeded)?;
    let id=core::str::from_utf8(&decoded).map_err(|_|SourceError::Missing)?;
    if id.is_empty(){return Err(SourceError::Missing);}
    for &scope in scopes.unwrap_or(&[]) {
        if let Some(target)=crate::selector::get_element_by_id(document,scope,id).map_err(|_|SourceError::InvalidTree)? {
            return compile_target(document,target,available,context,style);
        }
    }
    // The canonical declaring scope chain excludes the document sentinel.
    compile(document,document.root(),id,available,context,style)
}

fn compile_target(document:&Document,target:NodeId,available:usize,context:CoordinateEnvironment<'_>,
    mut style:impl FnMut(NodeId)->Result<DefinitionStyle,SourceError>)->Result<Arc<Program>,SourceError> {
    let NodeKind::Element{namespace:Namespace::Svg,name,attributes}=document.kind(target).map_err(|_|SourceError::InvalidTree)? else{return Err(SourceError::Missing);};
    if svg::local_name(name)!="filter" {return Err(SourceError::Missing);}
    let filter_units=units(svg::attribute(attributes,"filterUnits"),Units::ObjectBoundingBox,available)?;
    let primitive_units=units(svg::attribute(attributes,"primitiveUnits"),Units::UserSpaceOnUse,available)?;
    let mut region=[Coordinate{value:-0.1,percentage:true},Coordinate{value:-0.1,percentage:true},Coordinate{value:1.2,percentage:true},Coordinate{value:1.2,percentage:true}];
    {
        let definition=style(target)?;
        let coordinates=crate::css::SvgCoordinateContext::for_owner(document,target,&[],&definition.style,context.text,context.environment,definition.query);
        for (axis,key) in ["x","y","width","height"].into_iter().enumerate() {if let Some(raw)=svg::attribute(attributes,key){region[axis]=recovered_coordinate(coordinate(raw,axis,filter_units,&coordinates,context.viewport,available),region[axis])?;}}
    }
    let mut builder=Builder::new();
    let mut child=document.first_child(target).map_err(|_|SourceError::InvalidTree)?;
    while let Some(node)=child {
        child=document.next_sibling(node).map_err(|_|SourceError::InvalidTree)?;
        let NodeKind::Element{namespace:Namespace::Svg,name,attributes}=document.kind(node).map_err(|_|SourceError::InvalidTree)? else{continue;};
        let tag=svg::local_name(name);if matches!(tag,"desc"|"title"|"metadata"|"animate"|"set"|"script"){continue;}
        let definition=style(node)?;
        let computed=PrimitiveStyle::computed(&definition.style);
        let coordinates=crate::css::SvgCoordinateContext::for_owner(document,node,&[],&definition.style,context.text,context.environment,definition.query);
        let input_name=primitive_name(svg::attribute(attributes,"in"),builder.remaining(available).ok_or(SourceError::BudgetExceeded)?)?;
        let input=builder.input(input_name.as_deref());drop(input_name);
        let mut inputs=Vec::new();
        // Small fixed-input parameter storage is preflighted before allocation.
        let fixed_bytes=2*core::mem::size_of::<Input>()+2*core::mem::size_of::<usize>();
        let remaining=builder.remaining(available).ok_or(SourceError::BudgetExceeded)?;
        if remaining<fixed_bytes+core::mem::size_of::<Node>(){return Err(SourceError::BudgetExceeded);}
        inputs.try_reserve_exact(2).map_err(|_|SourceError::BudgetExceeded)?;
        inputs.capacity().checked_mul(core::mem::size_of::<Input>()).filter(|bytes|*bytes<=remaining).ok_or(SourceError::BudgetExceeded)?;
        let parameter_available=inputs.capacity().checked_mul(core::mem::size_of::<Input>()).and_then(|bytes|remaining.checked_sub(bytes)).and_then(|bytes|bytes.checked_sub(core::mem::size_of::<Node>())).ok_or(SourceError::BudgetExceeded)?;
        let number=|key,initial|source_number(svg::attribute(attributes,key),initial,&coordinates,parameter_available);
        let operation=match tag {
            "feGaussianBlur"=>{
                let sigma=pair(svg::attribute(attributes,"stdDeviation"),0.0,&coordinates,parameter_available)?;
                if sigma.iter().any(|v|*v<0.0){inputs.push(input);Primitive::GaussianBlur{sigma:[0.0;2],edge:EdgeMode::None}}else{
                    let edge=match source_keyword(svg::attribute(attributes,"edgeMode"),&["none","duplicate","wrap","mirror"],parameter_available)?{None|Some("none")=>EdgeMode::None,Some("duplicate")=>EdgeMode::Duplicate,Some("wrap")=>EdgeMode::Wrap,Some("mirror")=>EdgeMode::Mirror,_=>EdgeMode::None};
                    inputs.push(input);Primitive::GaussianBlur{sigma,edge}
                }
            }
            "feColorMatrix"=>{
                use lumen_common::filter::{ColorFilter,ColorMatrix};
                // SVG 2 §4.2: invalid enumerated attributes recover to their
                // initial value. Unknown type is matrix, not an unknown effect.
                let kind=source_keyword(svg::attribute(attributes,"type"),&["matrix","saturate","hueRotate","luminanceToAlpha"],parameter_available)?.unwrap_or("matrix");
                let matrix=match kind {
                    "matrix"=>{
                        let available=parameter_available.checked_sub(core::mem::size_of::<[[f32;5];4]>()+2*core::mem::size_of::<usize>()).ok_or(SourceError::BudgetExceeded)?;
                        match svg::attribute(attributes,"values").map(|raw|source_numbers::<20>(raw,&coordinates,available)).transpose(){
                            Ok(Some((values,20)))=>core::array::from_fn(|row|core::array::from_fn(|column|values[row*5+column])),
                            Ok(_)|Err(SourceError::Unsupported)=>ColorMatrix::IDENTITY.coefficients(),Err(error)=>return Err(error),
                        }
                    },
                    kind @ ("saturate"|"hueRotate")=>{
                        let value=number("values",if kind=="saturate"{1.0}else{0.0})?;
                        if kind=="saturate"{ColorFilter::Saturate(value)}else{ColorFilter::HueRotate(value)}.matrix().coefficients()
                    }
                    "luminanceToAlpha"=>[[0.0;5],[0.0;5],[0.0;5],[0.2126,0.7152,0.0722,0.0,0.0]],
                    _=>return Err(SourceError::Unsupported),
                };
                remaining.checked_sub(core::mem::size_of::<[[f32;5];4]>()+2*core::mem::size_of::<usize>()+fixed_bytes+core::mem::size_of::<Node>()).ok_or(SourceError::BudgetExceeded)?;
                inputs.push(input);Primitive::Matrix(Arc::new(matrix))
            }
            "feComponentTransfer"=>{
                let mut functions:[Transfer;4]=core::array::from_fn(|_|Transfer::Identity);
                let mut function=document.first_child(node).map_err(|_|SourceError::InvalidTree)?;
                while let Some(entry)=function {
                    function=document.next_sibling(entry).map_err(|_|SourceError::InvalidTree)?;
                    if let NodeKind::Element{namespace:Namespace::Svg,name,attributes}=document.kind(entry).map_err(|_|SourceError::InvalidTree)? {
                        let channel=match svg::local_name(name){"feFuncR"=>0,"feFuncG"=>1,"feFuncB"=>2,"feFuncA"=>3,_=>continue};
                        let retained=functions.iter().try_fold(core::mem::size_of::<[Transfer;4]>()+2*core::mem::size_of::<usize>()+fixed_bytes+core::mem::size_of::<Node>(),|bytes,value|bytes.checked_add(match value{Transfer::Table(values)|Transfer::Discrete(values)=>values.len().checked_mul(core::mem::size_of::<f32>())?.checked_add(2*core::mem::size_of::<usize>())?,_=>0})).ok_or(SourceError::BudgetExceeded)?;
                        // Previous duplicate channel values remain live while
                        // compiling their replacement; charge that actual peak.
                        let function_definition=style(entry)?;
                        let function_coordinates=crate::css::SvgCoordinateContext::for_owner(document,entry,&[],&function_definition.style,context.text,context.environment,function_definition.query);
                        functions[channel]=transfer(attributes,&function_coordinates,remaining.checked_sub(retained).ok_or(SourceError::BudgetExceeded)?)?;
                    }
                }
                inputs.push(input);Primitive::ComponentTransfer(Arc::new(functions))
            }
            "feDropShadow"=>{
                // Filter Effects §9.12 defines the shorthand by these private
                // primitive steps. Source DOM and result names remain intact.
                let mut sigma=pair(svg::attribute(attributes,"stdDeviation"),2.0,&coordinates,parameter_available)?;
                if sigma.iter().any(|value|*value<0.0){sigma=[0.0;2];}
                let offset=[number("dx",2.0)?,number("dy",2.0)?];
                let mut flood=computed.flood;flood.alpha*=computed.flood_opacity;
                let matrix=[[0.0;5],[0.0;5],[0.0;5],[0.0,0.0,0.0,1.0,0.0]];
                remaining.checked_sub(core::mem::size_of::<[[f32;5];4]>()+2*core::mem::size_of::<usize>()+fixed_bytes+core::mem::size_of::<Node>()).ok_or(SourceError::BudgetExceeded)?;
                let private_available=available.checked_sub(inputs.capacity()*core::mem::size_of::<Input>()).ok_or(SourceError::BudgetExceeded)?;
                let alpha=private_step(&mut builder,Primitive::Matrix(Arc::new(matrix)),&[input],computed.space,private_available)?;
                let blur=private_step(&mut builder,Primitive::GaussianBlur{sigma,edge:EdgeMode::None},&[alpha],computed.space,private_available)?;
                let displaced=private_step(&mut builder,Primitive::Offset(offset),&[blur],computed.space,private_available)?;
                let paint=private_step(&mut builder,Primitive::Flood(flood),&[],computed.space,private_available)?;
                let shadow=private_step(&mut builder,Primitive::Composite(Composite::In),&[paint,displaced],computed.space,private_available)?;
                inputs.push(shadow);inputs.push(input);Primitive::DropShadowMerge{original:input}
            }
            "feOffset"=>{inputs.push(input);Primitive::Offset([number("dx",0.0)?,number("dy",0.0)?])}
            "feFlood"=>{let mut flood=computed.flood;flood.alpha*=computed.flood_opacity;Primitive::Flood(flood)}
            "feBlend"=>{
                inputs.push(input);let second=primitive_name(svg::attribute(attributes,"in2"),parameter_available)?;
                inputs.push(builder.input(second.as_deref()));drop(second);
                // Invalid enumerated attributes recover to the initial mode.
                let mode=source_keyword(svg::attribute(attributes,"mode"),&["normal","multiply","screen","overlay","darken","lighten","color-dodge","color-burn","hard-light","soft-light","difference","exclusion","hue","saturation","color","luminosity"],parameter_available)?.and_then(BlendMode::from_keyword).unwrap_or(BlendMode::Normal);
                Primitive::Blend{mode,composite:svg::attribute(attributes,"no-composite").is_none()}
            }
            "feComposite"=>{
                inputs.push(input);let second=primitive_name(svg::attribute(attributes,"in2"),parameter_available)?;inputs.push(builder.input(second.as_deref()));drop(second);
                let operation=match source_keyword(svg::attribute(attributes,"operator"),&["over","in","out","atop","xor","lighter","arithmetic"],parameter_available)?{None|Some("over")=>Composite::Over,Some("in")=>Composite::In,Some("out")=>Composite::Out,Some("atop")=>Composite::Atop,Some("xor")=>Composite::Xor,Some("lighter")=>Composite::Lighter,
                    Some("arithmetic")=>Composite::Arithmetic([number("k1",0.0)?,number("k2",0.0)?,number("k3",0.0)?,number("k4",0.0)?]),_=>Composite::Over};
                Primitive::Composite(operation)
            }
            "feMerge"=>{
                let mut merge=document.first_child(node).map_err(|_|SourceError::InvalidTree)?;
                while let Some(entry)=merge {
                    merge=document.next_sibling(entry).map_err(|_|SourceError::InvalidTree)?;
                    if let NodeKind::Element{namespace:Namespace::Svg,name,attributes}=document.kind(entry).map_err(|_|SourceError::InvalidTree)? {
                        if svg::local_name(name)=="feMergeNode" {
                            let required=inputs.len().checked_add(1).ok_or(SourceError::BudgetExceeded)?;
                            let capacity=if required>inputs.capacity(){required.max(inputs.capacity().saturating_mul(2))}else{inputs.capacity()};
                            if capacity>inputs.capacity(){capacity.checked_add(inputs.capacity()).and_then(|count|count.checked_mul(core::mem::size_of::<Input>())).filter(|bytes|*bytes<=remaining).ok_or(SourceError::BudgetExceeded)?;inputs.try_reserve_exact(capacity-inputs.len()).map_err(|_|SourceError::BudgetExceeded)?;}
                            let name_available=inputs.capacity().checked_mul(core::mem::size_of::<Input>()).and_then(|bytes|remaining.checked_sub(bytes)).and_then(|bytes|bytes.checked_sub(core::mem::size_of::<Node>())).ok_or(SourceError::BudgetExceeded)?;
                            let input_name=primitive_name(svg::attribute(attributes,"in"),name_available)?;inputs.push(builder.input(input_name.as_deref()));
                        }
                    }
                }Primitive::Merge
            }
            // All other primitives remain unsupported until their actual
            // typed operation and pixel consumer are joined to the shared DAG.
            _=>return Err(SourceError::Unsupported),
        };
        let coordinate_available=builder.remaining(available).and_then(|bytes|bytes.checked_sub(inputs.capacity().checked_mul(core::mem::size_of::<Input>())?)).and_then(|bytes|bytes.checked_sub(operation.payload_bytes()?)).ok_or(SourceError::BudgetExceeded)?;
        let mut subregion=[None;4];for (axis,key) in ["x","y","width","height"].into_iter().enumerate(){if let Some(raw)=svg::attribute(attributes,key){subregion[axis]=Some(recovered_coordinate(coordinate(raw,axis,primitive_units,&coordinates,context.viewport,coordinate_available),Coordinate{value:if axis<2{0.0}else{1.0},percentage:true})?);}}
        // Converting the input Vec to an Arc allocates its final slice while
        // the temporary vector is still live. Charge that actual peak first.
        let remaining=builder.remaining(available).ok_or(SourceError::BudgetExceeded)?;
        let payload=operation.payload_bytes().ok_or(SourceError::BudgetExceeded)?;
        let input_bytes=inputs.len().checked_mul(core::mem::size_of::<Input>()).and_then(|bytes|bytes.checked_add(2*core::mem::size_of::<usize>())).ok_or(SourceError::BudgetExceeded)?;
        inputs.capacity().checked_mul(core::mem::size_of::<Input>()).and_then(|bytes|bytes.checked_add(input_bytes))
            .and_then(|bytes|bytes.checked_add(payload)).filter(|bytes|*bytes<=remaining).ok_or(SourceError::BudgetExceeded)?;
        let inputs=inputs.into();
        let result_budget=remaining.checked_sub(payload).and_then(|bytes|bytes.checked_sub(input_bytes)).ok_or(SourceError::BudgetExceeded)?;
        let result=primitive_name(svg::attribute(attributes,"result"),result_budget)?;
        builder.push(Node{operation,inputs,region:subregion,color_space:computed.space},result,available).map_err(source_error)?;
    }
    builder.finish(region,filter_units,primitive_units,available).map_err(source_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn computed(document:&Document,node:NodeId,index:&crate::css::StyleIndex)->DefinitionStyle {
        let mut ancestors=Vec::new();let mut current=Some(node);
        while let Some(node)=current{ancestors.push(node);current=document.parent(node).unwrap();}
        let mut parent=None;
        for node in ancestors.into_iter().rev(){if matches!(document.kind(node).unwrap(),NodeKind::Element{..}){parent=Some(crate::css::compute_node(document,node,parent.as_ref(),index).unwrap());}}
        DefinitionStyle{style:parent.unwrap(),query:crate::css::ContainerUnitContext::no_container(index.environment)}
    }
    fn coordinate_environment()->CoordinateEnvironment<'static>{CoordinateEnvironment{text:None,environment:crate::css::MediaEnvironment::default(),viewport:[200.0,100.0]}}
    #[test]
    fn specification_svg_filter_invalid_attributes_recover_per_owner_without_swallowing_budget() {
        let invalid="<filter id='f' filterUnits='unknown' primitiveUnits='unknown' x='1s' y='1deg' width='bogus' height='bogus'><feFlood/><feGaussianBlur stdDeviation='bogus' edgeMode='unknown'/><feColorMatrix type='unknown' values='bogus'/><feColorMatrix type='hueRotate' values='1 2'/><feOffset dx='10px' dy='20%'/><feComposite operator='unknown'/><feComposite operator='arithmetic' k1='bogus' k2='1px' k3='2%' k4='1 2'/><feComponentTransfer><feFuncR type='unknown'/><feFuncG type='linear' slope='1px' intercept='1%'/><feFuncB type='gamma' amplitude='bogus' exponent='1s' offset='1deg'/><feFuncA type='table' tableValues='0 invalid 1'/></feComponentTransfer></filter>";
        let initial="<filter id='f'><feFlood/><feGaussianBlur/><feColorMatrix/><feColorMatrix type='hueRotate'/><feOffset/><feComposite/><feComposite operator='arithmetic'/><feComponentTransfer><feFuncR type='identity'/><feFuncG type='linear'/><feFuncB type='gamma'/><feFuncA type='identity'/></feComponentTransfer></filter>";
        let compile_source=|markup| {
            let document=crate::xml::parse(&alloc::format!("<svg xmlns='http://www.w3.org/2000/svg'>{markup}</svg>"),64).unwrap();
            let index=crate::layout::stylesheets(&document).unwrap();
            compile_local(&document,None,"#f",65536,coordinate_environment(),|node|Ok(computed(&document,node,&index))).unwrap()
        };
        let recovered=compile_source(invalid);let expected=compile_source(initial);
        assert_eq!(recovered.region(),expected.region());assert_eq!(recovered.nodes(),expected.nodes(),"each supported primitive uses its own enumerated/numeric initial value; number attributes do not admit px or percentage values");
        let attributes=alloc::vec![("type".into(),"table".into()),("tableValues".into(),"0 1 2 3 4 5 6 7".into())];
        let initial_style=crate::css::Style::initial();let environment=crate::css::MediaEnvironment::default();
        let coordinates=crate::css::SvgCoordinateContext::new(&initial_style,None,environment,crate::css::ContainerUnitContext::no_container(environment),(1,1));
        assert_eq!(transfer(&attributes,&coordinates,0),Err(SourceError::BudgetExceeded),"resource exhaustion cannot become identity recovery");
        assert_eq!(recovered_coordinate(Err(SourceError::BudgetExceeded),Coordinate{value:0.0,percentage:true}),Err(SourceError::BudgetExceeded));
        let unknown=crate::xml::parse("<svg xmlns='http://www.w3.org/2000/svg'><filter id='f'><feDisplacementMap/></filter></svg>",16).unwrap();let index=crate::layout::stylesheets(&unknown).unwrap();
        assert!(matches!(compile_local(&unknown,None,"#f",65536,coordinate_environment(),|node|Ok(computed(&unknown,node,&index))),Err(SourceError::Unsupported)),"an unimplemented primitive remains unsupported");
    }

    #[test]
    fn specification_svg_filter_enums_share_css_keyword_comments_escapes_and_budget() {
        for (raw,expected) in [(" userSpaceOnUse ",Units::UserSpaceOnUse),(" /* before */ USERSPACEONUSE /**/ ",Units::UserSpaceOnUse),(r"u\73 erSpaceOnUse",Units::UserSpaceOnUse),("unknown",Units::ObjectBoundingBox),("userSpaceOnUse extra",Units::ObjectBoundingBox),("\u{a0}userSpaceOnUse\u{a0}",Units::ObjectBoundingBox)] {
            assert_eq!(units(Some(raw),Units::ObjectBoundingBox,65536),Ok(expected));
        }
        let compile_source=|attributes|{
            let document=crate::xml::parse(&alloc::format!("<svg xmlns='http://www.w3.org/2000/svg'><filter id='f' {attributes}></filter></svg>"),32).unwrap();
            let index=crate::layout::stylesheets(&document).unwrap();
            compile_local(&document,None,"#f",65536,coordinate_environment(),|node|Ok(computed(&document,node,&index))).unwrap()
        };
        let spelled=r"filterUnits=' /**/ UserSpaceOnUse /**/' primitiveUnits=' u\73 erSpaceOnUse '> <feGaussianBlur stdDeviation='1' edgeMode=' du\70 licate /**/'/><feColorMatrix type=' SATURATE /**/' values='.5'/><feColorMatrix type=' HUErotate ' values='30'/><feComponentTransfer><feFuncR type=' l\69 near /**/' slope='2'/></feComponentTransfer><feBlend mode=' MuLtiPly /**/'/><feComposite operator=' /**/ a\72 ithmetic ' k1='.5'/>";
        let plain="filterUnits='userSpaceOnUse' primitiveUnits='userSpaceOnUse'> <feGaussianBlur stdDeviation='1' edgeMode='duplicate'/><feColorMatrix type='saturate' values='.5'/><feColorMatrix type='hueRotate' values='30'/><feComponentTransfer><feFuncR type='linear' slope='2'/></feComponentTransfer><feBlend mode='multiply'/><feComposite operator='arithmetic' k1='.5'/>";
        // The supplied text closes the opening filter to exercise actual
        // attribute owners; remove the helper's otherwise redundant close.
        let full=|text|{
            let source=alloc::format!("<svg xmlns='http://www.w3.org/2000/svg'><filter id='f' {text}</filter></svg>");
            let document=crate::xml::parse(&source,32).unwrap();let index=crate::layout::stylesheets(&document).unwrap();
            compile_local(&document,None,"#f",65536,coordinate_environment(),|node|Ok(computed(&document,node,&index))).unwrap()
        };
        assert_eq!(full(spelled).nodes(),full(plain).nodes());
        let program=compile_source("filterUnits=' userSpaceOnUse '");
        assert_eq!(program.filter_units(),Units::UserSpaceOnUse);assert_eq!(program.primitive_units(),Units::UserSpaceOnUse);
        assert_eq!(units(Some(" userSpaceOnUse "),Units::ObjectBoundingBox,0),Ok(Units::UserSpaceOnUse),"plain keyword matching has no heap peak");
        let raw=r"u\73 erSpaceOnUse";let peak=crate::css::css_identifier_peak_bytes_bound(raw).unwrap();
        assert_eq!(units(Some(raw),Units::ObjectBoundingBox,peak-1),Err(SourceError::BudgetExceeded));
        assert_eq!(units(Some(raw),Units::ObjectBoundingBox,peak),Ok(Units::UserSpaceOnUse));
    }

    #[test]
    fn specification_svg_filter_math_lists_use_actual_primitive_and_function_owners() {
        let math="<feFlood/><feGaussianBlur font-size='20px' stdDeviation='calc(1em / 1px), calc(2)'/><feColorMatrix values='calc(1) 0 0 0 0 0 calc(1) 0 0 0 0 0 calc(1) 0 0 0 0 0 calc(1) 0'/><feOffset font-size='20px' dx='calc(2 * sign(1em - 15px))' dy='sibling-index()'/><feComposite operator='arithmetic' k1='calc(-1)' k2='progress(5, 0, 10)' k3='calc(2)' k4='calc(3)'/><feComponentTransfer font-size='10px'><feFuncR type='linear' font-size='20px' slope='calc(sign(1em - 15px) * 2)' intercept='calc(.25)'/><feFuncG type='table' tableValues='calc(0) /* boundary */ calc(.5), calc(1)'/><feFuncB type='gamma' amplitude='calc(2)' exponent='calc(3)' offset='calc(.5)'/></feComponentTransfer>";
        let literal="<feFlood/><feGaussianBlur stdDeviation='20 2'/><feColorMatrix/><feOffset dx='2' dy='4'/><feComposite operator='arithmetic' k1='-1' k2='.5' k3='2' k4='3'/><feComponentTransfer><feFuncR type='linear' slope='2' intercept='.25'/><feFuncG type='table' tableValues='0 .5 1'/><feFuncB type='gamma' amplitude='2' exponent='3' offset='.5'/></feComponentTransfer>";
        let compile_source=|body|{
            let document=crate::xml::parse(&alloc::format!("<svg xmlns='http://www.w3.org/2000/svg'><filter id='f' font-size='10px'>{body}</filter></svg>"),64).unwrap();
            let index=crate::layout::stylesheets(&document).unwrap();
            compile_local(&document,None,"#f",65536,coordinate_environment(),|node|Ok(computed(&document,node,&index))).unwrap()
        };
        assert_eq!(compile_source(math).nodes(),compile_source(literal).nodes(),"valid calculations never silently recover to initial: primitive fonts, child function fonts, CSS comments, comma lists and actual element sibling position share canonical numeric evaluation");
        let initial=crate::css::Style::initial();let environment=crate::css::MediaEnvironment::default();
        let context=crate::css::SvgCoordinateContext::new(&initial,None,environment,crate::css::ContainerUnitContext::no_container(environment),(2,3));
        for raw in ["calc(1px)","calc(50%)","calc(1s)","calc(1deg)"] {
            assert_eq!(source_number(Some(raw),7.0,&context,65536),Ok(7.0),"uncancelled dimensions recover as invalid <number>");
        }
        let raw=r"c\61 lc(2)";
        assert_eq!(source_number(Some(raw),7.0,&context,65536),Ok(2.0),"escaped mathematical names use CSS Syntax authority");
        let temporary=crate::css::numeric_list_peak_bytes_bound("calc(2)").unwrap();
        assert_eq!(source_number(Some("calc(2)"),7.0,&context,temporary-1),Err(SourceError::BudgetExceeded));
        assert_eq!(source_number(Some("calc(2)"),7.0,&context,temporary),Ok(2.0));
        assert_eq!(source_number(Some("2"),7.0,&context,0),Ok(2.0),"literal number hot path remains allocation free");
    }

    #[test]
    fn specification_svg_filter_invalid_present_region_is_initial_not_missing_union() {
        let compile_source=|attributes|{
            let document=crate::xml::parse(&alloc::format!("<svg xmlns='http://www.w3.org/2000/svg'><filter id='f' x='0' y='0' width='1' height='1'><feFlood x='2' y='3' width='4' height='5'/><feOffset {attributes}/></filter></svg>"),32).unwrap();
            let index=crate::layout::stylesheets(&document).unwrap();
            compile_local(&document,None,"#f",65536,coordinate_environment(),|node|Ok(computed(&document,node,&index))).unwrap()
        };
        let absent=compile_source("");let invalid=compile_source("x='1s' y='bogus' width='1deg' height='bogus'");
        let initial=compile_source("x='0%' y='0%' width='100%' height='100%'");
        assert_eq!(invalid.nodes(),initial.nodes(),"SVG2 Types §4.2 treats invalid present XML attributes as specified initial values");
        assert_ne!(invalid.nodes()[1].region,absent.nodes()[1].region,"Filter Effects §9.4 input-union inference applies to missing subregion attributes");
        let used=|program|resource::Use{url:Arc::from("#f"),program,reference_box:[0.0,0.0,200.0,100.0],viewport:[0.0,0.0,200.0,100.0],owner_transform:[1.0,0.0,0.0,1.0,0.0,0.0],fill:resource::PaintInput::None,stroke:resource::PaintInput::None};
        assert_eq!(used(absent).resolved_regions(65536).unwrap()[1],Some([2.0,3.0,4.0,5.0]));
        assert_eq!(used(invalid).resolved_regions(65536).unwrap()[1],Some([0.0,0.0,200.0,100.0]));
    }

    #[test]
    fn specification_svg_color_matrix_saturation_keeps_current_unbounded_source_range() {
        for value in [-2.0,0.0,0.5,1.0,2.0] {
            let source=alloc::format!("<svg xmlns='http://www.w3.org/2000/svg'><filter id='f'><feColorMatrix type='saturate' values='{value}'/></filter></svg>");
            let document=crate::xml::parse(&source,16).unwrap();let index=crate::layout::stylesheets(&document).unwrap();
            let program=compile_local(&document,None,"#f",65536,coordinate_environment(),|node|Ok(computed(&document,node,&index))).unwrap();
            let Primitive::Matrix(matrix)=&program.nodes()[0].operation else{panic!("shared matrix operation")};
            assert_eq!(**matrix,lumen_common::filter::ColorFilter::Saturate(value).matrix().coefficients(),"current Filter Effects §9.6 permits under/oversaturation outside 0..1");
        }
    }

    #[test]
    fn specification_svg_filter_numeric_admission_preflights_shared_temporary_budget() {
        let style=crate::css::Style::initial();let environment=crate::css::MediaEnvironment::default();
        let context=crate::css::SvgCoordinateContext::new(&style,None,environment,crate::css::ContainerUnitContext::no_container(environment),(2,3));
        let bound=crate::css::typed_numeric::numeric_expression_peak_bytes_bound().unwrap();
        assert!(bound<=65536);
        assert!(coordinate("50%",0,Units::ObjectBoundingBox,&context,[200.0,100.0],0).is_ok(),"literal fast path has no numeric heap admission");
        for raw in ["calc(25% + .25px)","calc(1px * sibling-index())"] {
            assert!(matches!(coordinate(raw,0,Units::ObjectBoundingBox,&context,[200.0,100.0],bound-1),Err(SourceError::BudgetExceeded)));
            assert!(coordinate(raw,0,Units::ObjectBoundingBox,&context,[200.0,100.0],bound).is_ok());
        }
        let large=" ".repeat(crate::css::typed_numeric::MAX_NUMERIC_EXPRESSION_BYTES+1);
        assert!(context.coordinate(&large,200.0).is_none(),"XML coordinates enforce the shared input limit before sibling scanning");
    }

    #[test]
    fn specification_svg_filter_regions_share_definition_font_and_target_axes() {
        for (units,region,primitive,expected) in [
            ("objectBoundingBox","calc(25% + .25px)","calc(10% + .1px)",[0.0,0.0,100.0,100.0]),
            ("objectBoundingBox","1em",".5",[0.0,0.0,2000.0,100.0]),
            ("userSpaceOnUse","calc(25% + 1em)","calc(10% + 1em)",[0.0,0.0,60.0,100.0]),
        ] {
            let source=alloc::format!("<svg xmlns='http://www.w3.org/2000/svg'><filter id='f' filterUnits='{units}' primitiveUnits='{units}' x='0' y='0' width='{region}' height='{}' font-size='10px'><feFlood flood-color='green' x='0' y='0' width='{primitive}' height='{}' font-size='20px'/></filter></svg>",if units=="objectBoundingBox"{"1"}else{"100"},if units=="objectBoundingBox"{"1"}else{"100"});
            let document=crate::xml::parse(&source,32).unwrap();let index=crate::layout::stylesheets(&document).unwrap();
            let program=compile_local(&document,None,"#f",65536,coordinate_environment(),|node|Ok(computed(&document,node,&index))).unwrap();
            let used=resource::Use{url:Arc::from("#f"),program,reference_box:[0.0,0.0,200.0,100.0],viewport:[0.0,0.0,200.0,100.0],owner_transform:[1.0,0.0,0.0,1.0,0.0,0.0],fill:resource::PaintInput::None,stroke:resource::PaintInput::None};
            assert_eq!(used.filter_region(),Some(expected),"{units}: region owner font is 10px independently of primitive font and target viewport");
            let expected_primitive=if units=="userSpaceOnUse"{40.0}else if primitive==".5"{100.0}else{40.0};
            assert_eq!(used.resolved_regions(65536).unwrap()[0],Some([0.0,0.0,expected_primitive,100.0]));
        }
    }

    #[test]
    fn specification_svg_blend_source_keeps_order_mode_and_no_composite() {
        for keyword in ["normal","multiply","screen","overlay","darken","lighten","color-dodge","color-burn","hard-light","soft-light","difference","exclusion","hue","saturation","color","luminosity","invalid","Multiply"] {
            for no_composite in [false,true] {
                let flag=if no_composite{" no-composite='no-composite'"}else{""};
                let source=alloc::format!("<svg xmlns='http://www.w3.org/2000/svg'><filter id='f' color-interpolation-filters='sRGB'><feFlood result='backdrop'/><feFlood result='source'/><feBlend in='source' in2='backdrop' mode='{keyword}'{flag}/></filter></svg>");
                let document=crate::xml::parse(&source,32).unwrap();let index=crate::layout::stylesheets(&document).unwrap();
                let program=compile(&document,document.root(),"f",65536,coordinate_environment(),|node|Ok(computed(&document,node,&index))).unwrap();
                let blend=&program.nodes()[2];
                assert_eq!(blend.inputs.as_ref(),&[Input::Result(1),Input::Result(0)],"input order {keyword}");
                assert_eq!(blend.operation,Primitive::Blend{mode:BlendMode::from_keyword(&keyword.to_ascii_lowercase()).unwrap_or(BlendMode::Normal),composite:!no_composite});
                assert_eq!(blend.color_space,ColorSpace::Srgb);
                assert!(program.valid(program.bytes()));assert!(!program.valid(program.bytes()-1));
            }
        }
    }
    #[test]
    fn specification_svg_filter_source_uses_actual_primitive_cascade_and_input_names(){
        let source="<svg xmlns='http://www.w3.org/2000/svg'><style>.paint {flood-color:rgb(0 255 0);flood-opacity:25%} #filter {color-interpolation-filters:sRGB}</style><defs style='display:none;color:red'><filter id='filter'><feFlood class='paint' flood-color='red' result='paint'/><feOffset in='missing' dx='-3' dy='4' result='paint'/><feComposite in='paint' in2='SourceAlpha' operator='in'/></filter></defs></svg>";
        let document=crate::xml::parse(source,64).unwrap();let index=crate::layout::stylesheets(&document).unwrap();
        let program=compile_local(&document,None,"#filter",65536,coordinate_environment(),|node|Ok(computed(&document,node,&index))).unwrap();
        assert_eq!(program.nodes().len(),3);
        assert_eq!(program.nodes()[0].color_space,ColorSpace::Srgb);
        assert_eq!(program.nodes()[0].operation,Primitive::Flood(Color::new(ColorSpace::Srgb,[0.0,1.0,0.0],0.25,0)));
        assert_eq!(program.nodes()[1].inputs.as_ref(),&[Input::Result(0)]);
        assert_eq!(program.nodes()[2].inputs.as_ref(),&[Input::Result(1),Input::SourceAlpha]);
    }
    #[test]
    fn specification_svg_drop_shadow_keeps_private_results_and_actual_style() {
        let source="<svg xmlns='http://www.w3.org/2000/svg'><filter id='f' color-interpolation-filters='sRGB'><feFlood flood-color='red' result='paint' x='2' y='3' width='4' height='5'/><feDropShadow in='paint' stdDeviation='0 .5' dx='-2' dy='3' flood-color='lime' flood-opacity='.25' result='shadow'/><feOffset in='paint'/><feOffset in='shadow'/></filter></svg>";
        let document=crate::xml::parse(source,32).unwrap();let index=crate::layout::stylesheets(&document).unwrap();
        let program=compile_local(&document,None,"#f",65536,coordinate_environment(),|node|Ok(computed(&document,node,&index))).unwrap();
        assert_eq!(program.nodes().len(),9);
        assert_eq!(program.nodes()[2].operation,Primitive::GaussianBlur{sigma:[0.0,0.5],edge:EdgeMode::None});
        assert_eq!(program.nodes()[3].operation,Primitive::Offset([-2.0,3.0]));
        assert_eq!(program.nodes()[4].operation,Primitive::Flood(Color::new(ColorSpace::Srgb,[0.0,1.0,0.0],0.25,0)));
        assert_eq!(program.nodes()[6].operation,Primitive::DropShadowMerge{original:Input::Result(0)});
        assert_eq!(program.nodes()[7].inputs.as_ref(),&[Input::Result(0)]);
        assert_eq!(program.nodes()[8].inputs.as_ref(),&[Input::Result(6)]);
        assert_eq!(program.nodes()[6].color_space,ColorSpace::Srgb);
        let used=resource::Use{url:Arc::from("#f"),program,reference_box:[0.0,0.0,20.0,20.0],viewport:[0.0,0.0,20.0,20.0],owner_transform:[1.0,0.0,0.0,1.0,0.0,0.0],fill:resource::PaintInput::None,stroke:resource::PaintInput::None};
        assert_eq!(used.resolved_regions(65536).unwrap()[6],Some([2.0,3.0,4.0,5.0]),"private flood cannot enlarge public shorthand input region");
        assert!(matches!(compile_local(&document,None,"#f",256,coordinate_environment(),|node|Ok(computed(&document,node,&index))),Err(SourceError::BudgetExceeded)));
    }
    #[test]
    fn specification_svg_color_matrix_invalid_type_uses_initial_matrix_with_its_values() {
        for kind in ["", "identity", "Matrix", "unknown"] {
            for values in ["", " values='0 0 0 0 1 0 0 0 0 0 0 0 0 0 0 0 0 0 1 0'"] {
                let source=alloc::format!("<svg xmlns='http://www.w3.org/2000/svg'><filter id='f'><feColorMatrix type='{kind}'{values}/></filter></svg>");
                let document=crate::xml::parse(&source,16).unwrap();let index=crate::layout::stylesheets(&document).unwrap();
                let program=compile_local(&document,None,"#f",65536,coordinate_environment(),|node|Ok(computed(&document,node,&index))).unwrap();
                let expected=if values.is_empty(){lumen_common::filter::ColorMatrix::IDENTITY.coefficients()}else{[[0.0,0.0,0.0,0.0,1.0],[0.0;5],[0.0;5],[0.0,0.0,0.0,1.0,0.0]]};
                assert_eq!(program.nodes()[0].operation,Primitive::Matrix(Arc::new(expected)),"invalid type {kind:?} preserves actual values");
            }
        }
    }
    #[test]
    fn specification_svg_filter_transfer_last_channel_wins_and_empty_program_is_distinct_from_missing(){
        let source="<svg xmlns='http://www.w3.org/2000/svg'><filter id='empty'/><filter id='transfer'><feComponentTransfer><feFuncR type='table' tableValues='0 1'/><feFuncR type='discrete' tableValues='1 0'/><feFuncA type='gamma' exponent='2'/></feComponentTransfer></filter><g id='wrong'/></svg>";
        let document=crate::xml::parse(source,32).unwrap();let index=crate::layout::stylesheets(&document).unwrap();
        let program=compile_local(&document,None,"#transfer",65536,coordinate_environment(),|node|Ok(computed(&document,node,&index))).unwrap();
        let Primitive::ComponentTransfer(values)=&program.nodes()[0].operation else{panic!("transfer operation")};
        assert_eq!(values[0],Transfer::Discrete(Arc::from([1.0,0.0])));assert_eq!(values[1],Transfer::Identity);assert_eq!(values[2],Transfer::Identity);
        assert_eq!(values[3],Transfer::Gamma{amplitude:1.0,exponent:2.0,offset:0.0});
        assert!(compile_local(&document,None,"#empty",65536,coordinate_environment(),|node|Ok(computed(&document,node,&index))).unwrap().nodes().is_empty());
        assert!(matches!(compile_local(&document,None,"#missing",65536,coordinate_environment(),|node|Ok(computed(&document,node,&index))),Err(SourceError::Missing)));
        assert!(matches!(compile_local(&document,None,"#wrong",65536,coordinate_environment(),|node|Ok(computed(&document,node,&index))),Err(SourceError::Missing)));
        assert!(matches!(compile_local(&document,None,"#transfer",16,coordinate_environment(),|node|Ok(computed(&document,node,&index))),Err(SourceError::BudgetExceeded)));
    }
}
