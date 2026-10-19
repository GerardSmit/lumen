//! Resolved SVG filter primitive graph. Markup and resource loading belong to
//! the host; all input references here already name preceding primitives.
use alloc::sync::Arc;
use alloc::vec::Vec;
use alloc::collections::BTreeMap;
use alloc::borrow::Cow;
use crate::color::{Color,ColorSpace};

#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub enum Input {SourceGraphic,SourceAlpha,Result(u32),FillPaint,StrokePaint,BackgroundImage,BackgroundAlpha}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub enum Units {ObjectBoundingBox,UserSpaceOnUse}
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub enum EdgeMode {None,Duplicate,Wrap,Mirror}
#[derive(Clone,Copy,Debug,PartialEq)]
pub struct Coordinate {pub value:f32,pub percentage:bool}
impl Coordinate {
    pub fn resolve(self,basis:f32,units:Units)->Option<f32> {
        let value=if self.percentage || units==Units::ObjectBoundingBox {self.value*basis}else{self.value};
        value.is_finite().then_some(value)
    }
}
#[derive(Clone,Copy,Debug,PartialEq)]
pub enum Composite {Over,In,Out,Atop,Xor,Lighter,Arithmetic([f32;4])}
/// Compositing and Blending Level 1 modes; raster implementation belongs to
/// the host's maintained compositing backend.
#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub enum BlendMode {Normal,Multiply,Screen,Overlay,Darken,Lighten,ColorDodge,ColorBurn,HardLight,SoftLight,Difference,Exclusion,Hue,Saturation,Color,Luminosity}
impl BlendMode {
    pub fn from_keyword(value:&str)->Option<Self>{Some(match value {
        "normal"=>Self::Normal,"multiply"=>Self::Multiply,"screen"=>Self::Screen,"overlay"=>Self::Overlay,
        "darken"=>Self::Darken,"lighten"=>Self::Lighten,"color-dodge"=>Self::ColorDodge,"color-burn"=>Self::ColorBurn,
        "hard-light"=>Self::HardLight,"soft-light"=>Self::SoftLight,"difference"=>Self::Difference,"exclusion"=>Self::Exclusion,
        "hue"=>Self::Hue,"saturation"=>Self::Saturation,"color"=>Self::Color,"luminosity"=>Self::Luminosity,_=>return None,
    })}
}
#[derive(Clone,Debug,PartialEq)]
pub enum Transfer {Identity,Table(Arc<[f32]>),Discrete(Arc<[f32]>),Linear{ slope:f32,intercept:f32 },Gamma{amplitude:f32,exponent:f32,offset:f32}}
impl Transfer {
    /// Component transfer consumes non-premultiplied components, clamping
    /// after the complete transfer function rather than each table operand.
    pub fn sample(&self,value:f32)->f32 {
        let value=value.clamp(0.0,1.0);
        let out=match self {
            Self::Identity=>value,
            Self::Table(values)=>match values.len(){
                0=>value,1=>values[0],count=>{
                    let position=value*(count-1) as f32;
                    let at=(position as usize).min(count-1);
                    let next=(at+1).min(count-1);
                    (f64::from(values[at])+f64::from(position-at as f32)*(f64::from(values[next])-f64::from(values[at]))) as f32
                }
            },
            Self::Discrete(values)=>if values.is_empty(){value}else{values[((value*values.len() as f32) as usize).min(values.len()-1)]},
            Self::Linear{slope,intercept}=>slope*value+intercept,
            Self::Gamma{amplitude,exponent,offset}=>if *amplitude==0.0{*offset}else{amplitude*libm::powf(value,*exponent)+offset},
        };
        out.clamp(0.0,1.0)
    }
    fn bytes(&self)->Option<usize> {match self {Self::Table(v)|Self::Discrete(v)=>v.len().checked_mul(core::mem::size_of::<f32>()).and_then(|v|v.checked_add(2*core::mem::size_of::<usize>())),_=>Some(0)}}
    fn valid(&self)->bool {match self {
        Self::Identity=>true,Self::Table(v)|Self::Discrete(v)=>v.iter().all(|v|v.is_finite()),
        Self::Linear{slope,intercept}=>slope.is_finite() && intercept.is_finite(),
        Self::Gamma{amplitude,exponent,offset}=>amplitude.is_finite() && exponent.is_finite() && offset.is_finite(),
    }}
}
#[derive(Clone,Debug,PartialEq)]
pub enum Primitive {
    Transparent,
    GaussianBlur {sigma:[f32;2],edge:EdgeMode},
    Matrix(Arc<[[f32;5];4]>),ComponentTransfer(Arc<[Transfer;4]>),
    Offset([f32;2]),Flood(Color),Composite(Composite),Blend{mode:BlendMode,composite:bool},Merge,
    /// The public feDropShadow region derives from its original input, not
    /// the private flood/merge tree used to evaluate the shorthand.
    DropShadowMerge { original:Input },
}
impl Primitive {
    /// Point primitives use the selected SVG working space; conversions use
    /// the same color core as CSS colors and gradients. Spatial sampling and
    /// region clipping remain the host graph's responsibility.
    pub fn sample(&self,first:Color,second:Color,space:ColorSpace)->Option<Color> {
        let unpremultiplied=|source:Color|{let mut source=source.to(space);if source.alpha==0.0{source.components=[0.0;3];}source};
        let first=unpremultiplied(first);let second=unpremultiplied(second);
        let input=[first.components[0],first.components[1],first.components[2],first.alpha];
        let output=match self {
            Self::Transparent=>[0.0;4],
            Self::Flood(color)=>{let color=color.to(space);[color.components[0],color.components[1],color.components[2],color.alpha]},
            Self::Matrix(matrix)=>{let out=super::ColorMatrix::from_coefficients(**matrix).apply_in_space(first,space);[out.components[0],out.components[1],out.components[2],out.alpha]},
            Self::ComponentTransfer(functions)=>core::array::from_fn(|channel|functions[channel].sample(input[channel])),
            Self::Composite(operator)=>{
                let a=first.alpha.clamp(0.0,1.0);let b=second.alpha.clamp(0.0,1.0);
                let left=[first.components[0]*a,first.components[1]*a,first.components[2]*a,a];
                let right=[second.components[0]*b,second.components[1]*b,second.components[2]*b,b];
                let weights=match operator{Composite::Over=>(1.0,1.0-a),Composite::In=>(b,0.0),Composite::Out=>(1.0-b,0.0),Composite::Atop=>(b,1.0-a),Composite::Xor=>(1.0-b,1.0-a),Composite::Lighter=>(1.0,1.0),Composite::Arithmetic(_)=>(0.0,0.0)};
                let mut premult:[f32;4]=core::array::from_fn(|channel|match operator {
                    Composite::Arithmetic(k)=>(f64::from(k[0])*f64::from(left[channel])*f64::from(right[channel])+f64::from(k[1])*f64::from(left[channel])+f64::from(k[2])*f64::from(right[channel])+f64::from(k[3])).clamp(0.0,1.0) as f32,
                    _=>(left[channel]*weights.0+right[channel]*weights.1).clamp(0.0,1.0),
                });
                // Arithmetic can produce channels above output alpha; SVG
                // premultiplied results require them bounded by that alpha.
                for channel in 0..3{premult[channel]=premult[channel].min(premult[3]);}
                if premult[3]>0.0 {for channel in 0..3{premult[channel]/=premult[3];}}
                premult
            }
            _=>return None,
        };
        let alpha=output[3].clamp(0.0,1.0);
        let components=if alpha==0.0{[0.0;3]}else{[output[0].clamp(0.0,1.0),output[1].clamp(0.0,1.0),output[2].clamp(0.0,1.0)]};
        Some(Color::new(space,components,alpha,0))
    }
}
#[derive(Clone,Debug,PartialEq)]
pub struct Node {
    pub operation:Primitive,pub inputs:Arc<[Input]>,
    /// Omitted subregion axes retain input-region defaults. They cannot be
    /// replaced with a universal 0%,0%,100%,100% without losing input unions.
    pub region:[Option<Coordinate>;4],pub color_space:ColorSpace,
}
impl Primitive {
    pub fn payload_bytes(&self)->Option<usize> {
        match self {
            Self::Matrix(_)=>core::mem::size_of::<[[f32;5];4]>().checked_add(2*core::mem::size_of::<usize>()),
            Self::ComponentTransfer(values)=>{
                let mut payload=core::mem::size_of::<[Transfer;4]>().checked_add(2*core::mem::size_of::<usize>())?;
                for value in values.iter(){payload=payload.checked_add(value.bytes()?)?;}Some(payload)
            }
            _=>Some(0),
        }
    }
}
impl Node {
    pub fn bytes(&self)->Option<usize> {
        self.inputs.len().checked_mul(core::mem::size_of::<Input>())?
            .checked_add(2*core::mem::size_of::<usize>())?.checked_add(self.operation.payload_bytes()?)
    }
}
#[derive(Clone,Debug,PartialEq)]
pub struct Program {
    region:[Coordinate;4],filter_units:Units,primitive_units:Units,
    nodes:Arc<[Node]>,retained_bytes:usize,
}
impl Program {
    /// All retained primitive storage is charged before publication. The
    /// renderer adds its actual window, dependency and temporary allocations.
    fn measure_bytes(&self)->Option<usize> {
        let mut bytes=core::mem::size_of::<Self>().checked_add(2*core::mem::size_of::<usize>())?
            .checked_add(self.nodes.len().checked_mul(core::mem::size_of::<Node>())?)?
            .checked_add(2*core::mem::size_of::<usize>())?;
        for node in self.nodes.iter() {bytes=bytes.checked_add(node.bytes()?)?;}Some(bytes)
    }
    pub fn bytes(&self)->usize {self.retained_bytes}
    pub fn region(&self)->[Coordinate;4] {self.region}
    pub fn filter_units(&self)->Units {self.filter_units}
    pub fn primitive_units(&self)->Units {self.primitive_units}
    pub fn nodes(&self)->&[Node] {&self.nodes}
    pub fn valid(&self,available:usize)->bool {self.retained_bytes<=available}
    fn valid_structure(&self)->bool {
        if self.nodes.len()>u32::MAX as usize || self.region.iter().any(|v|!v.value.is_finite()) {return false;}
        self.nodes.iter().enumerate().all(|(at,node)| {
            if !matches!(node.color_space,ColorSpace::Srgb|ColorSpace::SrgbLinear)
                || node.region.iter().flatten().any(|v|!v.value.is_finite())
                || node.inputs.iter().any(|input|matches!(input,Input::Result(value) if *value as usize>=at)) {return false;}
            match &node.operation {
                Primitive::Transparent=>node.inputs.is_empty(),
                Primitive::GaussianBlur{sigma,..}=>node.inputs.len()==1 && sigma.iter().all(|v|v.is_finite() && *v>=0.0),
                Primitive::Matrix(matrix)=>node.inputs.len()==1 && matrix.iter().flatten().all(|v|v.is_finite()),
                Primitive::ComponentTransfer(values)=>node.inputs.len()==1 && values.iter().all(Transfer::valid),
                Primitive::Offset(offset)=>node.inputs.len()==1 && offset.iter().all(|v|v.is_finite()),
                Primitive::Flood(color)=>node.inputs.is_empty() && color.is_finite(),
                Primitive::Composite(operator)=>node.inputs.len()==2 && match operator{Composite::Arithmetic(values)=>values.iter().all(|v|v.is_finite()),_=>true},
                Primitive::Blend{..}=>node.inputs.len()==2,
                Primitive::Merge=>true,
                Primitive::DropShadowMerge{original}=>node.inputs.len()==2 && node.inputs[1]==*original
                    && !matches!(original,Input::Result(value) if *value as usize>=at),
            }
        })
    }
}

