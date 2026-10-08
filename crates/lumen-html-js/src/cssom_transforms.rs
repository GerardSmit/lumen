//! Live Typed OM transform components; geometry math is shared with DOMMatrix.
use super::*;
use lumen_common::dom_geometry::{Matrix,Point};
use css::typed_numeric::{NumericExpression,NumericUnit,NumericType,NumericDimension};

#[derive(Clone,Copy,PartialEq,Eq)]
enum Kind {Translate,Rotate,Scale,Skew,SkewX,SkewY,Perspective,Matrix}
#[derive(Clone,Copy)]
enum Dimension {Number,Angle,Length,LengthPercentage}

#[lumen_bind::class(name="CSSTransformComponent",hint(js(webidl)))]
pub struct DomCssTransformComponent {kind:Kind,is_2d:Cell<bool>,data_key:Value}
macro_rules! component_class {($rust:ident,$name:literal)=>{
    #[lumen_bind::class(name=$name,extends=DomCssTransformComponent,hint(js(webidl)))]
    pub struct $rust {base:DomCssTransformComponent}
};}
component_class!(DomCssTranslate,"CSSTranslate");component_class!(DomCssRotate,"CSSRotate");
component_class!(DomCssScale,"CSSScale");component_class!(DomCssSkew,"CSSSkew");
component_class!(DomCssSkewX,"CSSSkewX");component_class!(DomCssSkewY,"CSSSkewY");
component_class!(DomCssPerspective,"CSSPerspective");component_class!(DomCssMatrixComponent,"CSSMatrixComponent");

