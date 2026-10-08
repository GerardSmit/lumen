//! HTML image source selection shared by request and rendering owners.
use crate::{Document,Namespace,NodeId,NodeKind,css::{self,MediaEnvironment}};
use alloc::string::String;
use lumen_common::srcset::{Error,SourceSet};

#[derive(Clone,Debug,PartialEq)]
pub struct Selection {
    /// None distinguishes an omitted source from the explicit empty src error.
    pub source:Option<String>,
    pub density:f64,
    pub dimension_source:NodeId,
}
/// Presentation belongs to the committed request, not the decoded resource.
/// A pending selection cannot change the current image's density or SVG URL.
#[derive(Clone,Debug,PartialEq)]
pub struct ImageMetadata {
    pub intrinsic:Option<crate::object::IntrinsicSize>,
    pub density:f64,
    pub source:Option<alloc::sync::Arc<str>>,
}
impl Default for ImageMetadata {
    fn default()->Self {Self{intrinsic:None,density:1.0,source:None}}
}
impl ImageMetadata {
    pub fn is_valid(&self)->bool {self.intrinsic.is_none_or(crate::object::IntrinsicSize::is_valid)&&!self.density.is_nan()&&self.density>=0.0&&self.source.as_ref().is_none_or(|source|source.len()<=65536)}
}
fn html_element(document:&Document,node:NodeId,name:&str)->bool {
    matches!(document.kind(node),Ok(NodeKind::Element{namespace:Namespace::Html,name:actual,..})if actual==name)
}
fn attribute<'a>(document:&'a Document,node:NodeId,name:&str)->Option<&'a str> {document.get_attribute_ns_ref(node,None,name).ok().flatten()}

/// HTML auto-sizes eligibility uses the img attributes even when a preceding
/// source supplies its own sizes list; raw whitespace does not alter the flag.
pub fn allows_auto_sizes(document:&Document,node:NodeId)->bool {
    html_element(document,node,"img")
        && attribute(document,node,"loading").is_some_and(|value|value.eq_ignore_ascii_case("lazy"))
        && auto_sizes_attribute(attribute(document,node,"sizes").unwrap_or(""))
}
pub fn auto_sizes_attribute(raw:&str)->bool {raw.eq_ignore_ascii_case("auto")||raw.get(..5).is_some_and(|prefix|prefix.eq_ignore_ascii_case("auto,"))}

/// Source sets are evaluated in DOM sibling order, stopping at this img.
/// Source URLs remain relative until the existing request owner captures base.
/// auto_width comes from the rendering owner, never width-attribute guessing.
pub fn select(document:&Document,node:NodeId,environment:MediaEnvironment,auto_width:Option<f32>)->Result<Option<Selection>,Error> {
    select_for_device(document,node,environment,f64::from(environment.resolution),auto_width)
}
pub fn select_for_device(document:&Document,node:NodeId,environment:MediaEnvironment,dpr:f64,auto_width:Option<f32>)->Result<Option<Selection>,Error> {
    if !html_element(document,node,"img"){return Ok(None);}
    let auto_width=allows_auto_sizes(document,node).then_some(auto_width).flatten();
    if let Some(parent)=document.parent(node).ok().flatten().filter(|parent|html_element(document,*parent,"picture")) {
        let mut current=document.first_child(parent).ok().flatten();let mut remaining=document.node_count();
        while let Some(source)=current {
            if source==node{break;}
            if remaining==0{return Err(Error::Capacity);}remaining-=1;
            current=document.next_sibling(source).ok().flatten();
            if !html_element(document,source,"source"){continue;}
            let Some(srcset)=attribute(document,source,"srcset")else{continue;};
            let candidates=SourceSet::create(None,srcset)?;if candidates.is_empty(){continue;}
            if attribute(document,source,"media").is_some_and(|media|!css::media_query_matches(media,environment)){continue;}
            let source_size=css::image_source_size(attribute(document,source,"sizes").unwrap_or(""),environment,auto_width);
            if attribute(document,source,"type").is_some_and(|ty|!lumen_common::mime::image_type_supported(ty)){continue;}
            let dimension_source=if attribute(document,source,"width").is_some()||attribute(document,source,"height").is_some(){source}else{node};
            if let Some(selected)=candidates.select(f64::from(source_size),dpr)? {
                return Ok(Some(Selection{source:Some(String::from(selected.url)),density:selected.density,dimension_source}));
            }
        }
    }
    let source=attribute(document,node,"src");let srcset=attribute(document,node,"srcset").unwrap_or_default();
    let candidates=SourceSet::create(source,srcset)?;
    let source_size=css::image_source_size(attribute(document,node,"sizes").unwrap_or(""),environment,auto_width);
    let selected=candidates.select(f64::from(source_size),dpr)?;
    Ok(Some(match selected {
        Some(selected)=>Selection{source:Some(String::from(selected.url)),density:selected.density,dimension_source:node},
        None=>Selection{source:source.map(String::from),density:1.0,dimension_source:node},
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn specification_picture_selection_filters_actual_media_type_and_source_order() {
        let document=crate::html::parse("<picture><source srcset='wrong.png 1x' type='image/avif'><source srcset='small.png 400w, large.png 800w' sizes='(width > 30em) 400px, 100vw' media='(orientation: landscape)' type='image/png' width=50><img src=fallback.png><source srcset=after.png></picture>",64).unwrap();
        let img=crate::selector::query_selector(&document,document.root(),"img").unwrap().unwrap();
        let mut environment=MediaEnvironment{width:800.0,height:600.0,resolution:2.0,..MediaEnvironment::default()};
        let selected=select(&document,img,environment,None).unwrap().unwrap();
        assert_eq!(selected.source.as_deref(),Some("large.png"));assert_eq!(selected.density,2.0);assert_ne!(selected.dimension_source,img);
        environment.width=400.0;environment.height=800.0;
        assert_eq!(select(&document,img,environment,None).unwrap().unwrap().source.as_deref(),Some("fallback.png"));
        let document=crate::html::parse("<picture><source srcset='bad.png 0x 2x'><span></span><img src=fallback.png srcset='small.png 1x, large.png 2x'></picture>",64).unwrap();
        let img=crate::selector::query_selector(&document,document.root(),"img").unwrap().unwrap();
        assert_eq!(select(&document,img,environment,None).unwrap().unwrap().source.as_deref(),Some("large.png"));
    }
}