#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub enum BuildError {Invalid,BudgetExceeded}

/// Source result names are borrowed only while compiling the resource. The
/// published program stores numeric edges, and cannot retain a DOM lifetime.
pub struct Builder<'a> {nodes:Vec<Node>,names:BTreeMap<Cow<'a,str>,u32>,payload:usize,name_bytes:usize}
impl<'a> Builder<'a> {
    pub fn new()->Self {Self{nodes:Vec::new(),names:BTreeMap::new(),payload:0,name_bytes:0}}
    fn bytes(&self)->Option<usize> {
        self.nodes.capacity().checked_mul(core::mem::size_of::<Node>())?
            .checked_add(self.names.len().checked_mul(core::mem::size_of::<(Cow<str>,u32)>()*16+128)?)?
            .checked_add(self.payload)?.checked_add(self.name_bytes)
    }
    pub fn remaining(&self,available:usize)->Option<usize> {available.checked_sub(self.bytes()?)}
    pub fn input(&self,name:Option<&str>)->Input {
        let implicit=||self.nodes.len().checked_sub(1).map_or(Input::SourceGraphic,|at|Input::Result(at as u32));
        match name {
            Some("SourceGraphic")=>Input::SourceGraphic,Some("SourceAlpha")=>Input::SourceAlpha,
            Some("BackgroundImage")=>Input::BackgroundImage,Some("BackgroundAlpha")=>Input::BackgroundAlpha,
            Some("FillPaint")=>Input::FillPaint,Some("StrokePaint")=>Input::StrokePaint,
            Some(name)=>self.names.get(name).map_or_else(implicit,|at|Input::Result(*at)),
            None=>implicit(),
        }
    }
    /// Publish one private shorthand step without exposing a source result
    /// name. Its numeric identity uses the same bounded graph authority.
    pub fn push_anonymous(&mut self,node:Node,available:usize)->Result<Input,BuildError> {
        let at=u32::try_from(self.nodes.len()).map_err(|_|BuildError::BudgetExceeded)?;
        self.push(node,None,available)?;Ok(Input::Result(at))
    }
    pub fn push(&mut self,node:Node,result:Option<Cow<'a,str>>,available:usize)->Result<(),BuildError> {
        let at=u32::try_from(self.nodes.len()).map_err(|_|BuildError::BudgetExceeded)?;
        // Validate the node with its original preceding-result identities.
        if node.inputs.iter().any(|input|matches!(input,Input::Result(input) if *input>=at)){return Err(BuildError::Invalid);}
        let payload=node.bytes().ok_or(BuildError::BudgetExceeded)?;
        let current=self.bytes().ok_or(BuildError::BudgetExceeded)?;
        let requested=if self.nodes.len()==self.nodes.capacity(){self.nodes.len().checked_add(1).and_then(|count|self.nodes.capacity().checked_mul(2).map(|grown|count.max(grown))).ok_or(BuildError::BudgetExceeded)?}else{self.nodes.capacity()};
        let fresh_name=result.as_ref().is_some_and(|name|!self.names.contains_key(name.as_ref()));
        let temporary_name=result.as_ref().map_or(0,|name|match name{Cow::Owned(name)=>name.capacity(),Cow::Borrowed(_)=>0});
        let extra_names=if fresh_name{core::mem::size_of::<(Cow<str>,u32)>()*16+128}else{0};
        // Existing storage remains live during each vector reallocation.
        let extra_nodes=if requested>self.nodes.capacity(){requested.checked_mul(core::mem::size_of::<Node>()).ok_or(BuildError::BudgetExceeded)?}else{0};
        current.checked_add(payload).and_then(|bytes|bytes.checked_add(extra_nodes)).and_then(|bytes|bytes.checked_add(extra_names)).and_then(|bytes|bytes.checked_add(temporary_name)).filter(|bytes|*bytes<=available).ok_or(BuildError::BudgetExceeded)?;
        if extra_nodes!=0 {self.nodes.try_reserve_exact(requested-self.nodes.len()).map_err(|_|BuildError::BudgetExceeded)?;}
        self.payload=self.payload.checked_add(payload).ok_or(BuildError::BudgetExceeded)?;
        if self.bytes().is_none_or(|bytes|bytes>available){return Err(BuildError::BudgetExceeded);}
        self.nodes.push(node);if let Some(result)=result{if fresh_name{self.name_bytes=self.name_bytes.checked_add(temporary_name).ok_or(BuildError::BudgetExceeded)?;}self.names.insert(result,at);}Ok(())
    }
    pub fn finish(self,region:[Coordinate;4],filter_units:Units,primitive_units:Units,available:usize)->Result<Arc<Program>,BuildError> {
        let published=self.nodes.len().checked_mul(core::mem::size_of::<Node>()).and_then(|bytes|bytes.checked_add(core::mem::size_of::<Program>()+4*core::mem::size_of::<usize>())).ok_or(BuildError::BudgetExceeded)?;
        self.bytes().and_then(|bytes|bytes.checked_add(published)).filter(|bytes|*bytes<=available).ok_or(BuildError::BudgetExceeded)?;
        let mut program=Program{region,filter_units,primitive_units,nodes:self.nodes.into(),retained_bytes:0};
        program.retained_bytes=program.measure_bytes().ok_or(BuildError::BudgetExceeded)?;
        if program.retained_bytes>available{return Err(BuildError::BudgetExceeded);}
        if !program.valid_structure(){return Err(BuildError::Invalid);}Ok(Arc::new(program))
    }
}