#[lumen_bind::class(name="CSSTransformValue",extends=DomCssStyleValue,hint(js(webidl)))]
pub struct DomCssTransformValue {base:DomCssStyleValue,data_key:Value,length:Cell<usize>}
#[lumen_bind::class(name="CSSTransformValueIterator")]
struct TransformIterator {owner_key:Value,kind:NumericArrayIteratorKind,index:Cell<usize>}
struct TransformConstructor(Value);
macro_rules! ctor_result {($($class:ty),*)=>{$(impl CtorRet<JsHost,$class> for TransformConstructor {
    fn into_ctor(self,_cx:&<JsHost as Host>::Cx<'_>)->Result<Value,Value>{Ok(self.0)}
})*};}
ctor_result!(DomCssTranslate,DomCssRotate,DomCssScale,DomCssSkew,DomCssSkewX,DomCssSkewY,DomCssPerspective,DomCssMatrixComponent,DomCssTransformValue);

fn numeric(ctx:&mut Ctx,value:&Value,dimension:Dimension)->OpResult<()> {
    let kind=numeric_expression_from_value(ctx,value)?.numeric_type().ok_or_else(||OpError::type_error("invalid transform numeric type"))?;
    let expected=NumericType::from_unit(match dimension{Dimension::Number=>NumericUnit::Number,Dimension::Angle=>NumericUnit::Deg,_=>NumericUnit::Px});
    let matches=match dimension {
        Dimension::LengthPercentage=>kind==expected ||kind==NumericType::from_unit(NumericUnit::Percent)
            || kind==NumericType{percent_hint:Some(NumericDimension::Length),..expected},
        _=>kind==expected,
    };
    if matches{Ok(())}else{Err(OpError::type_error("incompatible transform numeric type"))}
}
fn numberish(ctx:&mut Ctx,value:Value)->OpResult<Value>{
    if ctx.with_instance::<DomCssNumericValue,_>(&value,|_|()).is_ok(){return Ok(value);}
    let number=ctx.coerce_number(&value).map_err(OpError::thrown)?;
    if !number.is_finite(){return Err(OpError::type_error("transform numbers must be finite"));}
    Ok(unit_value(ctx,number,NumericUnit::Number))
}
fn perspective(ctx:&mut Ctx,value:Value)->OpResult<Value>{
    if ctx.with_instance::<DomCssNumericValue,_>(&value,|_|()).is_ok(){numeric(ctx,&value,Dimension::Length)?;return Ok(value);}
    let text=if let Ok(text)=ctx.with_instance::<DomCssKeywordValue,_>(&value,|value|value.base.serialized_value.borrow().clone()){text}
        else{ctx.coerce_string(&value).map_err(OpError::thrown)?.to_string()};
    if !text.eq_ignore_ascii_case("none"){return Err(OpError::type_error("perspective keyword must be none"));}
    if ctx.with_instance::<DomCssKeywordValue,_>(&value,|_|()).is_ok(){Ok(value)}else{Ok(ctx.new_instance(keyword_value(text)))}
}
fn component(ctx:&mut Ctx,kind:Kind,is_2d:bool,values:Vec<(&str,Value)>)->OpResult<Value>{
    let key=ctx.new_symbol(Some("CSSTransformComponent.data".into()));
    let base=DomCssTransformComponent{kind,is_2d:Cell::new(is_2d),data_key:key.clone()};
    let value=match kind {
        Kind::Translate=>ctx.new_instance(DomCssTranslate{base}),Kind::Rotate=>ctx.new_instance(DomCssRotate{base}),
        Kind::Scale=>ctx.new_instance(DomCssScale{base}),Kind::Skew=>ctx.new_instance(DomCssSkew{base}),
        Kind::SkewX=>ctx.new_instance(DomCssSkewX{base}),Kind::SkewY=>ctx.new_instance(DomCssSkewY{base}),
        Kind::Perspective=>ctx.new_instance(DomCssPerspective{base}),Kind::Matrix=>ctx.new_instance(DomCssMatrixComponent{base}),
    };
    let data=ctx.new_object_with_proto(&Value::Null);
    for (name,value)in values{ctx.create_data_property(&data,name,value).map_err(OpError::thrown)?;}
    define_data_property(ctx,&value,key,data,false,false,false).map_err(OpError::thrown)?;
    Ok(value)
}
fn slots(ctx:&mut Ctx,this:&Value)->OpResult<Value>{
    let key=ctx.with_instance::<DomCssTransformComponent,_>(this,|value|value.data_key.clone())?;
    ctx.reflect_get(this,&key,this).map_err(OpError::thrown)
}
fn slot(ctx:&mut Ctx,this:&Value,name:&str)->OpResult<Value>{
    let data=slots(ctx,this)?;ctx.member_get(&data,name).map_err(OpError::thrown)
}
fn set_slot(ctx:&mut Ctx,this:&Value,name:&str,value:Value,dimension:Option<Dimension>)->OpResult<()> {
    let kind=ctx.with_instance::<DomCssTransformComponent,_>(this,|value|value.kind)?;
    let value=if kind==Kind::Perspective{perspective(ctx,value)?}else if let Some(dimension)=dimension {
        if matches!(dimension,Dimension::Number){numberish(ctx,value)?}
        else{numeric(ctx,&value,dimension)?;value}
    }else{crate::geometry::validate_mutable_matrix(ctx,&value)?;value};
    let data=slots(ctx,this)?;ctx.create_data_property(&data,name,value).map_err(OpError::thrown)?;Ok(())
}
fn scalar(ctx:&mut Ctx,value:&Value,unit:NumericUnit)->OpResult<f64>{
    let mut expression=numeric_expression_from_value(ctx,value)?;expression.simplify_absolute_units();
    let value=expression.single_numeric_value().ok_or_else(||OpError::type_error("matrix needs an absolute numeric value"))?;
    let (canonical,factor)=value.unit.canonical_unit_and_factor().ok_or_else(||OpError::type_error("matrix cannot resolve relative units"))?;
    if canonical!=unit{return Err(OpError::type_error("matrix cannot resolve relative or percentage units"));}
    Ok(value.value*factor)
}
fn slot_number(ctx:&mut Ctx,this:&Value,name:&str,unit:NumericUnit)->OpResult<f64>{let value=slot(ctx,this,name)?;scalar(ctx,&value,unit)}
fn matrix(ctx:&mut Ctx,this:&Value)->OpResult<Matrix>{
    let (kind,is_2d)=ctx.with_instance::<DomCssTransformComponent,_>(this,|value|(value.kind,value.is_2d.get()))?;
    let mut matrix=match kind {
        Kind::Translate=>{let x=slot_number(ctx,this,"x",NumericUnit::Px)?;let y=slot_number(ctx,this,"y",NumericUnit::Px)?;
            let z=if is_2d{0.0}else{slot_number(ctx,this,"z",NumericUnit::Px)?};Matrix::default().translated(x,y,z)},
        Kind::Scale=>{let x=slot_number(ctx,this,"x",NumericUnit::Number)?;let y=slot_number(ctx,this,"y",NumericUnit::Number)?;
            let z=if is_2d{1.0}else{slot_number(ctx,this,"z",NumericUnit::Number)?};Matrix::default().scaled(x,y,z,Point::default())},
        Kind::Rotate=>{let angle=slot_number(ctx,this,"angle",NumericUnit::Deg)?;if is_2d{Matrix::default().rotated(0.0,0.0,angle)}else{
            let x=slot_number(ctx,this,"x",NumericUnit::Number)?;let y=slot_number(ctx,this,"y",NumericUnit::Number)?;let z=slot_number(ctx,this,"z",NumericUnit::Number)?;
            Matrix::default().rotated_axis(x,y,z,angle)}},
        Kind::Skew|Kind::SkewX|Kind::SkewY=>{let x=if kind==Kind::SkewY{0.0}else{slot_number(ctx,this,"ax",NumericUnit::Deg)?};
            let y=if kind==Kind::SkewX{0.0}else{slot_number(ctx,this,"ay",NumericUnit::Deg)?};Matrix::default().skewed(x,y)},
        Kind::Perspective=>{let value=slot(ctx,this,"length")?;let mut matrix=Matrix::default();
            if ctx.with_instance::<DomCssNumericValue,_>(&value,|_|()).is_ok(){let length=scalar(ctx,&value,NumericUnit::Px)?;
                matrix.values[11]=-1.0/length.max(1.0);}
            matrix},
        Kind::Matrix=>{let value=slot(ctx,this,"matrix")?;let matrix=crate::geometry::matrix_native_value(ctx,&value)?;
            if is_2d{Matrix::from_affine([matrix.values[0],matrix.values[1],matrix.values[4],matrix.values[5],matrix.values[12],matrix.values[13]])}else{matrix}},
    };matrix.is_2d=is_2d;Ok(matrix)
}
fn component_text(ctx:&mut Ctx,this:&Value)->OpResult<String>{
    let (kind,is_2d)=ctx.with_instance::<DomCssTransformComponent,_>(this,|value|(value.kind,value.is_2d.get()))?;
    if kind==Kind::Matrix{
        let value=matrix(ctx,this)?;
        if value.values.iter().any(|value|!value.is_finite()){return Err(OpError::type_error("nonfinite matrix cannot serialize"));}
        let values=if is_2d{vec![value.values[0],value.values[1],value.values[4],value.values[5],value.values[12],value.values[13]]}else{value.values.to_vec()};
        return Ok(format!("{}({})",if is_2d{"matrix"}else{"matrix3d"},values.into_iter().map(|value|css::typed_numeric::serialize_numeric_value(value,NumericUnit::Number)).collect::<Vec<_>>().join(", ")));
    }
    if kind==Kind::Perspective {
        let value=slot(ctx,this,"length")?;
        let length=if ctx.with_instance::<DomCssNumericValue,_>(&value,|_|()).is_ok(){numeric_value_text_with_minimum(ctx,&value,Some(css::typed_numeric::NumericValue{value:0.0,unit:NumericUnit::Px}))?}else{String::from("none")};
        return Ok(format!("perspective({length})"));
    }
    if kind==Kind::Scale && is_2d {
        let x=slot(ctx,this,"x")?;let y=slot(ctx,this,"y")?;
        if numeric_expression_from_value(ctx,&x)?==numeric_expression_from_value(ctx,&y)? {
            return Ok(format!("scale({})",numeric_value_text(ctx,&x)?));
        }
    }
    if kind==Kind::Skew {
        let y=slot(ctx,this,"ay")?;
        if ctx.with_instance::<DomCssUnitValue,_>(&y,|value|value.base.value.get()==0.0).unwrap_or(false) {
            let x=slot(ctx,this,"ax")?;return Ok(format!("skew({})",numeric_value_text(ctx,&x)?));
        }
    }
    let (name,names):(&str,&[&str])=match kind {
        Kind::Translate=>if is_2d{("translate",&["x","y"])}else{("translate3d",&["x","y","z"])},
        Kind::Scale=>if is_2d{("scale",&["x","y"])}else{("scale3d",&["x","y","z"])},
        Kind::Rotate=>if is_2d{("rotate",&["angle"])}else{("rotate3d",&["x","y","z","angle"])},
        Kind::Skew=>("skew",&["ax","ay"]),Kind::SkewX=>("skewX",&["ax"]),Kind::SkewY=>("skewY",&["ay"]),_=>unreachable!(),
    };
    let mut values=Vec::new();for name in names{let value=slot(ctx,this,name)?;values.push(numeric_value_text(ctx,&value)?);}
    Ok(format!("{name}({})",values.join(", ")))
}

#[lumen_bind::methods]
impl DomCssTransformComponent {
    #[getter(name="is2D")]fn is_2d(&self)->bool{self.is_2d.get()}
    #[setter(name="is2D")]fn set_is_2d(&self,value:bool){if !matches!(self.kind,Kind::Skew|Kind::SkewX|Kind::SkewY|Kind::Perspective){self.is_2d.set(value);}}
    #[method(name="toMatrix")]fn to_matrix(ctx:&mut Ctx,this:This<Value>)->OpResult<Value>{let matrix=matrix(ctx,&this.0)?;Ok(crate::geometry::matrix_native_instance(ctx,matrix))}
    #[method(name="toString")]fn to_string(ctx:&mut Ctx,this:This<Value>)->OpResult<String>{component_text(ctx,&this.0)}
    #[proto(str)]fn string_coercion(ctx:&mut Ctx,this:This<Value>)->OpResult<String>{component_text(ctx,&this.0)}
}
fn translate(ctx:&mut Ctx,x:Value,y:Value,z:Option<Value>)->OpResult<Value>{
    numeric(ctx,&x,Dimension::LengthPercentage)?;numeric(ctx,&y,Dimension::LengthPercentage)?;
    let is_2d=z.is_none();let z=match z{Some(z)=>{numeric(ctx,&z,Dimension::Length)?;z},None=>unit_value(ctx,0.0,NumericUnit::Px)};
    component(ctx,Kind::Translate,is_2d,vec![("x",x),("y",y),("z",z)])
}
fn scale(ctx:&mut Ctx,x:Value,y:Value,z:Option<Value>)->OpResult<Value>{
    let x=numberish(ctx,x)?;let y=numberish(ctx,y)?;numeric(ctx,&x,Dimension::Number)?;numeric(ctx,&y,Dimension::Number)?;
    let is_2d=z.is_none();let z=match z{Some(z)=>{let z=numberish(ctx,z)?;numeric(ctx,&z,Dimension::Number)?;z},None=>unit_value(ctx,1.0,NumericUnit::Number)};
    component(ctx,Kind::Scale,is_2d,vec![("x",x),("y",y),("z",z)])
}
fn rotate(ctx:&mut Ctx,angle:Value,axis:Option<[Value;3]>)->OpResult<Value>{
    numeric(ctx,&angle,Dimension::Angle)?;let is_2d=axis.is_none();
    let [x,y,z]=match axis{Some([x,y,z])=>{let x=numberish(ctx,x)?;let y=numberish(ctx,y)?;let z=numberish(ctx,z)?;
        for value in [&x,&y,&z]{numeric(ctx,value,Dimension::Number)?;}[x,y,z]},None=>[unit_value(ctx,0.0,NumericUnit::Number),unit_value(ctx,0.0,NumericUnit::Number),unit_value(ctx,1.0,NumericUnit::Number)]};
    component(ctx,Kind::Rotate,is_2d,vec![("x",x),("y",y),("z",z),("angle",angle)])
}

#[lumen_bind::methods]
impl DomCssTranslate {
#[constructor] fn new(ctx:&mut Ctx,x:Value,y:Value,z:Option<Value>)->OpResult<TransformConstructor>{Ok(TransformConstructor(translate(ctx,x,y,z)?))}
#[getter] fn x(ctx:&mut Ctx,this:This<Value>)->OpResult<Value>{slot(ctx,&this.0,"x")}
#[setter] fn set_x(ctx:&mut Ctx,this:This<Value>,value:Value)->OpResult<()>{set_slot(ctx,&this.0,"x",value,Some(Dimension::LengthPercentage))}
#[getter] fn y(ctx:&mut Ctx,this:This<Value>)->OpResult<Value>{slot(ctx,&this.0,"y")}
#[setter] fn set_y(ctx:&mut Ctx,this:This<Value>,value:Value)->OpResult<()>{set_slot(ctx,&this.0,"y",value,Some(Dimension::LengthPercentage))}
#[getter] fn z(ctx:&mut Ctx,this:This<Value>)->OpResult<Value>{slot(ctx,&this.0,"z")}
#[setter] fn set_z(ctx:&mut Ctx,this:This<Value>,value:Value)->OpResult<()>{set_slot(ctx,&this.0,"z",value,Some(Dimension::Length))}
}

#[lumen_bind::methods]
impl DomCssScale {
#[constructor] fn new(ctx:&mut Ctx,x:Value,y:Value,z:Option<Value>)->OpResult<TransformConstructor>{Ok(TransformConstructor(scale(ctx,x,y,z)?))}
#[getter] fn x(ctx:&mut Ctx,this:This<Value>)->OpResult<Value>{slot(ctx,&this.0,"x")}
#[setter] fn set_x(ctx:&mut Ctx,this:This<Value>,value:Value)->OpResult<()>{set_slot(ctx,&this.0,"x",value,Some(Dimension::Number))}
#[getter] fn y(ctx:&mut Ctx,this:This<Value>)->OpResult<Value>{slot(ctx,&this.0,"y")}
#[setter] fn set_y(ctx:&mut Ctx,this:This<Value>,value:Value)->OpResult<()>{set_slot(ctx,&this.0,"y",value,Some(Dimension::Number))}
#[getter] fn z(ctx:&mut Ctx,this:This<Value>)->OpResult<Value>{slot(ctx,&this.0,"z")}
#[setter] fn set_z(ctx:&mut Ctx,this:This<Value>,value:Value)->OpResult<()>{set_slot(ctx,&this.0,"z",value,Some(Dimension::Number))}
}

#[lumen_bind::methods]
impl DomCssRotate {
#[constructor] fn new(ctx:&mut Ctx,x:Value,y:Passed<Value>,z:Passed<Value>,angle:Passed<Value>)->OpResult<TransformConstructor>{
    let value=match (y.0,z.0,angle.0){(None,None,None)=>rotate(ctx,x,None)?,(Some(y),Some(z),Some(angle))=>rotate(ctx,angle,Some([x,y,z]))?,_=>return Err(OpError::type_error("CSSRotate requires one or four arguments"))};
    Ok(TransformConstructor(value))
}

#[getter] fn x(ctx:&mut Ctx,this:This<Value>)->OpResult<Value>{slot(ctx,&this.0,"x")}
#[setter] fn set_x(ctx:&mut Ctx,this:This<Value>,value:Value)->OpResult<()>{set_slot(ctx,&this.0,"x",value,Some(Dimension::Number))}
#[getter] fn y(ctx:&mut Ctx,this:This<Value>)->OpResult<Value>{slot(ctx,&this.0,"y")}
#[setter] fn set_y(ctx:&mut Ctx,this:This<Value>,value:Value)->OpResult<()>{set_slot(ctx,&this.0,"y",value,Some(Dimension::Number))}
#[getter] fn z(ctx:&mut Ctx,this:This<Value>)->OpResult<Value>{slot(ctx,&this.0,"z")}
#[setter] fn set_z(ctx:&mut Ctx,this:This<Value>,value:Value)->OpResult<()>{set_slot(ctx,&this.0,"z",value,Some(Dimension::Number))}
#[getter] fn angle(ctx:&mut Ctx,this:This<Value>)->OpResult<Value>{slot(ctx,&this.0,"angle")}
#[setter] fn set_angle(ctx:&mut Ctx,this:This<Value>,value:Value)->OpResult<()>{set_slot(ctx,&this.0,"angle",value,Some(Dimension::Angle))}
}

