//! Fragmentation controls use one keyword codec and sparse computed carrier.
use super::*;
#[derive(Clone,Copy,Debug,Default,PartialEq,Eq)]
#[repr(u8)]
pub enum BreakControl {
    #[default] Auto,Avoid,Always,All,AvoidPage,Page,Left,Right,Recto,Verso,
    AvoidColumn,Column,AvoidRegion,Region,
}
impl BreakControl {
    pub(crate) fn keyword(self)->&'static str {match self {
        Self::Auto=>"auto",Self::Avoid=>"avoid",Self::Always=>"always",Self::All=>"all",
        Self::AvoidPage=>"avoid-page",Self::Page=>"page",Self::Left=>"left",Self::Right=>"right",
        Self::Recto=>"recto",Self::Verso=>"verso",Self::AvoidColumn=>"avoid-column",Self::Column=>"column",
        Self::AvoidRegion=>"avoid-region",Self::Region=>"region",
    }}
    pub(crate) fn forces_column(self)->bool {matches!(self,Self::Column|Self::Always|Self::All)}
    pub(crate) fn avoids_column(self)->bool {matches!(self,Self::Avoid|Self::AvoidColumn)}
}
pub(crate) fn slot(name:&str)->Option<usize> {Some(match name {
    "break-before"|"page-break-before"=>247,"break-after"|"page-break-after"=>248,
    "break-inside"|"page-break-inside"=>249,_=>return None,
})}
pub(crate) fn parse(name:&str,raw:&str)->Option<BreakControl> {
    let inside=slot(name)?==249;let legacy=name.starts_with("page-");
    if legacy {
        return [BreakControl::Auto,BreakControl::Avoid,BreakControl::Left,BreakControl::Right,BreakControl::Page]
            .into_iter().filter(|value|!inside || matches!(value,BreakControl::Auto|BreakControl::Avoid))
            .find(|value|decoded_css_keyword(raw,if *value==BreakControl::Page{"always"}else{value.keyword()}));
    }
    [BreakControl::Auto,BreakControl::Avoid,BreakControl::Always,BreakControl::All,BreakControl::AvoidPage,
        BreakControl::Page,BreakControl::Left,BreakControl::Right,BreakControl::Recto,BreakControl::Verso,
        BreakControl::AvoidColumn,BreakControl::Column,BreakControl::AvoidRegion,BreakControl::Region]
        .into_iter().filter(|value|!inside || matches!(value,BreakControl::Auto|BreakControl::Avoid|BreakControl::AvoidPage|BreakControl::AvoidColumn|BreakControl::AvoidRegion))
        .find(|value|decoded_css_keyword(raw,value.keyword()))
}
pub(crate) fn serialize(name:&str,value:BreakControl)->Option<&'static str> {
    if !name.starts_with("page-"){return Some(value.keyword());}
    Some(match value {
        BreakControl::Auto=>"auto",BreakControl::Avoid=>"avoid",BreakControl::Left if slot(name)!=Some(249)=>"left",
        BreakControl::Right if slot(name)!=Some(249)=>"right",BreakControl::Page if slot(name)!=Some(249)=>"always",
        _=>return None,
    })
}
pub(crate) fn specified(name:&str,raw:&str)->Option<String> {
    if let Some(keyword)=["initial","inherit","unset","revert","revert-layer"].into_iter()
        .find(|keyword|decoded_css_keyword(raw,keyword)){return Some(keyword.into());}
    Some(serialize(name,parse(name,raw)?)?.into())
}