#[derive(Clone,Copy,Debug,PartialEq)]
pub enum PaintInput {None,Solid(Color),Unsupported}
/// One used reference preserves the referencing element's local space. SVG
/// geometric boxes and viewport axes must not be reconstructed from ink crops.
#[derive(Clone,Debug,PartialEq)]
pub struct Use {
    pub url:Arc<str>,pub program:Arc<Program>,pub reference_box:[f32;4],pub viewport:[f32;4],
    pub owner_transform:[f32;6],pub fill:PaintInput,pub stroke:PaintInput,
}
impl Use {
    pub fn valid(&self,available:usize)->bool {
        self.url.len()<=8192 && self.program.valid(available) && self.reference_box.iter().chain(&self.viewport).chain(&self.owner_transform).all(|v|v.is_finite())
            && self.reference_box[2]>=0.0 && self.reference_box[3]>=0.0 && self.viewport[2]>=0.0 && self.viewport[3]>=0.0
            && [self.fill,self.stroke].into_iter().all(|paint|match paint{PaintInput::Solid(color)=>color.is_finite(),_=>true})
    }
    /// Resolve primitive lengths independently on the reference box axes.
    /// This returns local user-space values; the renderer applies the actual
    /// owner matrix when choosing a raster space, never a global scalar sigma.
    pub fn primitive_lengths(&self,values:[f32;2])->Option<[f32;2]> {
        let values=if self.program.primitive_units==Units::ObjectBoundingBox{
            [values[0]*self.reference_box[2],values[1]*self.reference_box[3]]
        }else{values};
        values.iter().all(|v|v.is_finite()).then_some(values)
    }
    /// Default subregions are unions of input subregions. Standard inputs and
    /// generator primitives instead use the complete filter region. Explicit
    /// axes then override that default before the mandatory filter intersection.
    pub fn primitive_region(&self,node:&Node,input_union:Option<[f32;4]>)->Option<[f32;4]> {
        let filter=self.filter_region()?;
        if filter[2]<=0.0 || filter[3]<=0.0{return None;}
        let region_inputs=match &node.operation {
            Primitive::DropShadowMerge{original}=>core::slice::from_ref(original),
            _=>node.inputs.as_ref(),
        };
        let standard=region_inputs.iter().any(|input|!matches!(input,Input::Result(_)));
        let mut rect=if region_inputs.is_empty() || standard{filter}else{input_union.unwrap_or([filter[0],filter[1],0.0,0.0])};
        let units=self.program.primitive_units;
        let basis=if units==Units::ObjectBoundingBox{self.reference_box}else{self.viewport};
        for axis in 0..4 {
            if let Some(coordinate)=node.region[axis] {
                rect[axis]=coordinate.resolve(basis[2+axis%2],units)?;
                if axis<2 && units==Units::ObjectBoundingBox{rect[axis]+=basis[axis];}
            }
        }
        if rect.iter().any(|v|!v.is_finite()) || rect[2]<=0.0 || rect[3]<=0.0{return None;}
        let left=rect[0].max(filter[0]);let top=rect[1].max(filter[1]);
        let right=(rect[0]+rect[2]).min(filter[0]+filter[2]);let bottom=(rect[1]+rect[3]).min(filter[1]+filter[3]);
        (right>left && bottom>top && right.is_finite() && bottom.is_finite()).then_some([left,top,right-left,bottom-top])
    }
    /// Subregion geometry is resolved once per actual reference. It depends
    /// on preceding declared regions, never on alpha crops of cached images.
    pub fn resolved_regions(&self,available:usize)->Result<Arc<[Option<[f32;4]>]>,BuildError> {
        let count=self.program.nodes.len();let slot=core::mem::size_of::<Option<[f32;4]>>();
        count.checked_mul(slot).and_then(|bytes|bytes.checked_mul(2)).and_then(|bytes|bytes.checked_add(2*core::mem::size_of::<usize>())).filter(|bytes|*bytes<=available).ok_or(BuildError::BudgetExceeded)?;
        let mut regions:Vec<Option<[f32;4]>>=Vec::new();regions.try_reserve_exact(count).map_err(|_|BuildError::BudgetExceeded)?;
        regions.capacity().checked_add(count).and_then(|count|count.checked_mul(slot)).and_then(|bytes|bytes.checked_add(2*core::mem::size_of::<usize>())).filter(|bytes|*bytes<=available).ok_or(BuildError::BudgetExceeded)?;
        for node in self.program.nodes.iter() {
            let mut union:Option<[f32;4]>=None;
            let region_inputs=match &node.operation {
                Primitive::DropShadowMerge{original}=>core::slice::from_ref(original),
                _=>node.inputs.as_ref(),
            };
            for input in region_inputs.iter(){
                if let Input::Result(at)=input {
                    if let Some(rect)=regions.get(*at as usize).ok_or(BuildError::Invalid)? {
                        union=Some(if let Some(previous)=union {
                            let left=previous[0].min(rect[0]);let top=previous[1].min(rect[1]);
                            let right=(previous[0]+previous[2]).max(rect[0]+rect[2]);let bottom=(previous[1]+previous[3]).max(rect[1]+rect[3]);
                            if !right.is_finite() || !bottom.is_finite(){return Err(BuildError::Invalid);}
                            [left,top,right-left,bottom-top]
                        }else{*rect});
                    }
                }
            }
            regions.push(self.primitive_region(node,union));
        }
        Ok(regions.into())
    }
    pub fn filter_region(&self)->Option<[f32;4]> {
        let units=self.program.filter_units;
        let basis=if units==Units::ObjectBoundingBox{self.reference_box}else{self.viewport};
        let mut rect=[0.0;4];
        for axis in 0..4 {rect[axis]=self.program.region[axis].resolve(basis[2+axis%2],units)?;
            if axis<2 && units==Units::ObjectBoundingBox {rect[axis]+=basis[axis];}}
        rect.iter().all(|v|v.is_finite()).then_some(rect)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn node(operation:Primitive,inputs:&[Input])->Node {Node{operation,inputs:Arc::from(inputs),region:[None;4],color_space:ColorSpace::SrgbLinear}}
    fn region()->[Coordinate;4] {[Coordinate{value:-0.1,percentage:true},Coordinate{value:-0.1,percentage:true},Coordinate{value:1.2,percentage:true},Coordinate{value:1.2,percentage:true}]}
    #[test]
    fn svg_filter_program_keeps_nearest_preceding_identity_and_bounded_publication(){
        let mut builder=Builder::new();
        assert_eq!(builder.input(Some("later")),Input::SourceGraphic);
        builder.push(node(Primitive::Offset([1.0,2.0]),&[Input::SourceGraphic]),Some(Cow::Borrowed("shared")),8192).unwrap();
        assert_eq!(builder.input(Some("shared")),Input::Result(0));
        assert_eq!(builder.input(Some("later")),Input::Result(0));
        builder.push(node(Primitive::Offset([-3.0,4.0]),&[Input::Result(0)]),Some(Cow::Borrowed("shared")),8192).unwrap();
        assert_eq!(builder.input(Some("shared")),Input::Result(1));
        assert_eq!(builder.input(Some("SourceAlpha")),Input::SourceAlpha);
        let program=builder.finish(region(),Units::ObjectBoundingBox,Units::ObjectBoundingBox,8192).unwrap();
        assert!(program.valid(program.bytes()));assert!(!program.valid(program.bytes()-1));
        assert_eq!(program.nodes()[1].inputs.as_ref(),&[Input::Result(0)]);
        assert!(matches!(Builder::new().finish(region(),Units::ObjectBoundingBox,Units::UserSpaceOnUse,0),Err(BuildError::BudgetExceeded)));
        assert!(Builder::new().finish(region(),Units::ObjectBoundingBox,Units::UserSpaceOnUse,8192).unwrap().nodes().is_empty());
    }
    #[test]
    fn svg_transfer_and_composite_preserve_working_space_and_unpremultiplied_channels(){
        let table=Transfer::Table(Arc::from([1.0,0.5,0.0]));
        assert_eq!(table.sample(0.0),1.0);assert_eq!(table.sample(0.25),0.75);assert_eq!(table.sample(1.0),0.0);
        let discrete=Transfer::Discrete(Arc::from([0.0,0.25,0.75,1.0]));
        assert_eq!(discrete.sample(0.249),0.0);assert_eq!(discrete.sample(0.25),0.25);assert_eq!(discrete.sample(1.0),1.0);
        assert_eq!(Transfer::Table(Arc::from([])).sample(0.75),0.75);
        assert_eq!(Transfer::Gamma{amplitude:1.0,exponent:2.0,offset:0.0}.sample(0.5),0.25);
        let first=Color::new(ColorSpace::Srgb,[1.0,0.0,0.0],0.5,0);
        let second=Color::new(ColorSpace::Srgb,[0.0,0.0,1.0],0.5,0);
        let out=Primitive::Composite(Composite::Over).sample(first,second,ColorSpace::Srgb).unwrap();
        assert_eq!(out.alpha,0.75);assert_eq!(out.components,[2.0/3.0,0.0,1.0/3.0]);
        let identity=Primitive::Matrix(Arc::new(super::super::ColorMatrix::IDENTITY.coefficients()));
        let gray=Color::new(ColorSpace::Srgb,[0.5;3],0.25,0);
        let linear=identity.sample(gray,second,ColorSpace::SrgbLinear).unwrap();
        assert_eq!(linear.components,gray.to(ColorSpace::SrgbLinear).components);assert_eq!(linear.alpha,0.25);
        let functions=Primitive::ComponentTransfer(Arc::new([table,Transfer::Identity,Transfer::Identity,Transfer::Linear{slope:0.0,intercept:0.5}]));
        let out=functions.sample(first,second,ColorSpace::Srgb).unwrap();assert_eq!(out.components,[0.0,0.0,0.0]);assert_eq!(out.alpha,0.5);
    }
    #[test]
    fn svg_primitive_straight_alpha_storage_does_not_retain_invisible_color(){
        let invisible=Color::rgba8([255,0,0,0]);let black=Color::rgba8([0,0,0,0]);
        let flood=Primitive::Flood(invisible).sample(black,black,ColorSpace::Srgb).unwrap();
        assert_eq!(flood.components,[0.0;3]);assert_eq!(flood.alpha,0.0);
        let transfer=Primitive::ComponentTransfer(Arc::new([Transfer::Identity,Transfer::Identity,Transfer::Identity,Transfer::Linear{slope:0.0,intercept:1.0}]));
        assert_eq!(transfer.sample(invisible,black,ColorSpace::Srgb).unwrap().to_rgba8(),[0,0,0,255]);
        let mut matrix=super::super::ColorMatrix::IDENTITY.coefficients();matrix[3]=[1.0,0.0,0.0,0.0,0.0];
        let out=Primitive::Matrix(Arc::new(matrix)).sample(invisible,black,ColorSpace::Srgb).unwrap();
        assert_eq!(out.to_rgba8(),[0;4]);
    }

    #[test]
    fn svg_used_regions_keep_nonuniform_axes_and_generator_extent(){
        let mut builder=Builder::new();builder.push(node(Primitive::Flood(Color::rgba8([255,0,0,255])),&[]),None,8192).unwrap();
        let used=Use{url:Arc::from("#filter"),program:builder.finish(region(),Units::ObjectBoundingBox,Units::ObjectBoundingBox,8192).unwrap(),reference_box:[10.0,20.0,200.0,50.0],viewport:[0.0,0.0,500.0,300.0],owner_transform:[1.0,0.0,0.0,1.0,0.0,0.0],fill:PaintInput::None,stroke:PaintInput::None};
        assert_eq!(used.primitive_lengths([0.1,0.2]),Some([20.0,10.0]));
        assert_eq!(used.filter_region(),Some([-10.0,15.0,1.2f32*200.0,1.2f32*50.0]));
        assert_eq!(used.primitive_region(&used.program.nodes()[0],None),used.filter_region());
        let mut input=node(Primitive::Offset([0.0;2]),&[Input::Result(0)]);
        assert_eq!(used.primitive_region(&input,Some([20.0,30.0,10.0,10.0])),Some([20.0,30.0,10.0,10.0]));
        input.region[0]=Some(Coordinate{value:0.5,percentage:false});input.region[2]=Some(Coordinate{value:0.25,percentage:false});
        assert_eq!(used.primitive_region(&input,Some([20.0,30.0,10.0,10.0])),Some([110.0,30.0,50.0,10.0]));
    }
}