#[lumen_bind::methods]
impl DomCssSkew {
#[constructor] fn new(ctx:&mut Ctx,ax:Value,ay:Value)->OpResult<TransformConstructor>{numeric(ctx,&ax,Dimension::Angle)?;numeric(ctx,&ay,Dimension::Angle)?;Ok(TransformConstructor(component(ctx,Kind::Skew,true,vec![("ax",ax),("ay",ay)])?))}
#[getter] fn ax(ctx:&mut Ctx,this:This<Value>)->OpResult<Value>{slot(ctx,&this.0,"ax")}
#[setter] fn set_ax(ctx:&mut Ctx,this:This<Value>,value:Value)->OpResult<()>{set_slot(ctx,&this.0,"ax",value,Some(Dimension::Angle))}
#[getter] fn ay(ctx:&mut Ctx,this:This<Value>)->OpResult<Value>{slot(ctx,&this.0,"ay")}
#[setter] fn set_ay(ctx:&mut Ctx,this:This<Value>,value:Value)->OpResult<()>{set_slot(ctx,&this.0,"ay",value,Some(Dimension::Angle))}
}

#[lumen_bind::methods]
impl DomCssSkewX {
#[constructor] fn new(ctx:&mut Ctx,ax:Value)->OpResult<TransformConstructor>{numeric(ctx,&ax,Dimension::Angle)?;Ok(TransformConstructor(component(ctx,Kind::SkewX,true,vec![("ax",ax)])?))}
#[getter] fn ax(ctx:&mut Ctx,this:This<Value>)->OpResult<Value>{slot(ctx,&this.0,"ax")}
#[setter] fn set_ax(ctx:&mut Ctx,this:This<Value>,value:Value)->OpResult<()>{set_slot(ctx,&this.0,"ax",value,Some(Dimension::Angle))}
}

