//! Typed homogeneous geometry values. Numerical operations live in lumen-common.
use super::{DomRect, DomRectReadOnly};
use lumen::embed::{CloneBrand, Ctx, JsHost, OpError, OpResult, TaKind, Value};
use lumen_bind::This;
use lumen_common::dom_geometry::{self as core, Matrix, Point};
use lumen_host::realm_services::RealmServices;
use std::cell::Cell;

struct GeometryContext { window: bool }
fn dictionary(value:&Value)->OpResult<()> {
    if matches!(value,Value::Obj(_)|Value::Undefined|Value::Null) {Ok(())}else{Err(OpError::type_error("Expected a geometry dictionary"))}
}
fn optional_number(ctx:&mut Ctx,value:&Value,name:&str)->OpResult<Option<f64>> {
    if matches!(value,Value::Undefined|Value::Null) {return Ok(None)}
    let value=ctx.member_get(value,name).map_err(OpError::thrown)?;
    if matches!(value,Value::Undefined) {Ok(None)}else{ctx.coerce_number(&value).map(Some).map_err(OpError::thrown)}
}
fn point_dictionary(ctx:&mut Ctx,value:&Value)->OpResult<Point> {
    dictionary(value)?;
    // WebIDL dictionaries convert their members in alphabetical order.
    let w=optional_number(ctx,value,"w")?.unwrap_or(1.0);
    let x=optional_number(ctx,value,"x")?.unwrap_or(0.0);
    let y=optional_number(ctx,value,"y")?.unwrap_or(0.0);
    let z=optional_number(ctx,value,"z")?.unwrap_or(0.0);
    Ok(Point([x,y,z,w]))
}
fn matrix_dictionary(ctx:&mut Ctx,value:&Value)->OpResult<Matrix> {
    dictionary(value)?;
    let mut aliases=[None;6];
    for (index,name) in ["a","b","c","d","e","f"].into_iter().enumerate() {aliases[index]=optional_number(ctx,value,name)?;}
    let mut elements=[None;16];
    for index in [0,1,4,5,12,13] {elements[index]=optional_number(ctx,value,&format!("m{}{}",index/4+1,index%4+1))?;}
    let flag=if matches!(value,Value::Undefined|Value::Null) {Value::Undefined}else{ctx.member_get(value,"is2D").map_err(OpError::thrown)?};
    for index in [2,3,6,7,8,9,10,11,14,15] {elements[index]=optional_number(ctx,value,&format!("m{}{}",index/4+1,index%4+1))?;}
    for (alias,index) in [0,1,4,5,12,13].into_iter().enumerate() {
        if let (Some(a),Some(b))=(aliases[alias],elements[index]) {if a!=b && !(a.is_nan()&&b.is_nan()) {return Err(OpError::type_error("Conflicting matrix alias members"))}}
        if elements[index].is_none() {elements[index]=aliases[alias];}
    }
    let mut matrix=Matrix::default();
    for (index,value) in elements.into_iter().enumerate() {if let Some(value)=value {matrix.values[index]=value;}}
    let is_2d=matrix.has_2d_components();
    matrix.is_2d=if matches!(flag,Value::Undefined) {is_2d}else{ctx.to_boolean(&flag)};
    if matrix.is_2d&&!is_2d {return Err(OpError::type_error("A 2D matrix cannot have 3D components"))}
    Ok(matrix)
}
fn matrix_sequence(ctx:&mut Ctx,value:&Value)->OpResult<Matrix> {
    let values=ctx.convert_iterable(value,4096,|ctx,value|ctx.coerce_number(&value).map_err(OpError::thrown))?;
    match values.len() {
        6=>Ok(Matrix::from_affine(values.try_into().expect("verified sequence length"))),
        16=>Ok(Matrix{values:values.try_into().expect("verified sequence length"),is_2d:false}),
        _=>Err(OpError::type_error("A matrix sequence must contain six or sixteen elements")),
    }
}
fn matrix_constructor(ctx:&mut Ctx,value:&Value)->OpResult<Matrix> {
    if matches!(value,Value::Undefined) {return Ok(Matrix::default())}
    if !matches!(value,Value::Obj(_)) {
        if !RealmServices::<GeometryContext>::current(ctx).is_some_and(|context|context.window) {return Err(OpError::type_error("CSS matrix strings are only available in Window"))}
        let source=ctx.coerce_string(value).map_err(OpError::thrown)?;
        if source.trim().is_empty()||source.trim().eq_ignore_ascii_case("none") {return Ok(Matrix::default())}
        return Err(OpError::new("NotSupportedError","CSS string matrix parsing is not implemented; numeric sequences are supported"));
    }
    matrix_sequence(ctx,value)
}
fn matrix_from_array(ctx:&mut Ctx,value:&Value,expected:TaKind)->OpResult<Matrix> {
    let length=match ctx.clone_brand(value) {CloneBrand::TypedArray{kind,length:Some(length),..} if kind==expected=>length,_=>return Err(OpError::type_error("Expected an attached floating-point typed array"))};
    if length!=6&&length!=16 {return Err(OpError::type_error("A matrix array must contain six or sixteen elements"))}
    let bytes=ctx.buffer_source_bytes(value).ok_or_else(||OpError::type_error("Matrix array is out of bounds"))?;
    let numbers:Vec<_>=if expected==TaKind::F32 {bytes.chunks_exact(4).map(|part|f32::from_le_bytes(part.try_into().expect("four bytes")) as f64).collect()}else{bytes.chunks_exact(8).map(|part|f64::from_le_bytes(part.try_into().expect("eight bytes"))).collect()};
    if numbers.len()!=length {return Err(OpError::type_error("Matrix array changed while reading"))}
    if length==6 {Ok(Matrix::from_affine(numbers.try_into().expect("six values")))}else{Ok(Matrix{values:numbers.try_into().expect("sixteen values"),is_2d:false})}
}
fn float_array(ctx:&mut Ctx,matrix:Matrix,kind:TaKind)->OpResult<Value> {
    let bytes=if kind==TaKind::F32 {matrix.values.into_iter().flat_map(|number|(number as f32).to_le_bytes()).collect()}else{core::encode_numbers(matrix.values)};
    let buffer=ctx.make_array_buffer_from(bytes);
    ctx.new_typed_array_view(kind,&buffer,0,16).map_err(OpError::thrown)
}
fn object_numbers(ctx:&mut Ctx,numbers:impl IntoIterator<Item=(&'static str,f64)>)->OpResult<Value> {
    let value=Value::Obj(ctx.new_object());
    for (name,number) in numbers {ctx.create_data_property(&value,name,Value::Num(number)).map_err(OpError::thrown)?;}
    Ok(value)
}
fn point_json(ctx:&mut Ctx,point:Point)->OpResult<Value> {object_numbers(ctx,["x","y","z","w"].into_iter().zip(point.0))}
fn matrix_json(ctx:&mut Ctx,matrix:Matrix)->OpResult<Value> {
    let object=Value::Obj(ctx.new_object());
    for index in 0..16 {ctx.create_data_property(&object,&format!("m{}{}",index/4+1,index%4+1),Value::Num(matrix.values[index])).map_err(OpError::thrown)?;}
    for (name,index) in [("a",0),("b",1),("c",4),("d",5),("e",12),("f",13)] {ctx.create_data_property(&object,name,Value::Num(matrix.values[index])).map_err(OpError::thrown)?;}
    ctx.create_data_property(&object,"is2D",Value::Bool(matrix.is_2d)).map_err(OpError::thrown)?;
    ctx.create_data_property(&object,"isIdentity",Value::Bool(matrix.is_identity())).map_err(OpError::thrown)?;
    Ok(object)
}

#[lumen_bind::class(name="DOMPointReadOnly",hint(js(webidl,invalid_this)))]
pub struct DomPointReadOnly { point:Cell<Point> }
#[lumen_bind::methods]
impl DomPointReadOnly {
    #[constructor(coerce)] fn new(#[default(0.0)]x:f64,#[default(0.0)]y:f64,#[default(0.0)]z:f64,#[default(1.0)]w:f64)->Self {Self{point:Cell::new(Point([x,y,z,w]))}}
    fn from_point(ctx:&mut Ctx,#[default(Value::Undefined)]other:Value)->OpResult<Self> {Ok(Self{point:Cell::new(point_dictionary(ctx,&other)?)})}
    #[getter] fn x(&self)->f64 {self.point.get().0[0]}
    #[getter] fn y(&self)->f64 {self.point.get().0[1]}
    #[getter] fn z(&self)->f64 {self.point.get().0[2]}
    #[getter] fn w(&self)->f64 {self.point.get().0[3]}
    fn matrix_transform(&self,ctx:&mut Ctx,#[default(Value::Undefined)]matrix:Value)->OpResult<DomPoint> {Ok(DomPoint::value(matrix_dictionary(ctx,&matrix)?.transform(self.point.get())))}
    #[method(name="toJSON")] fn to_json(&self,ctx:&mut Ctx)->OpResult<Value> {point_json(ctx,self.point.get())}
}
#[lumen_bind::class(name="DOMPoint",extends=DomPointReadOnly,hint(js(webidl,invalid_this)))]
pub struct DomPoint { base:DomPointReadOnly }
impl DomPoint {fn value(point:Point)->Self {Self{base:DomPointReadOnly{point:Cell::new(point)}}}}
#[lumen_bind::methods]
impl DomPoint {
    #[constructor(coerce)] fn new(#[default(0.0)]x:f64,#[default(0.0)]y:f64,#[default(0.0)]z:f64,#[default(1.0)]w:f64)->Self {Self::value(Point([x,y,z,w]))}
    fn from_point(ctx:&mut Ctx,#[default(Value::Undefined)]other:Value)->OpResult<Self> {Ok(Self::value(point_dictionary(ctx,&other)?))}
    #[getter] fn x(&self)->f64 {self.base.point.get().0[0]}
    #[getter] fn y(&self)->f64 {self.base.point.get().0[1]}
    #[getter] fn z(&self)->f64 {self.base.point.get().0[2]}
    #[getter] fn w(&self)->f64 {self.base.point.get().0[3]}
    #[setter(coerce)] fn set_x(&self,value:f64) {let mut point=self.base.point.get();point.0[0]=value;self.base.point.set(point)}
    #[setter(coerce)] fn set_y(&self,value:f64) {let mut point=self.base.point.get();point.0[1]=value;self.base.point.set(point)}
    #[setter(coerce)] fn set_z(&self,value:f64) {let mut point=self.base.point.get();point.0[2]=value;self.base.point.set(point)}
    #[setter(coerce)] fn set_w(&self,value:f64) {let mut point=self.base.point.get();point.0[3]=value;self.base.point.set(point)}
}

#[lumen_bind::class(name="DOMMatrixReadOnly",hint(js(webidl,invalid_this)))]
pub struct DomMatrixReadOnly {matrix:Cell<Matrix>}
impl DomMatrixReadOnly {fn value(matrix:Matrix)->Self {Self{matrix:Cell::new(matrix)}}}
#[lumen_bind::methods]
impl DomMatrixReadOnly {
    #[constructor] fn new(ctx:&mut Ctx,#[default(Value::Undefined)]init:Value)->OpResult<Self> {Ok(Self::value(matrix_constructor(ctx,&init)?))}
    fn from_matrix(ctx:&mut Ctx,#[default(Value::Undefined)]other:Value)->OpResult<Self> {Ok(Self::value(matrix_dictionary(ctx,&other)?))}
    fn from_float32_array(ctx:&mut Ctx,array:Value)->OpResult<Self> {Ok(Self::value(matrix_from_array(ctx,&array,TaKind::F32)?))}
    fn from_float64_array(ctx:&mut Ctx,array:Value)->OpResult<Self> {Ok(Self::value(matrix_from_array(ctx,&array,TaKind::F64)?))}
    #[getter] fn m11(&self) -> f64 { self.matrix.get().values[0] }
    #[getter] fn m12(&self) -> f64 { self.matrix.get().values[1] }
    #[getter] fn m13(&self) -> f64 { self.matrix.get().values[2] }
    #[getter] fn m14(&self) -> f64 { self.matrix.get().values[3] }
    #[getter] fn m21(&self) -> f64 { self.matrix.get().values[4] }
    #[getter] fn m22(&self) -> f64 { self.matrix.get().values[5] }
    #[getter] fn m23(&self) -> f64 { self.matrix.get().values[6] }
    #[getter] fn m24(&self) -> f64 { self.matrix.get().values[7] }
    #[getter] fn m31(&self) -> f64 { self.matrix.get().values[8] }
    #[getter] fn m32(&self) -> f64 { self.matrix.get().values[9] }
    #[getter] fn m33(&self) -> f64 { self.matrix.get().values[10] }
    #[getter] fn m34(&self) -> f64 { self.matrix.get().values[11] }
    #[getter] fn m41(&self) -> f64 { self.matrix.get().values[12] }
    #[getter] fn m42(&self) -> f64 { self.matrix.get().values[13] }
    #[getter] fn m43(&self) -> f64 { self.matrix.get().values[14] }
    #[getter] fn m44(&self) -> f64 { self.matrix.get().values[15] }
    #[getter] fn a(&self) -> f64 { self.matrix.get().values[0] }
    #[getter] fn b(&self) -> f64 { self.matrix.get().values[1] }
    #[getter] fn c(&self) -> f64 { self.matrix.get().values[4] }
    #[getter] fn d(&self) -> f64 { self.matrix.get().values[5] }
    #[getter] fn e(&self) -> f64 { self.matrix.get().values[12] }
    #[getter] fn f(&self) -> f64 { self.matrix.get().values[13] }

    #[getter(name="is2D")] fn is_2d(&self)->bool {self.matrix.get().is_2d}
    #[getter] fn is_identity(&self)->bool {self.matrix.get().is_identity()}
    fn multiply(&self,ctx:&mut Ctx,#[default(Value::Undefined)]other:Value)->OpResult<DomMatrix> {Ok(DomMatrix::value(self.matrix.get().multiply(matrix_dictionary(ctx,&other)?)))}
    fn inverse(&self)->DomMatrix {DomMatrix::value(self.matrix.get().inverse())}
    #[method(coerce)] fn translate(&self,#[default(0.0)]x:f64,#[default(0.0)]y:f64,#[default(0.0)]z:f64)->DomMatrix {DomMatrix::value(self.matrix.get().translated(x,y,z))}
    #[method(coerce)] fn scale(&self,#[default(1.0)]x:f64,y:Option<f64>,#[default(1.0)]z:f64,#[default(0.0)]ox:f64,#[default(0.0)]oy:f64,#[default(0.0)]oz:f64)->DomMatrix {DomMatrix::value(self.matrix.get().scaled(x,y.unwrap_or(x),z,Point([ox,oy,oz,1.0])))}
    #[method(coerce)] fn scale_non_uniform(&self,#[default(1.0)]x:f64,#[default(1.0)]y:f64)->DomMatrix {DomMatrix::value(self.matrix.get().scaled(x,y,1.0,Point::default()))}
    #[method(coerce,name="scale3d")] fn scale_3d(&self,#[default(1.0)]scale:f64,#[default(0.0)]ox:f64,#[default(0.0)]oy:f64,#[default(0.0)]oz:f64)->DomMatrix {DomMatrix::value(self.matrix.get().scaled(scale,scale,scale,Point([ox,oy,oz,1.0])))}
    #[method(coerce)] fn rotate(&self,ctx:&mut Ctx,#[default(0.0)]x:f64,#[default(Value::Undefined)]y:Value,#[default(Value::Undefined)]z:Value)->OpResult<DomMatrix> {let (x,y,z)=rotation(ctx,x,y,z)?;Ok(DomMatrix::value(self.matrix.get().rotated(x,y,z)))}
    #[method(coerce)] fn rotate_axis_angle(&self,#[default(0.0)]x:f64,#[default(0.0)]y:f64,#[default(0.0)]z:f64,#[default(0.0)]angle:f64)->DomMatrix {DomMatrix::value(self.matrix.get().rotated_axis(x,y,z,angle))}
    #[method(coerce)] fn rotate_from_vector(&self,#[default(0.0)]x:f64,#[default(0.0)]y:f64)->DomMatrix {DomMatrix::value(self.matrix.get().rotated_from_vector(x,y))}
    #[method(coerce)] fn skew_x(&self,#[default(0.0)]angle:f64)->DomMatrix {DomMatrix::value(self.matrix.get().skewed(angle,0.0))}
    #[method(coerce)] fn skew_y(&self,#[default(0.0)]angle:f64)->DomMatrix {DomMatrix::value(self.matrix.get().skewed(0.0,angle))}
    fn flip_x(&self)->DomMatrix {DomMatrix::value(self.matrix.get().scaled(-1.0,1.0,1.0,Point::default()))}
    fn flip_y(&self)->DomMatrix {DomMatrix::value(self.matrix.get().scaled(1.0,-1.0,1.0,Point::default()))}
    fn transform_point(&self,ctx:&mut Ctx,#[default(Value::Undefined)]point:Value)->OpResult<DomPoint> {Ok(DomPoint::value(self.matrix.get().transform(point_dictionary(ctx,&point)?)))}
    fn to_float32_array(&self,ctx:&mut Ctx)->OpResult<Value> {float_array(ctx,self.matrix.get(),TaKind::F32)}
    fn to_float64_array(&self,ctx:&mut Ctx)->OpResult<Value> {float_array(ctx,self.matrix.get(),TaKind::F64)}
    #[method(name="toJSON")] fn to_json(&self,ctx:&mut Ctx)->OpResult<Value> {matrix_json(ctx,self.matrix.get())}
}
/// Shared branded matrix access for native consumers such as Typed OM.
pub(crate) fn matrix_native_value(ctx:&mut Ctx,value:&Value)->OpResult<Matrix>{
    ctx.with_instance::<DomMatrixReadOnly,_>(value,|value|value.matrix.get())
}
pub(crate) fn validate_mutable_matrix(ctx: &mut Ctx, value: &Value) -> OpResult<()> {
    ctx.with_instance::<DomMatrix, _>(value, |_| ())
}

pub(crate) fn matrix_native_instance(ctx:&mut Ctx,matrix:Matrix)->Value{
    ctx.new_instance(DomMatrix::value(matrix))
}

fn rotation(ctx:&mut Ctx,x:f64,y:Value,z:Value)->OpResult<(f64,f64,f64)> {
    if matches!(y,Value::Undefined)&&matches!(z,Value::Undefined) {return Ok((0.0,0.0,x))}
    Ok((x,if matches!(y,Value::Undefined){0.0}else{ctx.coerce_number(&y).map_err(OpError::thrown)?},if matches!(z,Value::Undefined){0.0}else{ctx.coerce_number(&z).map_err(OpError::thrown)?}))
}
#[lumen_bind::class(name="DOMMatrix",extends=DomMatrixReadOnly,hint(js(webidl,invalid_this)))]
pub struct DomMatrix {base:DomMatrixReadOnly}
impl DomMatrix {fn value(matrix:Matrix)->Self {Self{base:DomMatrixReadOnly::value(matrix)}}}
#[lumen_bind::methods]
impl DomMatrix {
    #[constructor] fn new(ctx:&mut Ctx,#[default(Value::Undefined)]init:Value)->OpResult<Self> {Ok(Self::value(matrix_constructor(ctx,&init)?))}
    fn from_matrix(ctx:&mut Ctx,#[default(Value::Undefined)]other:Value)->OpResult<Self> {Ok(Self::value(matrix_dictionary(ctx,&other)?))}
    fn from_float32_array(ctx:&mut Ctx,array:Value)->OpResult<Self> {Ok(Self::value(matrix_from_array(ctx,&array,TaKind::F32)?))}
    fn from_float64_array(ctx:&mut Ctx,array:Value)->OpResult<Self> {Ok(Self::value(matrix_from_array(ctx,&array,TaKind::F64)?))}
    #[getter] fn m11(&self) -> f64 { self.base.matrix.get().values[0] }
    #[setter] fn set_m11(&self, ctx: &mut Ctx, value: Value) -> OpResult<()> { let value=ctx.coerce_number(&value).map_err(OpError::thrown)?; let mut matrix=self.base.matrix.get(); matrix.set(0,value); self.base.matrix.set(matrix); Ok(()) }
    #[getter] fn m12(&self) -> f64 { self.base.matrix.get().values[1] }
    #[setter] fn set_m12(&self, ctx: &mut Ctx, value: Value) -> OpResult<()> { let value=ctx.coerce_number(&value).map_err(OpError::thrown)?; let mut matrix=self.base.matrix.get(); matrix.set(1,value); self.base.matrix.set(matrix); Ok(()) }
    #[getter] fn m13(&self) -> f64 { self.base.matrix.get().values[2] }
    #[setter] fn set_m13(&self, ctx: &mut Ctx, value: Value) -> OpResult<()> { let value=ctx.coerce_number(&value).map_err(OpError::thrown)?; let mut matrix=self.base.matrix.get(); matrix.set(2,value); self.base.matrix.set(matrix); Ok(()) }
    #[getter] fn m14(&self) -> f64 { self.base.matrix.get().values[3] }
    #[setter] fn set_m14(&self, ctx: &mut Ctx, value: Value) -> OpResult<()> { let value=ctx.coerce_number(&value).map_err(OpError::thrown)?; let mut matrix=self.base.matrix.get(); matrix.set(3,value); self.base.matrix.set(matrix); Ok(()) }
    #[getter] fn m21(&self) -> f64 { self.base.matrix.get().values[4] }
    #[setter] fn set_m21(&self, ctx: &mut Ctx, value: Value) -> OpResult<()> { let value=ctx.coerce_number(&value).map_err(OpError::thrown)?; let mut matrix=self.base.matrix.get(); matrix.set(4,value); self.base.matrix.set(matrix); Ok(()) }
    #[getter] fn m22(&self) -> f64 { self.base.matrix.get().values[5] }
    #[setter] fn set_m22(&self, ctx: &mut Ctx, value: Value) -> OpResult<()> { let value=ctx.coerce_number(&value).map_err(OpError::thrown)?; let mut matrix=self.base.matrix.get(); matrix.set(5,value); self.base.matrix.set(matrix); Ok(()) }
    #[getter] fn m23(&self) -> f64 { self.base.matrix.get().values[6] }
    #[setter] fn set_m23(&self, ctx: &mut Ctx, value: Value) -> OpResult<()> { let value=ctx.coerce_number(&value).map_err(OpError::thrown)?; let mut matrix=self.base.matrix.get(); matrix.set(6,value); self.base.matrix.set(matrix); Ok(()) }
    #[getter] fn m24(&self) -> f64 { self.base.matrix.get().values[7] }
    #[setter] fn set_m24(&self, ctx: &mut Ctx, value: Value) -> OpResult<()> { let value=ctx.coerce_number(&value).map_err(OpError::thrown)?; let mut matrix=self.base.matrix.get(); matrix.set(7,value); self.base.matrix.set(matrix); Ok(()) }
    #[getter] fn m31(&self) -> f64 { self.base.matrix.get().values[8] }
    #[setter] fn set_m31(&self, ctx: &mut Ctx, value: Value) -> OpResult<()> { let value=ctx.coerce_number(&value).map_err(OpError::thrown)?; let mut matrix=self.base.matrix.get(); matrix.set(8,value); self.base.matrix.set(matrix); Ok(()) }
    #[getter] fn m32(&self) -> f64 { self.base.matrix.get().values[9] }
    #[setter] fn set_m32(&self, ctx: &mut Ctx, value: Value) -> OpResult<()> { let value=ctx.coerce_number(&value).map_err(OpError::thrown)?; let mut matrix=self.base.matrix.get(); matrix.set(9,value); self.base.matrix.set(matrix); Ok(()) }
    #[getter] fn m33(&self) -> f64 { self.base.matrix.get().values[10] }
    #[setter] fn set_m33(&self, ctx: &mut Ctx, value: Value) -> OpResult<()> { let value=ctx.coerce_number(&value).map_err(OpError::thrown)?; let mut matrix=self.base.matrix.get(); matrix.set(10,value); self.base.matrix.set(matrix); Ok(()) }
    #[getter] fn m34(&self) -> f64 { self.base.matrix.get().values[11] }
    #[setter] fn set_m34(&self, ctx: &mut Ctx, value: Value) -> OpResult<()> { let value=ctx.coerce_number(&value).map_err(OpError::thrown)?; let mut matrix=self.base.matrix.get(); matrix.set(11,value); self.base.matrix.set(matrix); Ok(()) }
    #[getter] fn m41(&self) -> f64 { self.base.matrix.get().values[12] }
    #[setter] fn set_m41(&self, ctx: &mut Ctx, value: Value) -> OpResult<()> { let value=ctx.coerce_number(&value).map_err(OpError::thrown)?; let mut matrix=self.base.matrix.get(); matrix.set(12,value); self.base.matrix.set(matrix); Ok(()) }
    #[getter] fn m42(&self) -> f64 { self.base.matrix.get().values[13] }
    #[setter] fn set_m42(&self, ctx: &mut Ctx, value: Value) -> OpResult<()> { let value=ctx.coerce_number(&value).map_err(OpError::thrown)?; let mut matrix=self.base.matrix.get(); matrix.set(13,value); self.base.matrix.set(matrix); Ok(()) }
    #[getter] fn m43(&self) -> f64 { self.base.matrix.get().values[14] }
    #[setter] fn set_m43(&self, ctx: &mut Ctx, value: Value) -> OpResult<()> { let value=ctx.coerce_number(&value).map_err(OpError::thrown)?; let mut matrix=self.base.matrix.get(); matrix.set(14,value); self.base.matrix.set(matrix); Ok(()) }
    #[getter] fn m44(&self) -> f64 { self.base.matrix.get().values[15] }
    #[setter] fn set_m44(&self, ctx: &mut Ctx, value: Value) -> OpResult<()> { let value=ctx.coerce_number(&value).map_err(OpError::thrown)?; let mut matrix=self.base.matrix.get(); matrix.set(15,value); self.base.matrix.set(matrix); Ok(()) }
    #[getter] fn a(&self) -> f64 { self.base.matrix.get().values[0] }
    #[setter] fn set_a(&self, ctx: &mut Ctx, value: Value) -> OpResult<()> { let value=ctx.coerce_number(&value).map_err(OpError::thrown)?; let mut matrix=self.base.matrix.get(); matrix.set(0,value); self.base.matrix.set(matrix); Ok(()) }
    #[getter] fn b(&self) -> f64 { self.base.matrix.get().values[1] }
    #[setter] fn set_b(&self, ctx: &mut Ctx, value: Value) -> OpResult<()> { let value=ctx.coerce_number(&value).map_err(OpError::thrown)?; let mut matrix=self.base.matrix.get(); matrix.set(1,value); self.base.matrix.set(matrix); Ok(()) }
    #[getter] fn c(&self) -> f64 { self.base.matrix.get().values[4] }
    #[setter] fn set_c(&self, ctx: &mut Ctx, value: Value) -> OpResult<()> { let value=ctx.coerce_number(&value).map_err(OpError::thrown)?; let mut matrix=self.base.matrix.get(); matrix.set(4,value); self.base.matrix.set(matrix); Ok(()) }
    #[getter] fn d(&self) -> f64 { self.base.matrix.get().values[5] }
    #[setter] fn set_d(&self, ctx: &mut Ctx, value: Value) -> OpResult<()> { let value=ctx.coerce_number(&value).map_err(OpError::thrown)?; let mut matrix=self.base.matrix.get(); matrix.set(5,value); self.base.matrix.set(matrix); Ok(()) }
    #[getter] fn e(&self) -> f64 { self.base.matrix.get().values[12] }
    #[setter] fn set_e(&self, ctx: &mut Ctx, value: Value) -> OpResult<()> { let value=ctx.coerce_number(&value).map_err(OpError::thrown)?; let mut matrix=self.base.matrix.get(); matrix.set(12,value); self.base.matrix.set(matrix); Ok(()) }
    #[getter] fn f(&self) -> f64 { self.base.matrix.get().values[13] }
    #[setter] fn set_f(&self, ctx: &mut Ctx, value: Value) -> OpResult<()> { let value=ctx.coerce_number(&value).map_err(OpError::thrown)?; let mut matrix=self.base.matrix.get(); matrix.set(13,value); self.base.matrix.set(matrix); Ok(()) }

    fn multiply_self(&self,ctx:&mut Ctx,this:This<Value>,#[default(Value::Undefined)]other:Value)->OpResult<Value> {self.base.matrix.set(self.base.matrix.get().multiply(matrix_dictionary(ctx,&other)?));Ok(this.0)}
    fn pre_multiply_self(&self,ctx:&mut Ctx,this:This<Value>,#[default(Value::Undefined)]other:Value)->OpResult<Value> {self.base.matrix.set(matrix_dictionary(ctx,&other)?.multiply(self.base.matrix.get()));Ok(this.0)}
    fn invert_self(&self,this:This<Value>)->Value {self.base.matrix.set(self.base.matrix.get().inverse());this.0}
    #[method(coerce)] fn translate_self(&self,this:This<Value>,#[default(0.0)]x:f64,#[default(0.0)]y:f64,#[default(0.0)]z:f64)->Value {self.base.matrix.set(self.base.matrix.get().translated(x,y,z));this.0}
    #[method(coerce)] fn scale_self(&self,this:This<Value>,#[default(1.0)]x:f64,y:Option<f64>,#[default(1.0)]z:f64,#[default(0.0)]ox:f64,#[default(0.0)]oy:f64,#[default(0.0)]oz:f64)->Value {self.base.matrix.set(self.base.matrix.get().scaled(x,y.unwrap_or(x),z,Point([ox,oy,oz,1.0])));this.0}
    #[method(coerce,name="scale3dSelf")] fn scale_3d_self(&self,this:This<Value>,#[default(1.0)]scale:f64,#[default(0.0)]ox:f64,#[default(0.0)]oy:f64,#[default(0.0)]oz:f64)->Value {self.base.matrix.set(self.base.matrix.get().scaled(scale,scale,scale,Point([ox,oy,oz,1.0])));this.0}
    #[method(coerce)] fn rotate_self(&self,ctx:&mut Ctx,this:This<Value>,#[default(0.0)]x:f64,#[default(Value::Undefined)]y:Value,#[default(Value::Undefined)]z:Value)->OpResult<Value> {let (x,y,z)=rotation(ctx,x,y,z)?;self.base.matrix.set(self.base.matrix.get().rotated(x,y,z));Ok(this.0)}
    #[method(coerce)] fn rotate_axis_angle_self(&self,this:This<Value>,#[default(0.0)]x:f64,#[default(0.0)]y:f64,#[default(0.0)]z:f64,#[default(0.0)]angle:f64)->Value {self.base.matrix.set(self.base.matrix.get().rotated_axis(x,y,z,angle));this.0}
    #[method(coerce)] fn rotate_from_vector_self(&self,this:This<Value>,#[default(0.0)]x:f64,#[default(0.0)]y:f64)->Value {self.base.matrix.set(self.base.matrix.get().rotated_from_vector(x,y));this.0}
    #[method(coerce)] fn skew_x_self(&self,this:This<Value>,#[default(0.0)]angle:f64)->Value {self.base.matrix.set(self.base.matrix.get().skewed(angle,0.0));this.0}
    #[method(coerce)] fn skew_y_self(&self,this:This<Value>,#[default(0.0)]angle:f64)->Value {self.base.matrix.set(self.base.matrix.get().skewed(0.0,angle));this.0}
}

trait GeometryCodec: lumen_bind::Methods<JsHost>+Sized+'static {
    const KIND:&'static str;
    const MAX_BYTES:usize;
    fn encode(&self)->Vec<u8>;
    fn decode(bytes:&[u8])->Option<Self>;
}
fn register<T:GeometryCodec>(ctx:&mut Ctx) {
    lumen_host::clone_transfer::register_native_value_codec(ctx,lumen_host::clone_transfer::NativeValueCodec {
        kind:T::KIND,max_bytes:T::MAX_BYTES,
        matches:|ctx,value|ctx.with_instance::<T,_>(value,|_|()).is_ok(),
        serialize:|ctx,value|ctx.with_instance::<T,_>(value,GeometryCodec::encode),
        deserialize:|ctx,bytes|T::decode(bytes).map(|value|ctx.new_instance(value)).ok_or_else(||OpError::thrown(lumen_host::events::dom_exception(ctx,"Malformed geometry snapshot","DataCloneError"))),
    });
}
fn encode_matrix(matrix:Matrix)->Vec<u8> {
    let mut bytes=vec![u8::from(matrix.is_2d)];
    if matrix.is_2d {bytes.extend(core::encode_numbers([0,1,4,5,12,13].map(|index|matrix.values[index])))}
    else {bytes.extend(core::encode_numbers(matrix.values))}
    bytes
}
fn decode_matrix(bytes:&[u8])->Option<Matrix> {
    let (&flag,numbers)=bytes.split_first()?;if flag>1{return None}
    if flag==1 {core::decode_numbers::<6>(numbers).map(Matrix::from_affine)}
    else {core::decode_numbers::<16>(numbers).map(|values|Matrix{values,is_2d:false})}
}
impl GeometryCodec for DomMatrix {const KIND:&'static str="DOMMatrix";const MAX_BYTES:usize=129;fn encode(&self)->Vec<u8>{encode_matrix(self.base.matrix.get())}fn decode(bytes:&[u8])->Option<Self>{decode_matrix(bytes).map(Self::value)}}
impl GeometryCodec for DomMatrixReadOnly {const KIND:&'static str="DOMMatrixReadOnly";const MAX_BYTES:usize=129;fn encode(&self)->Vec<u8>{encode_matrix(self.matrix.get())}fn decode(bytes:&[u8])->Option<Self>{decode_matrix(bytes).map(Self::value)}}
impl GeometryCodec for DomPoint {const KIND:&'static str="DOMPoint";const MAX_BYTES:usize=32;fn encode(&self)->Vec<u8>{core::encode_numbers(self.base.point.get().0)}fn decode(bytes:&[u8])->Option<Self>{core::decode_numbers::<4>(bytes).map(|values|Self::value(Point(values)))}}
impl GeometryCodec for DomPointReadOnly {const KIND:&'static str="DOMPointReadOnly";const MAX_BYTES:usize=32;fn encode(&self)->Vec<u8>{core::encode_numbers(self.point.get().0)}fn decode(bytes:&[u8])->Option<Self>{core::decode_numbers::<4>(bytes).map(|values|Self{point:Cell::new(Point(values))})}}
impl GeometryCodec for DomRect {const KIND:&'static str="DOMRect";const MAX_BYTES:usize=32;fn encode(&self)->Vec<u8>{core::encode_numbers([self.base.x,self.base.y,self.base.width,self.base.height])}fn decode(bytes:&[u8])->Option<Self>{core::decode_numbers::<4>(bytes).map(|[x,y,width,height]|Self{base:DomRectReadOnly{x,y,width,height}})}}
impl GeometryCodec for DomRectReadOnly {const KIND:&'static str="DOMRectReadOnly";const MAX_BYTES:usize=32;fn encode(&self)->Vec<u8>{core::encode_numbers([self.x,self.y,self.width,self.height])}fn decode(bytes:&[u8])->Option<Self>{core::decode_numbers::<4>(bytes).map(|[x,y,width,height]|Self{x,y,width,height})}}

pub(super) fn install(ctx:&mut Ctx,window:bool)->OpResult<()> {
    RealmServices::replace_current(ctx,GeometryContext{window});
    let global=ctx.global_object();
    for (name,value) in [("DOMPointReadOnly",ctx.class_constructor::<DomPointReadOnly>()),("DOMPoint",ctx.class_constructor::<DomPoint>()),("DOMMatrixReadOnly",ctx.class_constructor::<DomMatrixReadOnly>()),("DOMMatrix",ctx.class_constructor::<DomMatrix>())] {
        crate::install_interface(ctx,&global,name,value).map_err(OpError::thrown)?;
    }
    // Mutable brands precede their readonly base brands in matching order.
    register::<DomPoint>(ctx);register::<DomPointReadOnly>(ctx);
    register::<DomMatrix>(ctx);register::<DomMatrixReadOnly>(ctx);
    register::<DomRect>(ctx);register::<DomRectReadOnly>(ctx);
    if window {
        for (name,value) in [("SVGPoint",ctx.class_constructor::<DomPoint>()),("SVGMatrix",ctx.class_constructor::<DomMatrix>()),("WebKitCSSMatrix",ctx.class_constructor::<DomMatrix>()),("SVGRect",ctx.class_constructor::<DomRect>())] {
            crate::install_interface(ctx,&global,name,value).map_err(OpError::thrown)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn geometry_snapshot_rejects_invalid_matrix_flags_and_preserves_scalar_bits() {
        let matrix=Matrix::from_affine([1.0,0.0,0.0,1.0,-0.0,f64::from_bits(0x7ff800000000002a)]);
        let bytes=encode_matrix(matrix);
        let decoded=decode_matrix(&bytes).unwrap();
        assert_eq!(decoded.values.map(f64::to_bits),matrix.values.map(f64::to_bits));
        let mut malformed=bytes.clone();malformed[0]=2;assert!(decode_matrix(&malformed).is_none());
        assert!(decode_matrix(&bytes[..48]).is_none());
        let mut three_d=Matrix::default();three_d.values[2]=1.0;
        assert_eq!(decode_matrix(&encode_matrix(three_d)).unwrap().values[2],0.0);
        three_d.is_2d=false;assert!(decode_matrix(&encode_matrix(three_d)).is_some());
    }
}
