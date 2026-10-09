//! Owner-qualified CSS and native progress timelines share this DOM/layout authority.
use super::ProgressRange;
use crate::{NodeId,layout::LayoutError};
use alloc::{format,string::String,sync::Arc};

#[derive(Clone)]
pub struct ProgressBinding {
    pub sampled_time: Option<f64>,
    pub source: Option<NodeId>,
    pub subject: Option<NodeId>,
    pub horizontal: bool,
    pub range: ProgressRange,
    pub insets: Option<[crate::css::DecorationLength;2]>,
    pub duration_auto:bool,
}

impl ProgressBinding{
    pub fn checked_retained_bytes(&self)->Option<usize>{
        self.range.checked_retained_bytes()?.checked_add(match &self.insets{Some(values)=>crate::css::checked_progress_offset_bytes(values)?,None=>0})
    }
}

#[derive(Clone)]
pub(crate) struct TransformTimelineValue {
    pub node:NodeId,
    pub pseudo:Option<crate::css::PseudoElement>,
    pub source:Arc<str>,
    pub used:Arc<str>,
}

/// One sparse owner-qualified source operation, shared by final paint geometry
/// and resolved CSSOM. Ordinary transforms allocate no map provenance.
#[derive(Default)]
pub(crate) struct TransformTimelineValues {
    pub values:alloc::collections::BTreeMap<(u128,u8),TransformTimelineValue>,
    retained_bytes:usize,
}
impl TransformTimelineValues {
    fn key(node:NodeId,pseudo:Option<crate::css::PseudoElement>)->(u128,u8){
        (node.key(),pseudo.map_or(0,|pseudo|pseudo as u8+1))
    }
    pub fn get_value(&self,node:NodeId,pseudo:Option<crate::css::PseudoElement>,source:&str)->Option<&TransformTimelineValue>{
        let value=self.values.get(&Self::key(node,pseudo))?;
        (value.source.as_ref()==source).then_some(value)
    }
    pub fn checked_retained_bytes(&self)->Option<usize>{
        self.retained_bytes.checked_add(core::mem::size_of::<Self>())
    }
    pub fn insert(&mut self,value:TransformTimelineValue)->Result<(),LayoutError>{
        const LIMIT:usize=16*1024*1024;
        // As with RetainedLayoutCache's B-tree index, conservatively charge
        // one allocator/node allowance per entry, including unused slots.
        const ENTRY:usize=6*(core::mem::size_of::<(u128,u8)>()+core::mem::size_of::<TransformTimelineValue>()+3*core::mem::size_of::<usize>());
        let charge=ENTRY.checked_add(core::mem::size_of::<usize>()*4)
            .and_then(|header|header.checked_add(value.source.len()))
            .and_then(|bytes|bytes.checked_add(value.used.len())).ok_or(LayoutError::CommandLimit)?;
        let key=Self::key(value.node,value.pseudo);
        let old_charge=self.values.get(&key).map_or(0,|old|ENTRY+4*core::mem::size_of::<usize>()+old.source.len()+old.used.len());
        if old_charge==0&&self.values.len()>=16*1024{return Err(LayoutError::CommandLimit);}
        let bytes=self.retained_bytes.checked_sub(old_charge).and_then(|bytes|bytes.checked_add(charge)).ok_or(LayoutError::CommandLimit)?;
        if bytes.checked_add(core::mem::size_of::<Self>()).is_none_or(|bytes|bytes>LIMIT){return Err(LayoutError::CommandLimit);}
        self.values.insert(key,value);self.retained_bytes=bytes;Ok(())
    }
}

/// Both proportional progress and actual scroll coordinates remain available
/// to consumers; absolute interpolation maps must not invent a pixel extent.
#[derive(Clone,Copy,Debug,PartialEq)]
pub struct ProgressTimelineSample {pub progress:f64,pub position:f64,pub start:f64,pub end:f64}

fn list_value(style:&[Option<Arc<str>>;19],property:usize,index:usize,default:&str)->String {
    let values=style[property].as_deref().map(crate::css::css_list_items).unwrap_or_default();
    values.get(index%values.len().max(1)).cloned().unwrap_or_else(||String::from(default))
}

pub fn axis_horizontal(session:&mut crate::session::RenderSession,node:NodeId,axis:&str)->Result<bool, LayoutError>{
    let node=if node==session.document().root(){session.document().document_element_at(node).map_err(|_|LayoutError::InvalidTree)?.unwrap_or(node)}else{node};
    let writing=session.computed_style(node)?.writing_mode;
    Ok(match axis{"x"=>true,"y"=>false,"inline"=>writing==crate::css::WritingMode::HorizontalTb,_=>writing!=crate::css::WritingMode::HorizontalTb})
}
pub fn nearest_scroll_source(session:&mut crate::session::RenderSession,node:NodeId,axis:&str)->Result<(Option<NodeId>,bool), LayoutError>{
    let mut ancestor=session.document().composed_parent(node).map_err(|_|LayoutError::InvalidTree)?;
    while let Some(id)=ancestor{
        let horizontal=axis_horizontal(session,id,axis)?;
        // The Document is the viewport scroll source, not an element with a
        // computed overflow declaration. Its axis was resolved via its root
        // element above; do not ask the element cascade for Document styles.
        if id==session.document().root(){return Ok((Some(id),horizontal));}

        let style=session.computed_style(id)?;
        let overflow=if horizontal{style.overflow_x}else{style.overflow_y};
        use crate::css::Display as D;
        if !matches!(style.display,D::Inline|D::Contents|D::None|D::TableRow|D::TableRowGroup|D::TableHeaderGroup|D::TableFooterGroup|D::TableColumn|D::TableColumnGroup)
            &&matches!(overflow,crate::css::Overflow::Hidden|crate::css::Overflow::Scroll|crate::css::Overflow::Auto){return Ok((Some(id),horizontal));}
        ancestor=session.document().composed_parent(id).map_err(|_|LayoutError::InvalidTree)?;
    }
    let source=session.document().root();let horizontal=axis_horizontal(session,source,axis)?;
    Ok((Some(source),horizontal))
}