#[lumen_bind::methods]
impl DomCssSkewY {
#[constructor] fn new(ctx:&mut Ctx,ay:Value)->OpResult<TransformConstructor>{numeric(ctx,&ay,Dimension::Angle)?;Ok(TransformConstructor(component(ctx,Kind::SkewY,true,vec![("ay",ay)])?))}
#[getter] fn ay(ctx:&mut Ctx,this:This<Value>)->OpResult<Value>{slot(ctx,&this.0,"ay")}
#[setter] fn set_ay(ctx:&mut Ctx,this:This<Value>,value:Value)->OpResult<()>{set_slot(ctx,&this.0,"ay",value,Some(Dimension::Angle))}
}

#[lumen_bind::methods]
impl DomCssPerspective {
#[constructor] fn new(ctx:&mut Ctx,length:Value)->OpResult<TransformConstructor>{let length=perspective(ctx,length)?;Ok(TransformConstructor(component(ctx,Kind::Perspective,false,vec![("length",length)])?))}
#[getter] fn length(ctx:&mut Ctx,this:This<Value>)->OpResult<Value>{slot(ctx,&this.0,"length")}
#[setter] fn set_length(ctx:&mut Ctx,this:This<Value>,value:Value)->OpResult<()>{set_slot(ctx,&this.0,"length",value,None)}
}

#[lumen_bind::methods]
impl DomCssMatrixComponent {
#[constructor] fn new(ctx:&mut Ctx,matrix:Value,options:Option<Value>)->OpResult<TransformConstructor>{
    let data=crate::geometry::matrix_native_value(ctx,&matrix)?;
    let flag=if let Some(options)=options{if !matches!(options,Value::Obj(_)|Value::Null){return Err(OpError::type_error("expected CSSMatrixComponent options"));}
        if matches!(options,Value::Null){Value::Undefined}else{ctx.member_get(&options,"is2D").map_err(OpError::thrown)?}}else{Value::Undefined};
    let is_2d=if matches!(flag,Value::Undefined){data.is_2d}else{ctx.to_boolean(&flag)};
    Ok(TransformConstructor(component(ctx,Kind::Matrix,is_2d,vec![("matrix",matrix)])?))
}

