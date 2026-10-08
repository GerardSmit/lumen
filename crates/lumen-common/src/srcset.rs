//! Bounded HTML responsive-image source sets over the maintained parser.
use alloc::{string::String,vec::Vec};

const MAX_SOURCE_BYTES:usize=65536;
const MAX_CANDIDATES:usize=256;

#[derive(Clone,Copy,Debug,Eq,PartialEq)]
pub enum Error { Capacity, InvalidEnvironment }

/// URL and density selected together; the density belongs to this request,
/// never to the resource cache or to the next pending request.
#[derive(Clone,Copy,Debug,PartialEq)]
pub struct Selected<'a> { pub url:&'a str, pub density:f64 }

pub struct SourceSet { candidates:Vec<parse_srcset::ImageCandidate> }
impl SourceSet {
    /// HTML source-set creation adds src as 1x only when neither a width
    /// descriptor nor an existing 1x candidate suppresses the default source.
    pub fn create(default_source:Option<&str>,srcset:&str)->Result<Self,Error> {
        if srcset.len()>MAX_SOURCE_BYTES || default_source.is_some_and(|value|value.len()>MAX_SOURCE_BYTES){return Err(Error::Capacity);}
        let mut candidates=parse_srcset::parse_srcset_bounded(srcset,MAX_CANDIDATES).ok_or(Error::Capacity)?;
        if candidates.len()>MAX_CANDIDATES{return Err(Error::Capacity);}
        if let Some(source)=default_source.filter(|source|!source.is_empty()) {
            if !candidates.iter().any(|value|value.width.is_some() || value.density.unwrap_or(1.0)==1.0) {
                if candidates.len()==MAX_CANDIDATES{return Err(Error::Capacity);}
                candidates.try_reserve(1).map_err(|_|Error::Capacity)?;
                candidates.push(parse_srcset::ImageCandidate{url:String::from(source),density:Some(1.0),width:None,height:None});
            }
        }
        Ok(Self{candidates})
    }
    pub fn is_empty(&self)->bool {self.candidates.is_empty()}
    /// Normalize width descriptors using the actually selected source size.
    /// The UA policy chooses the smallest density at least the actual DPR,
    /// otherwise the largest available density; equal densities keep source order.
    pub fn select(&self,source_size:f64,dpr:f64)->Result<Option<Selected<'_>>,Error> {
        if !source_size.is_finite() || source_size<0.0 || !dpr.is_finite() || dpr<=0.0{return Err(Error::InvalidEnvironment);}
        let mut above:Option<Selected<'_>>=None;let mut below:Option<Selected<'_>>=None;
        for candidate in &self.candidates {
            let density=candidate.density.unwrap_or_else(||candidate.width.map_or(1.0,|width|width as f64/source_size));
            let value=Selected{url:&candidate.url,density};
            if density>=dpr {
                if above.is_none_or(|prior|density<prior.density){above=Some(value);}
            }else if below.is_none_or(|prior|density>prior.density){below=Some(value);}
        }
        Ok(above.or(below))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn specification_srcset_preserves_url_commas_and_descriptor_presence() {
        let set=SourceSet::create(None,"data:image/png;base64,AAA 2x, low.png 1x").unwrap();
        assert_eq!(set.select(200.0,2.0).unwrap(),Some(Selected{url:"data:image/png;base64,AAA",density:2.0}));
        for raw in ["a.png 0x 1x","a.png 1h","a.png 1w 2x","a.png 0w","a.png 0h","a.png 1e999x"] {
            assert!(SourceSet::create(None,raw).unwrap().is_empty(),"{raw}");
        }
        assert_eq!(SourceSet::create(None,"zero.png 0x").unwrap().select(100.0,1.0).unwrap(),Some(Selected{url:"zero.png",density:0.0}));
        assert_eq!(SourceSet::create(None,"large.png 400w 200h").unwrap().select(200.0,1.0).unwrap(),Some(Selected{url:"large.png",density:2.0}));
    }
    #[test]
    fn specification_source_sets_use_actual_size_dpr_and_default_source_rules() {
        let set=SourceSet::create(Some("fallback.png"),"small.png 400w, large.png 800w").unwrap();
        assert_eq!(set.select(400.0,2.0).unwrap().unwrap().url,"large.png");
        assert_eq!(set.select(800.0,1.0).unwrap().unwrap().url,"large.png");
        assert_eq!(set.select(200.0,1.0).unwrap().unwrap().url,"small.png");
        let set=SourceSet::create(Some("fallback.png"),"large.png 2x").unwrap();
        assert_eq!(set.select(200.0,1.0).unwrap().unwrap().url,"fallback.png");
        let set=SourceSet::create(Some("fallback.png"),"first.png 1x, duplicate.png 1x").unwrap();
        assert_eq!(set.select(200.0,1.0).unwrap().unwrap().url,"first.png");
        assert_eq!(SourceSet::create(None,"a.png 1w").unwrap().select(0.0,1.0).unwrap().unwrap().density,f64::INFINITY);
        assert_eq!(set.select(200.0,0.0),Err(Error::InvalidEnvironment));
        assert!(matches!(SourceSet::create(None,&"x".repeat(MAX_SOURCE_BYTES+1)),Err(Error::Capacity)));
    }
    #[test]
    fn specification_bounded_srcset_scanner_recovers_without_descriptor_allocations() {
        let input="图片.png 20w 10h, data:image/png;base64,é 2x, bad 1w 2h 3x (a,b), 最後.png 3x";
        let set=SourceSet::create(None,input).unwrap();
        assert_eq!(set.select(10.0,2.0).unwrap().unwrap().url,"图片.png");
        assert_eq!(set.select(10.0,3.0).unwrap().unwrap().url,"最後.png");
        let data=SourceSet::create(None,"data:image/png;base64,é 2x").unwrap();
        assert_eq!(data.select(10.0,2.0).unwrap().unwrap().url,"data:image/png;base64,é");
        let mut many=String::from("bad ");
        for _ in 0..20_000 {many.push_str("1w ");}
        many.push_str("(hidden,comma), valid.png 0x");
        assert!(many.len()<MAX_SOURCE_BYTES);
        let set=SourceSet::create(None,&many).unwrap();
        assert_eq!(set.select(100.0,1.0).unwrap(),Some(Selected{url:"valid.png",density:0.0}));
        assert!(SourceSet::create(None,"bad 1w 2h (unterminated, next.png 2x").unwrap().is_empty());
    }

}