pub fn resolve(
    session: &mut crate::session::RenderSession, snapshot: &crate::session::AnimationSnapshot,
    node: NodeId, scope: Option<NodeId>, name: &str, range: ProgressRange,
) -> Result<ProgressBinding, LayoutError> {
    if let Some(timeline)=crate::css::parse_anonymous_progress_timeline(name){
        let(source,horizontal)=if timeline.scroller=="root"{
            let source=session.document().root();(Some(source),axis_horizontal(session,source,&timeline.axis)?)
        }else if timeline.scroller=="self"&&!timeline.view{(Some(node),axis_horizontal(session,node,&timeline.axis)?)}else{nearest_scroll_source(session,node,&timeline.axis)?};

        return Ok(ProgressBinding{sampled_time:None,source,subject:timeline.view.then_some(node),horizontal,range,insets:timeline.view.then_some(timeline.insets),duration_auto:true});
    }
    let mut current=Some(node);
    while let Some(candidate)=current {
        if let Some((_,_,_,style))=snapshot.nodes.iter().find(|(id,pseudo,tree,_)| *id==candidate && pseudo.is_none() && *tree==scope) {
            for (names_slot,axis_slot,view) in [(11,12,false),(13,14,true)] {
                let names=style[names_slot].as_deref().map(crate::css::css_list_items).unwrap_or_default();
                if let Some(index)=names.iter().rposition(|candidate|candidate==name) {
                    let axis=list_value(style,axis_slot,index,"block");
                    let(source,horizontal)=if view{nearest_scroll_source(session,candidate,&axis)?}else{(Some(candidate),axis_horizontal(session,candidate,&axis)?)};
                    let insets=if view{
                        let value=list_value(style,18,index,"auto");
                        crate::css::parse_anonymous_progress_timeline(&format!("view({value})")).map(|timeline|timeline.insets)
                    }else{None};
                    return Ok(ProgressBinding{sampled_time:None,source,subject:view.then_some(candidate),horizontal,range,insets,duration_auto:true});

                }
            }
        }
        current=session.document().composed_parent(candidate).map_err(|_|LayoutError::InvalidTree)?;
    }
    Ok(ProgressBinding{sampled_time:None,source:None,subject:None,horizontal:false,range,insets:None,duration_auto:true})
}

pub fn sample(session:&mut crate::session::RenderSession,binding:&ProgressBinding)->Option<ProgressTimelineSample> {
    let source=binding.source?;
    let style_node=if source==session.document().root(){session.document().document_element_at(source).ok().flatten().unwrap_or(source)}else{source};
    let sides=session.computed_style(style_node).ok()?.logical_sides();
    let offset=session.scroll_offset(source);
    sample_completed_geometry(binding,sides,session.completed_geometry()?,offset)
}

/// Sample a completed, owner-qualified layout operation. The caller supplies
/// the style and scroll offsets from that operation's same source epoch.
/// This does not create a layout, force a stale timeline update or assume a
/// missing box is a zero-sized scrollport.
pub(crate) fn sample_completed_geometry(binding:&ProgressBinding,sides:[usize;4],
    geometry:&crate::layout::LayoutGeometry,(x,y):(f32,f32))->Option<ProgressTimelineSample>{
    let source=binding.source?;
    let start_edge=[sides[0],sides[2]].into_iter().find(|edge|if binding.horizontal{matches!(*edge,1|3)}else{matches!(*edge,0|2)})?;
    let reverse=if binding.horizontal{start_edge==1}else{start_edge==2};

    let offset=f64::from(if binding.horizontal{x}else{y})*if reverse{-1.0}else{1.0};
    let (start,end)=if let Some(subject)=binding.subject {
        let rect=geometry.effect_box(subject,None)?.0;
        let (port,_)=geometry.scrollport_coordinate_space(source)?;
        let mut subject_start=if binding.horizontal{rect.x-port.x+x}else{rect.y-port.y+y} as f64;
        let subject_size=if binding.horizontal{rect.width}else{rect.height} as f64;
        let viewport_size=if binding.horizontal{port.width}else{port.height} as f64;
        if reverse{subject_start=viewport_size-subject_start-subject_size;}
                let offsets=binding.insets.as_ref().map(|insets|[f64::from(insets[0].used(viewport_size as f32).unwrap_or(0.0)),f64::from(insets[1].used(viewport_size as f32).unwrap_or(0.0))]).unwrap_or([0.0;2]);
        binding.range.view_bounds(subject_start-offsets[0],subject_size,viewport_size-offsets[0]-offsets[1])?

    }else{
        let (min_x,max_x,min_y,max_y)=geometry.scroll_bounds(source)?;
        let(start,end)=if binding.horizontal{(min_x as f64,max_x as f64)}else{(min_y as f64,max_y as f64)};let(start,end)=if reverse{(-end,-start)}else{(start,end)};binding.range.scroll_bounds(start,end)?
    };
    super::progress_fraction(offset,start,end).map(|progress|ProgressTimelineSample{progress,position:offset,start,end})
}