#[getter] fn matrix(ctx:&mut Ctx,this:This<Value>)->OpResult<Value>{slot(ctx,&this.0,"matrix")}
#[setter] fn set_matrix(ctx:&mut Ctx,this:This<Value>,value:Value)->OpResult<()>{set_slot(ctx,&this.0,"matrix",value,None)}
}
fn list_data(ctx:&mut Ctx,this:&Value)->OpResult<Value>{
    let key=ctx.with_instance::<DomCssTransformValue,_>(this,|value|value.data_key.clone())?;
    ctx.reflect_get(this,&key,this).map_err(OpError::thrown)
}
fn list_item(ctx:&mut Ctx,this:&Value,index:usize)->OpResult<Value>{let data=list_data(ctx,this)?;ctx.member_get(&data,&index.to_string()).map_err(OpError::thrown)}
fn list(ctx:&mut Ctx,values:Vec<Value>)->OpResult<Value>{
    if values.is_empty()||values.len()>MAX_UNPARSED_COMPONENTS{return Err(OpError::type_error("transform list must be nonempty and bounded"));}
    let key=ctx.new_symbol(Some("CSSTransformValue.values".into()));
    let value=ctx.new_instance(DomCssTransformValue{base:DomCssStyleValue{serialized_value:RefCell::new(String::new())},data_key:key.clone(),length:Cell::new(values.len())});
    let data=ctx.new_object_with_proto(&Value::Null);
    for (index,component) in values.into_iter().enumerate(){ctx.create_data_property(&data,&index.to_string(),component).map_err(OpError::thrown)?;}
    define_data_property(ctx,&value,key,data,false,false,false).map_err(OpError::thrown)?;Ok(value)
}
pub(super) fn is_value(ctx:&mut Ctx,value:&Value)->bool{ctx.with_instance::<DomCssTransformValue,_>(value,|_|()).is_ok()}
pub(super) fn serialize(ctx:&mut Ctx,this:&Value)->OpResult<String>{
    let length=ctx.with_instance::<DomCssTransformValue,_>(this,|value|value.length.get())?;
    let mut text=String::new();
    for index in 0..length{
        let value=list_item(ctx,this,index)?;let component=component_text(ctx,&value)?;
        let len=text.len().checked_add(component.len()).and_then(|n|n.checked_add(usize::from(index!=0))).filter(|n|*n<=MAX_UNPARSED_BYTES)
            .ok_or_else(||OpError::range_error("transform serialization exceeds byte limit"))?;
        text.try_reserve(len-text.len()).map_err(|_|OpError::range_error("transform serialization allocation failed"))?;
        if index!=0{text.push(' ');}text.push_str(&component);
    }Ok(text)
}
fn iterator(ctx:&mut Ctx,owner:Value,kind:NumericArrayIteratorKind)->OpResult<Value>{
    let key=ctx.new_symbol(Some("CSSTransformValueIterator.owner".into()));
    let value=ctx.new_instance(TransformIterator{owner_key:key.clone(),kind,index:Cell::new(0)});
    define_data_property(ctx,&value,key,owner,false,false,false).map_err(OpError::thrown)?;Ok(value)
}
#[lumen_bind::methods]
impl DomCssTransformValue {
    #[constructor]fn new(ctx:&mut Ctx,transforms:Value)->OpResult<TransformConstructor>{
        let values=ctx.convert_iterable(&transforms,MAX_UNPARSED_COMPONENTS,|ctx,value|{ctx.with_instance::<DomCssTransformComponent,_>(&value,|_|())?;Ok(value)})?;
        Ok(TransformConstructor(list(ctx,values)?))
    }
    #[getter]fn length(&self)->usize{self.length.get()}
    #[proto(len)]fn indexed_length(&self)->usize{self.length.get()}
    #[proto(getitem)]fn item(ctx:&mut Ctx,this:This<Value>,index:usize)->OpResult<Value>{
        let length=ctx.with_instance::<Self,_>(&this.0,|value|value.length.get())?;
        if index>=length{Ok(Value::Undefined)}else{list_item(ctx,&this.0,index)}
    }
    #[proto(setitem)]fn set_item(ctx:&mut Ctx,this:This<Value>,index:usize,value:Value)->OpResult<()> {
        let length=ctx.with_instance::<Self,_>(&this.0,|value|value.length.get())?;
        if index>length||index>=MAX_UNPARSED_COMPONENTS{return Err(OpError::range_error("transform index must replace or append one component"));}
        ctx.with_instance::<DomCssTransformComponent,_>(&value,|_|())?;
        let data=list_data(ctx,&this.0)?;ctx.create_data_property(&data,&index.to_string(),value).map_err(OpError::thrown)?;
        if index==length{ctx.with_instance_mut::<Self,_>(&this.0,|value|value.length.set(length+1))?;}Ok(())
    }
    #[getter(name="is2D")]fn is_2d(ctx:&mut Ctx,this:This<Value>)->OpResult<bool>{
        let length=ctx.with_instance::<Self,_>(&this.0,|value|value.length.get())?;
        for index in 0..length{let value=list_item(ctx,&this.0,index)?;if !ctx.with_instance::<DomCssTransformComponent,_>(&value,|value|value.is_2d.get())?{return Ok(false);}}Ok(true)
    }
    #[method(name="toMatrix")]fn to_matrix(ctx:&mut Ctx,this:This<Value>)->OpResult<Value>{
        let length=ctx.with_instance::<Self,_>(&this.0,|value|value.length.get())?;let mut product=Matrix::default();
        for index in 0..length{let value=list_item(ctx,&this.0,index)?;product=product.multiply(matrix(ctx,&value)?);}Ok(crate::geometry::matrix_native_instance(ctx,product))
    }
    #[method(name="toString")]fn to_string(ctx:&mut Ctx,this:This<Value>)->OpResult<String>{serialize(ctx,&this.0)}
    #[proto(str)]fn string_coercion(ctx:&mut Ctx,this:This<Value>)->OpResult<String>{serialize(ctx,&this.0)}
    #[proto(iter)]fn iter(ctx:&mut Ctx,this:This<Value>)->OpResult<Value>{iterator(ctx,this.0,NumericArrayIteratorKind::Values)}
    fn values(ctx:&mut Ctx,this:This<Value>)->OpResult<Value>{iterator(ctx,this.0,NumericArrayIteratorKind::Values)}
    fn keys(ctx:&mut Ctx,this:This<Value>)->OpResult<Value>{iterator(ctx,this.0,NumericArrayIteratorKind::Keys)}
    fn entries(ctx:&mut Ctx,this:This<Value>)->OpResult<Value>{iterator(ctx,this.0,NumericArrayIteratorKind::Entries)}
    fn for_each(ctx:&mut Ctx,this:This<Value>,callback:JsFunction,this_arg:Option<Value>)->OpResult<()> {
        let mut index=0;
        while index<ctx.with_instance::<Self,_>(&this.0,|value|value.length.get())? {
            let value=list_item(ctx,&this.0,index)?;callback.call(ctx,this_arg.clone().unwrap_or(Value::Undefined),&[value,Value::Num(index as f64),this.0.clone()])?;index+=1;
        }Ok(())
    }
}
#[lumen_bind::methods]
impl TransformIterator {
    #[proto(iter)]fn iter(this:This<Value>)->Value{this.0}
    #[proto(next)]fn next(&self,ctx:&mut Ctx,this:This<Value>)->OpResult<Option<Value>>{
        let owner=ctx.reflect_get(&this.0,&self.owner_key,&this.0).map_err(OpError::thrown)?;
        let length=ctx.with_instance::<DomCssTransformValue,_>(&owner,|value|value.length.get())?;
        let index=self.index.get();if index>=length{return Ok(None);}self.index.set(index+1);
        let value=match self.kind{NumericArrayIteratorKind::Keys=>Value::Num(index as f64),
            NumericArrayIteratorKind::Values=>list_item(ctx,&owner,index)?,NumericArrayIteratorKind::Entries=>{let value=list_item(ctx,&owner,index)?;JsHost::from_list(ctx,vec![Value::Num(index as f64),value])}};
        Ok(Some(value))
    }
}

