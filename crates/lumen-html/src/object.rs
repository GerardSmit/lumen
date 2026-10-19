//! HTML object representation and ancestor eligibility shared by hosts/layout.
use crate::{Document, Error, Namespace, NodeId, NodeKind};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Representation {
    #[default]
    Fallback,
    Nothing,
    Image,
    Document,
}
impl Representation {
    pub fn is_replaced(self) -> bool {
        matches!(self, Self::Image | Self::Document)
    }
    pub fn has_child_navigable(self) -> bool {
        self == Self::Document
    }
}

pub fn is_object(document: &Document, node: NodeId) -> bool {
    matches!(document.kind(node), Ok(NodeKind::Element { namespace: Namespace::Html, name, .. })
        if name == "object")
}

/// HTML object ancestors use DOM ancestry (not flattened CSS box ancestry).
/// An image/document object suppresses its fallback subtree;
/// media elements suppress object descendants even when their media fails.
pub fn ancestor_excludes(
    document: &Document,
    node: NodeId,
    fallback: impl Fn(NodeId) -> bool,
) -> Result<bool, Error> {
    let mut parent = document.parent(node)?;
    for _ in 0..512 {
        let Some(node) = parent else { return Ok(false) };
        if let NodeKind::Element {
            namespace: Namespace::Html,
            name,
            ..
        } = document.kind(node)?
        {
            if name == "audio" || name == "video" || name == "object" && !fallback(node) {
                return Ok(true);
            }
        }
        parent = document.parent(node)?;
    }
    Err(Error::LimitExceeded)
}

/// Embedded resource kind is derived from the actual element namespace/name.
/// No fallback representation exists for an HTML embed element.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind { Object, Embed }
pub fn kind(document: &Document, node: NodeId) -> Option<Kind> {
    match document.kind(node).ok()? {
        NodeKind::Element { namespace: Namespace::Html, name, .. } if name == "object" => Some(Kind::Object),
        NodeKind::Element { namespace: Namespace::Html, name, .. } if name == "embed" => Some(Kind::Embed),
        _ => None,
    }
}
pub fn is_embedded(document: &Document, node: NodeId) -> bool { kind(document, node).is_some() }
impl Kind {
    pub fn inactive(self) -> Representation {
        match self { Self::Object => Representation::Fallback, Self::Embed => Representation::Nothing }
    }
    pub fn source_attribute(self) -> &'static str {
        match self { Self::Object => "data", Self::Embed => "src" }
    }
}

/// Natural dimensions of an actually committed embedded document. Percentages
/// and automatic SVG dimensions remain indeterminate, not viewport measurements.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct IntrinsicSize {
    pub width: Option<f32>,
    pub height: Option<f32>,
    pub ratio: Option<f32>,
}
impl IntrinsicSize {
    pub fn is_valid(self) -> bool {
        [self.width, self.height].into_iter().flatten().all(|v| v.is_finite() && v >= 0.0)
            && self.ratio.is_none_or(|v| v.is_finite() && v > 0.0)
    }
    /// CSS Images default object sizing, with the shared 300 by 150 default.
    pub fn default_dimensions(self) -> (f32, f32) {
        self.default_dimensions_in((300.0,150.0))
    }
    /// CSS Images default object sizing with the caller's default object size.
    /// Backgrounds use the positioning area; replaced objects use 300 by 150.
    pub fn default_dimensions_in(self,default:(f32,f32))->(f32,f32) {
        let (width,height)=self.dimensions_at_density((f64::from(default.0),f64::from(default.1)),1.0);
        (width as f32,height as f32)
    }
    /// HTML current pixel density scales actual natural dimensions. Missing
    /// dimensions still use default object sizing after density correction.
    pub fn dimensions_at_density(self,default:(f64,f64),density:f64)->(f64,f64) {
        let density=if density==0.0{0.0}else{density};
        let width=self.width.map(|value|f64::from(value)/density);
        let height=self.height.map(|value|f64::from(value)/density);
        let ratio=self.ratio.map(f64::from);
        match (width,height,ratio) {
            (Some(w),Some(h),_)=>(w,h),
            (Some(w),None,Some(r))=>(w,w/r),
            (None,Some(h),Some(r))=>(h*r,h),
            (Some(w),None,None)=>(w,default.1),
            (None,Some(h),None)=>(default.0,h),
            (None,None,Some(r))=>{let w=default.0.min(default.1*r);(w,w/r)},
            (None,None,None)=>default,
        }
    }
    /// The HTML parser admits zero density (infinite natural dimensions).
    /// Rendering uses the existing finite f32 coordinate representation limit,
    /// uniformly limiting known dimensions so their natural ratio is preserved.
    /// Request metadata retains the exact density for the natural-size getters.
    pub fn density_corrected(self,density:f64)->Option<Self> {
        if !self.is_valid()||density.is_nan()||density<0.0{return None;}
        let density=if density==0.0{0.0}else{density};
        let largest=self.width.into_iter().chain(self.height).map(f64::from).fold(0.0,f64::max);
        let scale=if largest>0.0{(1.0/density).min(f64::from(f32::MAX)/largest)}else{0.0};
        let result=Self{width:self.width.map(|value|(f64::from(value)*scale) as f32),height:self.height.map(|value|(f64::from(value)*scale) as f32),ratio:self.ratio};
        result.is_valid().then_some(result)
    }
}