pub(super) fn install(ctx:&mut Ctx,global:&Value)->OpResult<()> {
    macro_rules! interface {($name:literal,$class:ty)=>{let ctor=ctx.class_constructor::<$class>();crate::install_interface(ctx,global,$name,ctor)
        .map_err(|_|OpError::new("Error",concat!($name," installation failed")))?;};}
    interface!("CSSTransformComponent",DomCssTransformComponent);interface!("CSSTranslate",DomCssTranslate);
    interface!("CSSRotate",DomCssRotate);interface!("CSSScale",DomCssScale);interface!("CSSSkew",DomCssSkew);
    interface!("CSSSkewX",DomCssSkewX);interface!("CSSSkewY",DomCssSkewY);interface!("CSSPerspective",DomCssPerspective);
    interface!("CSSMatrixComponent",DomCssMatrixComponent);interface!("CSSTransformValue",DomCssTransformValue);
    ctx.class_constructor::<TransformIterator>();Ok(())
}
fn argument(ctx:&mut Ctx,text:&str,dimension:Dimension)->OpResult<Value>{
    let mut expression=css::typed_numeric::parse_numeric_expression(text.trim()).ok_or_else(||OpError::type_error("invalid transform argument"))?;
    if let NumericExpression::Value(value)=&mut expression {
        if value.unit==NumericUnit::Number&&value.value==0.0 {
            value.unit=match dimension{Dimension::Angle=>NumericUnit::Deg,Dimension::Length|Dimension::LengthPercentage=>NumericUnit::Px,_=>NumericUnit::Number};
        }
    }
    if expression.contains_sign(){return Err(OpError::type_error("transform argument cannot be reified as CSSNumericValue"));}
    expression.simplify_absolute_units();let value=reify_numeric_expression(ctx,&expression)?;numeric(ctx,&value,dimension)?;Ok(value)
}
/// Reify the canonical function-token sequence without converting specified
/// percentages or font-relative lengths into layout-dependent matrices.
pub(super) fn parse(ctx:&mut Ctx,text:&str)->OpResult<Value>{
    let functions=css::typed_transforms::validated_function_tokens(text).ok_or_else(||OpError::type_error("invalid transform function list"))?;
    if functions.iter().any(|function|function.name=="transform-interpolate") {
        // Typed OM Level1 has no specialized component for this newer function.
        // Its direct CSSStyleValue representation retains the immutable source;
        // used matrices are resolved by the shared CSS consumer with a real box.
        return Ok(ctx.new_instance(DomCssStyleValue{serialized_value:RefCell::new(text.trim().to_owned())}));
    }
    let mut values=Vec::new();values.try_reserve_exact(functions.len()).map_err(|_|OpError::range_error("transform component allocation failed"))?;
    for function in functions {
        let args=function.arguments;let name=function.name.as_str();
        let value=match name {
            "translate" if (1..=2).contains(&args.len())=>{let x=argument(ctx,args[0],Dimension::LengthPercentage)?;let y=if args.len()==2{argument(ctx,args[1],Dimension::LengthPercentage)?}else{unit_value(ctx,0.0,NumericUnit::Px)};translate(ctx,x,y,None)?},
            "translatex"|"translatey" if args.len()==1=>{let v=argument(ctx,args[0],Dimension::LengthPercentage)?;let zero=unit_value(ctx,0.0,NumericUnit::Px);if name=="translatex"{translate(ctx,v,zero,None)?}else{translate(ctx,zero,v,None)?}},
            "translatez" if args.len()==1=>{let z=argument(ctx,args[0],Dimension::Length)?;let x=unit_value(ctx,0.0,NumericUnit::Px);let y=unit_value(ctx,0.0,NumericUnit::Px);translate(ctx,x,y,Some(z))?},
            "translate3d" if args.len()==3=>{let x=argument(ctx,args[0],Dimension::LengthPercentage)?;let y=argument(ctx,args[1],Dimension::LengthPercentage)?;let z=argument(ctx,args[2],Dimension::Length)?;translate(ctx,x,y,Some(z))?},
            "scale" if (1..=2).contains(&args.len())=>{let x=argument(ctx,args[0],Dimension::Number)?;let y=argument(ctx,if args.len()==2{args[1]}else{args[0]},Dimension::Number)?;scale(ctx,x,y,None)?},
            "scalex"|"scaley"|"scalez" if args.len()==1=>{let v=argument(ctx,args[0],Dimension::Number)?;let one=unit_value(ctx,1.0,NumericUnit::Number);let another=unit_value(ctx,1.0,NumericUnit::Number);match name{"scalex"=>scale(ctx,v,one,None)?,"scaley"=>scale(ctx,one,v,None)?,_=>scale(ctx,one,another,Some(v))?}},
            "scale3d" if args.len()==3=>{let x=argument(ctx,args[0],Dimension::Number)?;let y=argument(ctx,args[1],Dimension::Number)?;let z=argument(ctx,args[2],Dimension::Number)?;scale(ctx,x,y,Some(z))?},
            "rotate" if args.len()==1=>{let a=argument(ctx,args[0],Dimension::Angle)?;rotate(ctx,a,None)?},
            "rotatex"|"rotatey"|"rotatez" if args.len()==1=>{let a=argument(ctx,args[0],Dimension::Angle)?;let axis=[unit_value(ctx,if name=="rotatex"{1.0}else{0.0},NumericUnit::Number),unit_value(ctx,if name=="rotatey"{1.0}else{0.0},NumericUnit::Number),unit_value(ctx,if name=="rotatez"{1.0}else{0.0},NumericUnit::Number)];rotate(ctx,a,Some(axis))?},
            "rotate3d" if args.len()==4=>{let x=argument(ctx,args[0],Dimension::Number)?;let y=argument(ctx,args[1],Dimension::Number)?;let z=argument(ctx,args[2],Dimension::Number)?;let a=argument(ctx,args[3],Dimension::Angle)?;rotate(ctx,a,Some([x,y,z]))?},
            "skew" if (1..=2).contains(&args.len())=>{let x=argument(ctx,args[0],Dimension::Angle)?;let y=if args.len()==2{argument(ctx,args[1],Dimension::Angle)?}else{unit_value(ctx,0.0,NumericUnit::Deg)};component(ctx,Kind::Skew,true,vec![("ax",x),("ay",y)])?},
            "skewx"|"skewy" if args.len()==1=>{let a=argument(ctx,args[0],Dimension::Angle)?;let(kind,name)=if name=="skewx"{(Kind::SkewX,"ax")}else{(Kind::SkewY,"ay")};component(ctx,kind,true,vec![(name,a)])?},
            "perspective" if args.len()==1=>{let v=if css_identifier_value(args[0].trim()).is_some_and(|name|name.eq_ignore_ascii_case("none")){ctx.new_instance(keyword_value("none".into()))}else{argument(ctx,args[0],Dimension::Length)?};component(ctx,Kind::Perspective,false,vec![("length",v)])?},
            "matrix"|"matrix3d" if args.len()==if name=="matrix"{6}else{16}=>{
                let mut numbers=Vec::new();for raw in args{let value=argument(ctx,raw,Dimension::Number)?;numbers.push(scalar(ctx,&value,NumericUnit::Number)?);}
                let matrix=if name=="matrix"{Matrix::from_affine(numbers.try_into().map_err(|_|OpError::type_error("matrix needs six numbers"))?)}else{Matrix{values:numbers.try_into().map_err(|_|OpError::type_error("matrix3d needs sixteen numbers"))?,is_2d:false}};
                let value=crate::geometry::matrix_native_instance(ctx,matrix);component(ctx,Kind::Matrix,name=="matrix",vec![("matrix",value)])?
            },
            _=>return Err(OpError::type_error("invalid transform function or argument count")),
        };values.push(value);
    }list(ctx,values)
}

#[cfg(test)]
mod tests {
    fn evaluate(engine: &mut lumen::Engine, source: &str) {
        let result=engine.eval_value(source).unwrap();
        if let Err(error)=result {
            let message=engine.ctx().coerce_string(&error).map(|value|value.to_string()).unwrap_or_else(|_|"unprintable transform exception".into());
            panic!("Typed OM transform: {message}");
        }
    }
    #[test]
    fn specification_typed_om_deferred_transform_source_remains_unresolved() {
        let mut engine=lumen::Engine::new();let _realm=crate::install(engine.ctx(),"<div id=subject></div>",256).unwrap();
        evaluate(&mut engine,r#"(() => {
            const source='transform-interpolate(0.5, 0: translateX(50%), 1: scale(4))';
            const value=CSSStyleValue.parse('transform',source);
            if(!(value instanceof CSSStyleValue)||value instanceof CSSTransformValue||value.toString()!==source||value.toMatrix!==undefined)throw Error('deferred source was coerced to a guessed matrix');
            CSS.registerProperty({name:'--mapped',syntax:'<transform-function>',inherits:false,initialValue:source});
            const subject=document.getElementById('subject');
            const custom=subject.computedStyleMap().get('--mapped');
            if(!(custom instanceof CSSStyleValue)||custom.toString()!==source)throw Error('registered single-function fallback indexed a non-list value');
            subject.attributeStyleMap.set('transform',value);
            if(subject.style.transform!==source)throw Error('source did not survive setting its transform property');
        })()"#);
    }

    #[test]
    fn specification_typed_transforms_live_components_validation_and_shared_matrices() {
        let mut engine=lumen::Engine::new();let _realm=crate::install(engine.ctx(),"<div id=subject></div>",256).unwrap();
        evaluate(&mut engine,r#"(() => {
            const check=(ok,msg)=>{if(!ok)throw Error(msg)};
            const throws=(name,f)=>{let error;try{f()}catch(e){error=e}check(error&&error.name===name,'expected '+name)};
            const x=CSS.px(10),translation=new CSSTranslate(x,CSS.px(4));
            const scaling=new CSSScale(2,3),list=new CSSTransformValue([translation,scaling]);
            check(list instanceof CSSStyleValue && translation instanceof CSSTransformComponent,'native inheritance');
            check(list[0]===translation && translation.x===x && list.length===2 && list.is2D,'actual component references');
            let matrix=list.toMatrix();check(matrix instanceof DOMMatrix && matrix.a===2 && matrix.d===3 && matrix.e===10 && matrix.f===4,'postmultiply translation then scale');
            x.value=12;check(list.toMatrix().e===12 && list.toString().includes('12px'),'numeric mutation is live');
            list[0]=scaling;list[1]=translation;check(list.toMatrix().e===24 && list.toMatrix().f===12,'noncommuting list order');
            const rotation=new CSSRotate(CSS.deg(90));list[2]=rotation;
            check(list.length===3 && [...list][2]===rotation && [...list.keys()].join(',')==='0,1,2','append and iteration');
            check([...list.entries()][1][1]===translation,'entry identity');
            throws('RangeError',()=>{list[4]=translation});throws('TypeError',()=>new CSSTransformValue([]));
            throws('TypeError',()=>new CSSTranslate(CSS.deg(1),CSS.px(0)));
            throws('TypeError',()=>new CSSScale(CSS.px(1),2));
            throws('TypeError',()=>new CSSRotate(CSS.px(1)));
            throws('TypeError',()=>new CSSPerspective('auto'));
            throws('TypeError',()=>new CSSTranslate(CSS.percent(10),CSS.px(0)).toMatrix());
            throws('TypeError',()=>new CSSTranslate(CSS.em(1),CSS.px(0)).toMatrix());
            const three=new CSSTranslate(CSS.px(1),CSS.px(2),CSS.px(3));
            check(!three.is2D && !three.toMatrix().is2D && three.toMatrix().m43===3,'real three-dimensional matrix');
            three.is2D=true;check(three.toMatrix().m43===0 && three.z.value===3,'two-dimensional switch preserves stored z');
            const skew=new CSSSkew(CSS.deg(45),CSS.deg(45));skew.is2D=false;
            check(skew.is2D && Math.abs(skew.toMatrix().a-1)<1e-9 && Math.abs(skew.toMatrix().b-1)<1e-9,'simultaneous skew matrix and immutable dimension');
            const perspective=new CSSPerspective(CSS.px(10));perspective.is2D=true;
            check(!perspective.is2D && perspective.toMatrix().m34===-.1,'perspective matrix');
            check(new CSSPerspective(CSS.px(-1)).toString()==='perspective(calc(-1px))' && new CSSPerspective(CSS.px(-1)).toMatrix().m34===-1,'specified range wrapper and used perspective clamp');
            check(new CSSSkew(CSS.deg(1),CSS.turn(0)).toString()==='skew(1deg)' && new CSSScale(2,2).toString()==='scale(2)','canonical omitted equal and zero arguments');
            const owner=new DOMMatrix([1,0,0,1,5,6]),component=new CSSMatrixComponent(owner);
            check(component.matrix===owner,'matrix constructor preserves actual identity');owner.e=9;check(component.toMatrix().e===9,'matrix mutation is live');
            throws('TypeError',()=>{component.matrix=new DOMMatrixReadOnly()});check(component.matrix===owner,'matrix setter brand failure is transactional');
            globalThis.retainedTransform=list;
            return true;
        })()"#);
        engine.collect_garbage();
        evaluate(&mut engine,"if(retainedTransform[1].x.value!==12 || retainedTransform.toMatrix().e!==24)throw Error('live component graph reclaimed');");
    }
    #[test]
    fn specification_typed_transforms_reification_property_map_and_relative_source_identity() {
        let mut engine=lumen::Engine::new();let _realm=crate::install(engine.ctx(),"<div id=subject></div>",256).unwrap();
        evaluate(&mut engine,r#"(() => {
            const check=(ok,msg)=>{if(!ok)throw Error(msg)};
            const parse=text=>CSSStyleValue.parse('transform',text);
            const cases=[['translateX(1px)',CSSTranslate,true],['translateZ(1px)',CSSTranslate,false],['rotate(1deg)',CSSRotate,true],['rotateZ(1deg)',CSSRotate,false],['scaleY(2)',CSSScale,true],['scale3d(1,2,3)',CSSScale,false],['skew(1deg)',CSSSkew,true],['skewX(1deg)',CSSSkewX,true],['skewY(1deg)',CSSSkewY,true],['perspective(none)',CSSPerspective,false],['matrix(1,0,0,1,2,3)',CSSMatrixComponent,true],['matrix3d(1,0,0,0,0,1,0,0,0,0,1,0,2,3,4,1)',CSSMatrixComponent,false]];
            for(const [text,brand,two]of cases){const value=parse(text);check(value instanceof CSSTransformValue && value.length===1 && value[0] instanceof brand && value.is2D===two,'reify '+text)}
            const relative=parse('translate(calc(1px + 1em), 10%) rotate(1turn)');
            check(relative[0].x instanceof CSSMathSum && relative[0].y.unit==='percent' && relative[1].angle.unit==='turn','canonical typed math without used-length guessing');
            let failed=false;try{relative.toMatrix()}catch(e){failed=e.name==='TypeError'}check(failed,'relative matrix requires a layout-independent basis');
            const target=document.getElementById('subject'),map=target.attributeStyleMap;
            const component=new CSSTranslate(CSS.px(7),CSS.px(9)),value=new CSSTransformValue([component]);
            component.x.value=8;map.set('transform',value);
            check(map.get('transform') instanceof CSSTransformValue && map.get('transform')[0].x.value===8,'actual inline property-map serialization and reification');
            component.x.value=11;map.set('transform',value);check(map.get('transform')[0].x.value===11,'later component mutation is serialized');
            failed=false;try{map.set('width',value)}catch(e){failed=e.name==='TypeError'}check(failed,'transform cannot be assigned to another property');
            for(const text of ['translate(1deg)','scale(2px)','rotate(1px)','matrix(1,2)','translate(1px,)','unknown(1px)']){failed=false;try{parse(text)}catch(e){failed=e.name==='TypeError'}check(failed,'invalid transform '+text)}
            check(CSSStyleValue.parse('transform','none') instanceof CSSKeywordValue,'none remains a keyword');
            return true;
        })()"#);
    }
}
