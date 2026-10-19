//! Multicol declarations retain the shared numeric AST until their used inline
//! basis is known. Source parsing, query freezing, CSSOM and layout share it.
use super::*;
#[derive(Clone,Debug,Default,PartialEq)]
pub enum ColumnWidth {
    #[default] Auto,
    Length(Arc<typed_numeric::NumericExpression>),
    Intrinsic(IntrinsicSizing),
    Stretch,
    FitContent(Arc<typed_numeric::NumericExpression>),
}
impl ColumnWidth {
    pub(crate) fn serialize(&self)->Option<String> {Some(match self {
        Self::Auto=>"auto".into(),Self::Length(value)=>value.serialize()?,
        Self::Intrinsic(value)=>value.as_str().into(),Self::Stretch=>"stretch".into(),
        Self::FitContent(value)=>alloc::format!("fit-content({})",value.serialize()?),
    })}
    pub(crate) fn checked_retained_bytes(&self)->Option<usize> {match self {
        Self::Length(value)|Self::FitContent(value)=>core::mem::size_of::<usize>().checked_mul(2)?.checked_add(core::mem::size_of::<typed_numeric::NumericExpression>())?.checked_add(value.checked_retained_bytes()?),
        _=>Some(0),
    }}
    fn numeric(&self,basis:Option<f32>)->Option<f32> {
        let value=match self {Self::Length(value)|Self::FitContent(value)=>value,_=>return None};
        // Match the shared length-percentage evaluator's indefinite-basis rule.
        if basis.is_none() && value.contains_unit(|unit|unit==typed_numeric::NumericUnit::Percent){return None;}
        let result=value.evaluate(&mut FontAngleContext{length:None,query:ContainerUnitContext::default(),percent_scale:basis.map_or(0.0,|basis|f64::from(basis)/100.0)})?;
        Some(typed_numeric::computed_f32(result).max(0.0))
    }
    /// Optimal width precedes the canonical used-count algorithm; intrinsic
    /// keywords consume measured content contributions, never tag guesses.
    pub(crate) fn optimal(&self,available:Option<f32>,minimum:f32,maximum:f32)->Option<f32> {match self {
        Self::Auto=>None,Self::Length(_)=>self.numeric(available),
        Self::Stretch=>available,Self::Intrinsic(IntrinsicSizing::MinContent)=>Some(minimum),
        Self::Intrinsic(IntrinsicSizing::MaxContent)=>Some(maximum),
        Self::Intrinsic(IntrinsicSizing::FitContent)=>Some(minimum.max(available.unwrap_or(maximum)).min(maximum)),
        Self::FitContent(_)=>Some(minimum.max(self.numeric(available).unwrap_or(maximum)).min(maximum)),
    }}
}
fn expression(raw:&str,computed:Option<(LengthContext,ContainerUnitContext)>)->Option<Arc<typed_numeric::NumericExpression>> {
    use typed_numeric::{NumericUnit as Unit,NumericType,NumericValue};
    let mut value=typed_numeric::parse_numeric_expression(raw)?;
    if let Some(leaf)=value.single_numeric_value() {
        if leaf.unit==Unit::Number && leaf.value==0.0 && !math_function(raw) {value=typed_numeric::NumericExpression::Value(NumericValue{unit:Unit::Px,value:0.0});}
        else if leaf.value<0.0 && !math_function(raw){return None;}
    }
    let compatible=value.numeric_type()?.add(NumericType::from_unit(Unit::Px))?;
    if compatible.length!=1||compatible.angle!=0||compatible.time!=0||compatible.frequency!=0||compatible.resolution!=0||compatible.flex!=0||compatible.percent!=0{return None;}
    if let Some((context,query))=computed {
        value.simplify_absolute_units();
        value.map_numeric_values(|leaf|typed_numeric::computed_numeric_value(leaf,context,query))?;
        value.simplify_absolute_units();
        // A fully computed primitive has no remaining used percentage basis.
        // Remove its authored calc() boundary and apply the property's range
        // once, without clamping intermediate leaves in mixed calculations.
        if let Some(mut leaf)=value.single_numeric_value() {
            leaf.value=if leaf.value.is_finite(){leaf.value.clamp(0.0,f64::from(f32::MAX))}
                else{f64::from(typed_numeric::computed_f32(leaf.value)).max(0.0)};
            value=typed_numeric::NumericExpression::Value(leaf);
        }
    }
    Some(Arc::new(value))
}
fn width(raw:&str,computed:Option<(LengthContext,ContainerUnitContext)>)->Option<ColumnWidth> {
    if decoded_css_keyword(raw,"auto"){return Some(ColumnWidth::Auto);}
    if decoded_css_keyword(raw,"stretch"){return Some(ColumnWidth::Stretch);}
    if let Some(value)=intrinsic_sizing(raw){return Some(ColumnWidth::Intrinsic(value));}
    if let Some((_,body))=generated_content_function(raw).filter(|(name,_)|decoded_css_keyword(name,"fit-content")){return expression(body,computed).map(ColumnWidth::FitContent);}
    expression(raw,computed).map(ColumnWidth::Length)
}
fn width_declaration(raw:&str)->Option<Value> {
    let parsed=width(raw,None)?;
    Some(if matches!(parsed,ColumnWidth::Auto|ColumnWidth::Intrinsic(_)|ColumnWidth::Stretch){Value::ColumnWidth(parsed)}
        else{Value::ContextLength(238,raw.into(),true)})
}
pub(super) fn resolve(raw:&str,context:LengthContext,query:ContainerUnitContext)->Option<Value> {width(raw,Some((context,query))).map(Value::ColumnWidth)}
pub(super) fn declarations(name:&str,raw:&str,important:bool)->Option<Vec<Declaration>> {
    if name=="column-width"{return Some(alloc::vec![Declaration{value:width_declaration(raw)?,important}]);}
    let (width,count)=parts(raw)?;
    Some(alloc::vec![Declaration{value:width_declaration(width.unwrap_or("auto"))?,important},Declaration{value:column_count_value(count.unwrap_or("auto"))?,important}])
}
fn parts(raw:&str)->Option<(Option<&str>,Option<&str>)> {
    let tokens=components(raw).filter(|tokens|(1..=2).contains(&tokens.len()))?;
    let mut w=None;let mut c=None;let mut autos=0;
    for token in tokens {if decoded_css_keyword(token,"auto"){autos+=1;continue;}
        if c.is_none() && column_count_value(token).is_some(){c=Some(token);}
        else if w.is_none() && width(token,None).is_some(){w=Some(token);}
        else{return None;}
    }
    (autos+usize::from(w.is_some())+usize::from(c.is_some())<=2).then_some((w,c))
}
pub(super) fn numeric_input(raw:&str)->&str {generated_content_function(raw).filter(|(name,_)|decoded_css_keyword(name,"fit-content")).map_or(raw,|(_,body)|body)}
fn source_width(raw:&str)->Option<String> {Some(match width(raw,None)? {
    ColumnWidth::Length(value)=>(*value).clone().serialize_specified()?,
    ColumnWidth::FitContent(value)=>alloc::format!("fit-content({})",(*value).clone().serialize_specified()?),
    value=>value.serialize()?,
})}
pub(super) fn shorthand_value(width:&str,count:&str)->String {
    if width=="auto" {count.into()}else if count=="auto"{width.into()}else{alloc::format!("{width} {count}")}
}
pub(super) fn specified(name:&str,raw:&str)->Option<String> {
    if let Some(keyword)=["initial","inherit","unset","revert","revert-layer"].into_iter().find(|keyword|decoded_css_keyword(raw,keyword)){return Some(keyword.into());}
    if name=="column-width"{return source_width(raw);}
    if name=="column-count"{return count_source(raw);}
    let (w,c)=parts(raw)?;
    Some(shorthand_value(&source_width(w.unwrap_or("auto"))?,&count_source(c.unwrap_or("auto"))?))
}
fn count_source(raw:&str)->Option<String> {
    if decoded_css_keyword(raw,"auto"){Some("auto".into())}
    else {column_count_value(raw)?;typed_numeric::parse_numeric_expression(raw)?.serialize_specified()}
}
pub(super) fn specified_component(origin:&str,target:&str,raw:&str)->Option<String> {
    if origin!="columns"{return specified(target,raw);}
    let (w,c)=parts(raw)?;
    if target=="column-width"{source_width(w.unwrap_or("auto"))}else{count_source(c.unwrap_or("auto"))}
}
/// CSS Multicol §3.4. Cap only the renderer's used work, preserving valid
/// computed declarations and the source-selected explicit count.
pub(crate) fn used(style:&Style,available:f32,minimum:f32,maximum:f32,gap:f32)->(usize,f32) {
    let gap=gap.max(0.0);
    let n=match style.column_width.optimal(Some(available),minimum,maximum) {
        Some(optimal)=>{let fit=((available+gap)/(optimal.max(1.0)+gap)).floor().max(1.0) as usize;style.column_count.map_or(fit,|count|count.min(fit))},
        None=>style.column_count.unwrap_or(1),
    }.clamp(1,64);
    (n,((available+gap)/n as f32-gap).max(0.0))
}

/// Multicol applies to block containers, including flow list items and table
/// cells/captions. Table wrappers, flex and grid keep their own layout models.
pub(crate) fn active(style:&Style)->bool {
    matches!(style.display,Display::Block|Display::FlowRoot|Display::InlineBlock|Display::ListItem|Display::TableCell|Display::TableCaption)
        && (style.column_count.is_some() || !matches!(style.column_width,ColumnWidth::Auto))
}
pub(crate) fn clear(style:&mut Style) {
    style.column_count=None;
    if !matches!(style.column_width,ColumnWidth::Auto){style.column_width=ColumnWidth::Auto;}
}
pub(crate) fn intrinsic_inline(style:&Style,minimum:f32,maximum:f32,is_minimum:bool)->f32 {
    let count=style.column_count.unwrap_or(1).max(1) as f32;
    let gap=style.column_gap.unwrap_or(style.font_size).max(0.0);
    let width=style.column_width.optimal(None,minimum,maximum);
    let column=if is_minimum {width.map_or(minimum,|width|minimum.min(width))}
        else {width.map_or(maximum,|width|maximum.max(width))};
    typed_numeric::computed_f32(f64::from(column)*f64::from(count)+f64::from(gap)*f64::from(count-1.0))
}

/// Sparse per-measured-line planning state. Text, styles, glyphs and hits stay
/// with their original source owners; only numeric source boundaries live here.
#[derive(Clone,Copy)]
pub(crate) struct LineFragment {
    pub height:f32,
    pub fragment_start:bool,
    pub source_cut_allowed:bool,
    pub continuous:bool,
    break_flags:u8,
    previous:usize,
    fragments:usize,
}
impl LineFragment {
    pub(crate) fn new(height:f32)->Self {Self{height,fragment_start:false,source_cut_allowed:true,continuous:false,break_flags:0,previous:0,fragments:usize::MAX}}
    pub(crate) fn forced_start(&self)->bool {self.break_flags&1!=0}
    pub(crate) fn set_forced_start(&mut self){self.break_flags|=1;}
    pub(crate) fn avoids_end(&self)->bool {self.break_flags&2!=0}
    pub(crate) fn set_avoid_end(&mut self){self.break_flags|=2;}
    pub(crate) fn avoids_inside(&self)->bool {self.break_flags&4!=0}
    pub(crate) fn set_avoid_inside(&mut self){self.break_flags|=4;}
    pub(crate) fn relax_avoid(&mut self){self.break_flags&=1;}

}
/// CSS Break 3 §3.3/4.4. Prefix reachability chooses the fewest fragments
/// obeying source-boundary constraints: Rule 3 counts lines from a cut to the
/// enclosing block's start/end, not from one cut to the next. A monotonic
/// source-index queue yields linear work, including nonuniform line heights.
/// The supplied scratch contains only indices and is reused by balancing and
/// the final plan; no line payload or height scratch array is copied.
pub(crate) fn plan_line_fragments<T>(lines:&mut[T],project:fn(&mut T)->&mut LineFragment,
    capacity:f32,orphans:usize,widows:usize,scratch:&mut Vec<usize>,publish:bool)->Option<usize> {
    if !capacity.is_finite() || capacity<0.0{return None;}
    scratch.clear();
    if scratch.capacity()<lines.len(){scratch.try_reserve_exact(lines.len()).ok()?;}
    let mut head=0;let mut admitted=1;let mut lower=0;let mut consumed=0.0f64;
    let total=lines.len();
    for end in 1..=total {
        if end>1 && project(&mut lines[end-1]).forced_start(){lower=end-1;consumed=0.0;}
        let height=project(&mut lines[end-1]).height;
        if !height.is_finite() || height<0.0{return None;}
        consumed+=f64::from(height);
        while lower<end && consumed>f64::from(capacity) {
            consumed-=f64::from(project(&mut lines[lower]).height);lower+=1;
        }
        // A monolithic oversized line still makes progress in its own column.
        let first=lower.min(end-1);
        let upper=end-1;
        while admitted<=upper {
            let count=project(&mut lines[admitted-1]).fragments;
            if count!=usize::MAX {
                while scratch.len()>head && project(&mut lines[*scratch.last()? - 1]).fragments>=count {scratch.pop();}
                scratch.push(admitted);
            }
            admitted+=1;
        }
        while head<scratch.len() && scratch[head]<first {head+=1;}
        let forced_next=end<total && project(&mut lines[end]).forced_start();
        let allowed=end==total || forced_next || project(&mut lines[end-1]).source_cut_allowed
            && !project(&mut lines[end-1]).avoids_end()
            && end>=orphans.max(1) && total-end>=widows.max(1);
        let previous=if !allowed {None}else if first==0 {Some(0)}else{scratch.get(head).copied()};
        let count=previous.map_or(usize::MAX,|index|if index==0{1}else{project(&mut lines[index-1]).fragments+1});
        let state=project(&mut lines[end-1]);state.previous=previous.unwrap_or(0);state.fragments=count;
        if publish {state.fragment_start=false;}
    }
    let count=if total==0 {0}else{project(&mut lines[total-1]).fragments};
    if count==usize::MAX{return None;}
    if publish {
        let mut end=total;
        while end>0 {
            let previous=project(&mut lines[end-1]).previous;
            if previous>0 {project(&mut lines[previous]).fragment_start=true;}
            end=previous;
        }
    }
    Some(count)
}
/// Balanced height uses precisely the same source reachability as publication.
/// Search representable finite heights without fixture-specific tolerances.
pub(crate) fn balanced_line_height<T>(lines:&mut[T],project:fn(&mut T)->&mut LineFragment,
    columns:usize,orphans:usize,widows:usize,scratch:&mut Vec<usize>)->Option<f32> {
    if columns==0{return None;}
    let mut low=0.0f32;let mut total=0.0f64;
    for line in lines.iter_mut() {let height=project(line).height;if !height.is_finite() || height<0.0{return None;}low=low.max(height);total+=f64::from(height);}
    let mut high=typed_numeric::computed_f32(total);
    while low<high {
        let middle=typed_numeric::computed_f32((f64::from(low)+f64::from(high))*0.5);
        if middle>=high {break;}
        let fits=plan_line_fragments(lines,project,middle,orphans,widows,scratch,false).is_some_and(|used|used<=columns);
        if fits {high=middle;}else{low=middle.next_up();}
    }
    Some(high)
}

/// A source position identifies either a real item boundary or progress within
/// a measured continuous block area. It never manufactures line boxes.
#[derive(Clone,Copy,Debug,PartialEq)]
pub(crate) struct SourceCut {pub index:usize,pub offset:f32}

/// Reachability for a source containing continuous areas. Allowed boundaries
/// are absolute source constraints, so the farthest reachable allowed cut
/// minimizes the number of columns. Atomic items are revisited only across the
/// immediately preceding cut; continuous work is bounded by actual fragments.
/// A forbidden source segment fails as a whole.
pub(crate) fn plan_continuous_fragments<T>(items:&mut[T],project:fn(&mut T)->&mut LineFragment,
    capacity:f32,column_block_size:f32,orphans:usize,widows:usize,cuts:&mut Vec<SourceCut>,limit:usize)->Option<usize> {
    if !capacity.is_finite() || capacity<0.0 || !column_block_size.is_finite() || column_block_size<0.0{return None;}
    let capacity=f64::from(capacity.max(1.0));
    let column_block_size=f64::from(column_block_size.max(1.0));cuts.clear();
    let total=items.len();let mut start=SourceCut{index:0,offset:0.0};
    while start.index<total {
        let mut cursor=start;let mut used=0.0f64;let mut last=None;
        while cursor.index<total {
            if cursor.offset==0.0 && cursor!=start && project(&mut items[cursor.index]).forced_start(){
                last=Some(cursor);break;
            }
            let source=project(&mut items[cursor.index]);let height=f64::from(source.height);
            if !height.is_finite() || height<0.0 || f64::from(cursor.offset)>height{return None;}
            let remaining=height-f64::from(cursor.offset);let room=(capacity-used).max(0.0);
            if source.continuous && !source.avoids_inside() && remaining>room {
                if room>0.0 {
                    let target=f64::from(cursor.offset)+room+(column_block_size-capacity).max(0.0);
                    let mut offset=typed_numeric::computed_f32(target);
                    if f64::from(offset)>target {offset=offset.next_down();}
                    if offset<=cursor.offset{return None;}
                    if f64::from(offset)>=height {
                        let index=cursor.index+1;
                        let forbidden=!source.source_cut_allowed || source.avoids_end();
                        if index<total && forbidden && !project(&mut items[index]).forced_start(){return None;}
                        // The area finishes before the selected cut. Its own
                        // specified size stays complete; only still-open
                        // ancestors fill the remaining column extent.
                        last=Some(SourceCut{index,offset:0.0});
                    }else{last=Some(SourceCut{index:cursor.index,offset});}
                }
                break;
            }
            if remaining>room && used>0.0 {break;}
            if source.continuous && source.avoids_inside() && remaining>capacity{return None;}
            let allows_end=source.source_cut_allowed && !source.avoids_end();
            used+=remaining;cursor=SourceCut{index:cursor.index+1,offset:0.0};
            let forced_next=cursor.index<total && project(&mut items[cursor.index]).forced_start();
            let allowed=cursor.index==total || forced_next || allows_end
                && cursor.index>=orphans.max(1) && total-cursor.index>=widows.max(1);
            if allowed {last=Some(cursor);}
            // Oversized atomic sources make progress whole. A following
            // zero-sized box belongs to the next column after overflow.
            if used>capacity {break;}
        }
        let end=last?;
        if end.index<start.index || end.index==start.index && end.offset<=start.offset{return None;}
        if cuts.len()>=limit{return None;}
        cuts.try_reserve(1).ok()?;cuts.push(end);start=end;
    }
    Some(cuts.len())
}
pub(crate) fn balanced_continuous_height<T,F:Fn(f32)->f32>(items:&mut[T],project:fn(&mut T)->&mut LineFragment,
    columns:usize,orphans:usize,widows:usize,cuts:&mut Vec<SourceCut>,limit:usize,used_size:F)->Option<f32> {
    if columns==0{return None;}
    let mut low=0.0f32;let mut total=0.0f64;
    for item in items.iter_mut(){let source=project(item);let height=source.height;
        if !height.is_finite() || height<0.0{return None;}
        if !source.continuous {low=low.max(height);}total+=f64::from(height);
    }
    let mut high=typed_numeric::computed_f32(total);
    while low<high {
        let middle=typed_numeric::computed_f32((f64::from(low)+f64::from(high))*0.5);
        if middle>=high {break;}
        let fits=plan_continuous_fragments(items,project,middle,used_size(middle),orphans,widows,cuts,limit)
            .is_some_and(|used|used<=columns);
        if fits {high=middle;}else{low=middle.next_up();}
    }
    Some(high)
}


/// A position belongs to the shared column context, not to a private box
/// fragmentation context. Parallel overflow may return to an earlier column
/// when its fixed principal box finishes.
#[derive(Clone,Copy,Debug,PartialEq)]
pub(crate) struct SourcePosition {pub row:usize,pub column:usize,pub offset:f64}
#[derive(Clone,Copy,Debug)]
pub(crate) struct PrincipalProgress {
    remaining:Option<f64>,
    end:Option<SourcePosition>,
    decoration:(f32,f32,bool),
    minimum_remaining:f64,
    clone_constraints:[bool;2],
}
impl PrincipalProgress {
    pub(crate) fn new(height:f32,start:SourcePosition)->Option<Self> {
        if !height.is_finite() || height<0.0{return None;}
        Some(Self{remaining:Some(f64::from(height)),end:(height==0.0).then_some(start),decoration:(0.0,0.0,false),minimum_remaining:f64::from(height),clone_constraints:[false;2]})
    }
    fn automatic()->Self {Self{remaining:None,end:None,decoration:(0.0,0.0,false),minimum_remaining:0.0,clone_constraints:[false;2]}}
    pub(crate) fn remaining(&self)->f64 {self.minimum_remaining}
    pub(crate) fn end(&self)->Option<SourcePosition>{self.end}
}
/// Every active fixed principal receives only progress belonging to its own
/// normal flow. Once a descendant principal is complete, further ink in that
/// descendant is parallel to the ancestor's following normal content.
/// The deepest-to-root pass visits each active owner once and has no payload
/// copies or subtree traversal. Source and column-edge fill use this same path.
pub(crate) fn advance_principal_progress(active:&mut[PrincipalProgress],start:SourcePosition,
    extent:f64)->Option<()> {
    if !start.offset.is_finite() || start.offset<0.0 || !extent.is_finite() || extent<0.0{return None;}
    let mut propagated=extent;
    for owner in active.iter_mut().rev() {
        if let Some(remaining)=owner.remaining.as_mut() {
            propagated=propagated.min(*remaining);
            *remaining-=propagated;
            if owner.end.is_none() && *remaining==0.0 {
                owner.end=Some(SourcePosition{row:start.row,column:start.column,
                    offset:start.offset+propagated});
            }
        }
        owner.minimum_remaining=(owner.minimum_remaining-propagated).max(0.0);
    }
    Some(())
}


#[derive(Clone,Copy)]
pub(crate) struct PrincipalSourceSpan {pub start:usize,pub end:usize,pub parent:usize,pub height:f32,pub avoid_inside:bool}
pub(crate) trait PrincipalSpanSource {
    fn span(&self)->&PrincipalSourceSpan;
    // An automatic decorated box shares source ancestry and edges without
    // imposing a fixed-size endpoint on overflowing descendant ink.
    fn block_constraint(&self)->Option<f32> {Some(self.span().height)}
    fn decoration_edges(&self)->(f32,f32,bool) {(0.0,0.0,false)}
    // Preferred size fills its budget and bounds parallel overflow. An auto
    // minimum fills only after natural source ends; a maximum never fills.
    fn minimum_block_constraint(&self)->Option<f32>{self.block_constraint()}
    // The first edge pair was removed by canonical used content-box sizing.
    // Repeated edges charge only a corresponding numeric border-box bound.
    fn cloned_edge_constraints(&self)->[bool;2]{[false;2]}
}
impl PrincipalSpanSource for PrincipalSourceSpan {fn span(&self)->&PrincipalSourceSpan{self}}

#[derive(Clone,Copy,Debug)]
pub(crate) struct PrincipalSourcePlacement {pub source:usize,pub position:SourcePosition,pub height:f32,pub order:usize}
#[derive(Clone,Copy,Debug)]
pub(crate) struct PrincipalGapPlacement {pub owner:usize,pub position:SourcePosition,pub height:f32,pub order:usize}

#[derive(Clone,Copy,Debug)]
pub(crate) struct PrincipalDecorationPlacement {
    pub owner:usize,pub at:usize,pub position:SourcePosition,pub height:f32,pub start:bool,pub order:usize,
}

/// Numeric source trial state is independent of paint, fonts and CSS style.
/// The source payload remains in the normal layout owner. This state visits
/// interval boundaries in preorder and retains at most the existing box depth.
#[derive(Clone,Copy,Default,PartialEq,PartialOrd)]
pub(crate) enum DecorationTruncation {#[default] None,End,Start}

struct PrincipalTrial {
    next:usize,
    owners:Vec<usize>,
    progress:Vec<PrincipalProgress>,
    position:SourcePosition,
    columns:usize,
    decorations:Vec<PrincipalDecorationPlacement>,
    emission:usize,
    truncation:DecorationTruncation,
    start_reserve:f64,
    pending_start:bool,
}
pub(crate) struct PrincipalTrialScratch {state:PrincipalTrial,checkpoint:PrincipalTrial,row_start:PrincipalTrial,ends:Vec<(usize,SourcePosition)>}
impl PrincipalTrialScratch {
    pub(crate) fn ends(&self)->&[(usize,SourcePosition)]{&self.ends}
    pub(crate) fn decorations(&self)->&[PrincipalDecorationPlacement]{&self.state.decorations}
    pub(crate) fn take_decorations(&mut self)->Vec<PrincipalDecorationPlacement>{core::mem::take(&mut self.state.decorations)}
    pub(crate) fn truncate_decoration_edges(&mut self,value:DecorationTruncation){
        self.state.truncation=value;self.checkpoint.truncation=value;self.row_start.truncation=value;
    }
    pub(crate) fn checked_retained_bytes(&self)->Option<usize> {
        let indices=self.state.owners.capacity().checked_add(self.checkpoint.owners.capacity())?.checked_add(self.row_start.owners.capacity())?;
        let progress=self.state.progress.capacity().checked_add(self.checkpoint.progress.capacity())?.checked_add(self.row_start.progress.capacity())?;
        indices.checked_mul(core::mem::size_of::<usize>())?.checked_add(progress.checked_mul(core::mem::size_of::<PrincipalProgress>())?)?.checked_add(self.ends.capacity().checked_mul(core::mem::size_of::<(usize,SourcePosition)>())?)?.checked_add(self.state.decorations.capacity().checked_mul(core::mem::size_of::<PrincipalDecorationPlacement>())?)
    }
}
impl Default for PrincipalTrialScratch {
    fn default()->Self {
        let trial=||PrincipalTrial{next:0,owners:Vec::new(),progress:Vec::new(),position:SourcePosition{row:0,column:0,offset:0.0},columns:1,decorations:Vec::new(),emission:0,truncation:DecorationTruncation::None,start_reserve:0.0,pending_start:false};
        Self{state:trial(),checkpoint:trial(),row_start:trial(),ends:Vec::new()}
    }
}
impl PrincipalTrial {
    fn reset(&mut self) {self.next=0;self.owners.clear();self.progress.clear();self.decorations.clear();self.emission=0;self.start_reserve=0.0;self.pending_start=false;self.position=SourcePosition{row:0,column:0,offset:0.0};self.columns=1;}
    fn emission_order(&mut self)->Option<usize> {
        let order=self.emission;self.emission=self.emission.checked_add(1)?;Some(order)
    }
    // Edge cost belongs to normal progress of the owning box's parent.
    // It does not consume that box's own specified content-size budget.
    fn place_decoration(&mut self,owner:usize,at:usize,depth:usize,height:f32,start:bool,publish:bool,limit:usize,record_zero:bool)->Option<()> {
        if !height.is_finite() || height<0.0 || depth>self.progress.len(){return None;}
        if height==0.0 && !record_zero{return Some(());}
        if publish {
            if self.decorations.len()>=limit{return None;}self.decorations.try_reserve(1).ok()?;
            let order=self.emission_order()?;self.decorations.push(PrincipalDecorationPlacement{owner,at,position:self.position,height,start,order});
        }
        advance_principal_progress(&mut self.progress[..depth],self.position,f64::from(height))?;
        self.position.offset+=f64::from(height);Some(())
    }
    fn used_decoration_edge(&self,height:f32,start:bool,column_size:f32)->f32 {
        let truncate=if start{self.truncation==DecorationTruncation::Start}else{self.truncation>=DecorationTruncation::End};
        if !truncate{return height;}
        let reserve=if start{self.start_reserve}else{0.0};
        typed_numeric::computed_f32(f64::from(height).min((f64::from(column_size)-self.position.offset-reserve).max(0.0)))
    }
    fn decoration_end_reserve(&self)->f64 {
        if self.truncation>=DecorationTruncation::End{return 0.0;}
        self.progress.iter().filter(|owner|owner.decoration.2 && owner.end.is_none())
            .map(|owner|f64::from(owner.decoration.1)).sum()
    }
    fn content_edge(&self,column_size:f32)->f64 {(f64::from(column_size)-self.decoration_end_reserve()).max(0.0)}
    fn continue_decoration_edges(&mut self,at:usize,start:bool,column_size:f32,publish:bool,limit:usize)->Option<()> {
        for visit in 0..self.owners.len() {
            let depth=if start{visit}else{self.owners.len()-1-visit};
            let value=&self.progress[depth];
            if !value.decoration.2 || value.end.is_some(){continue;}
            let height=if start{value.decoration.0}else{value.decoration.1};
            let used=self.used_decoration_edge(height,start,column_size);
            self.place_decoration(self.owners[depth],at,depth,used,start,publish,limit,height>0.0)?;
            let owner=&mut self.progress[depth];
            if owner.clone_constraints[0] {
                owner.minimum_remaining=(owner.minimum_remaining-f64::from(used)).max(0.0);
            }
            if owner.clone_constraints[1] {
                if let Some(remaining)=owner.remaining.as_mut() {
                    // Min-size wins when differently sized numeric/intrinsic
                    // bounds meet after repeated border-box edge costs.
                    *remaining=(*remaining-f64::from(used)).max(owner.minimum_remaining);
                    // Preserve both sides of the committed break even when
                    // the restarted fragment has zero remaining content.
                    if start && *remaining==0.0 && owner.end.is_none() {
                        owner.end=Some(self.position);
                    }
                }
            }
        }
        Some(())
    }
    fn enter<S:PrincipalSpanSource>(&mut self,at:usize,spans:&[S],column_size:f32,empty_only:bool,
        gaps:&mut Vec<PrincipalGapPlacement>,ends:&mut Vec<(usize,SourcePosition)>,publish:bool,limit:usize)->Option<()> {
        while let Some(span)=spans.get(self.next).map(PrincipalSpanSource::span).filter(|span|span.start==at && (!empty_only || span.end==at)) {
            if span.start>span.end || span.parent!=self.owners.last().copied().unwrap_or(usize::MAX)
                || self.owners.len()>=512{return None;}
            let edges=spans[self.next].decoration_edges();
            if !edges.0.is_finite() || edges.0<0.0 || !edges.1.is_finite() || edges.1<0.0{return None;}
            let start=if edges.2{self.used_decoration_edge(edges.0,true,column_size)}else{edges.0};
            self.place_decoration(self.next,at,self.progress.len(),start,true,publish,limit,edges.0>0.0)?;
            let mut progress=match spans[self.next].block_constraint() {
                Some(height)=>PrincipalProgress::new(height,self.position)?,
                None=>PrincipalProgress::automatic(),
            };progress.decoration=edges;
            let minimum=spans[self.next].minimum_block_constraint().unwrap_or(0.0);
            if !minimum.is_finite() || minimum<0.0{return None;}
            progress.minimum_remaining=f64::from(minimum);
            if progress.remaining.is_some_and(|maximum|maximum<progress.minimum_remaining){return None;}
            progress.clone_constraints=spans[self.next].cloned_edge_constraints();
            self.owners.try_reserve(1).ok()?;self.progress.try_reserve(1).ok()?;
            self.owners.push(self.next);self.progress.push(progress);self.next+=1;
            // Empty source intervals close before their following sibling is
            // entered at the same source boundary. Their real principal gap
            // still consumes normal progress.
            if span.end==at{self.close(at,spans,column_size,gaps,ends,publish,limit)?;}
        }
        Some(())
    }
    fn advance(&mut self,height:f32)->Option<()> {self.advance_extent(f64::from(height))}
    fn advance_extent(&mut self,height:f64)->Option<()> {
        advance_principal_progress(&mut self.progress,self.position,height)?;
        self.position.offset+=height;Some(())
    }
    fn next_column(&mut self,column_size:f32,at:usize,publish:bool,limit:usize)->Option<()> {
        let fill=(self.content_edge(column_size)-self.position.offset).max(0.0);
        self.advance_extent(fill)?;
        self.continue_decoration_edges(at,false,column_size,publish,limit)?;
        self.position.column=self.position.column.checked_add(1).filter(|column|*column<limit)?;
        self.position.offset=0.0;self.columns=self.columns.max(self.position.column+1);
        self.continue_decoration_edges(at,true,column_size,publish,limit)
    }
    // A spanner separates complete column lines. Its out-of-flow block
    // extent does not consume a fixed ancestor's remaining principal budget.
    // Source-order content after the barrier cannot resume above the spanner,
    // even when a zero-height ancestor's descendant continues in parallel.
    fn next_row(&mut self,column_size:f32,at:usize,publish:bool,limit:usize)->Option<()> {
        let fill=(self.content_edge(column_size)-self.position.offset).max(0.0);
        self.advance_extent(fill)?;
        self.continue_decoration_edges(at,false,column_size,publish,limit)?;
        let row=self.position.row.checked_add(1).filter(|row|*row<limit)?;
        self.position=SourcePosition{row,column:0,offset:0.0};self.columns=1;
        for owner in &mut self.progress {
            if owner.end.is_some(){owner.end=Some(self.position);}
        }
        if self.truncation==DecorationTruncation::Start {
            // A row's fragment size is decided by its own real source trial.
            // Delay resumed start costs until that trial knows its next source.
            self.pending_start=true;Some(())
        }else{self.continue_decoration_edges(at.checked_add(1)?,true,column_size,publish,limit)}
    }
    fn close<S:PrincipalSpanSource>(&mut self,at:usize,spans:&[S],column_size:f32,
        gaps:&mut Vec<PrincipalGapPlacement>,ends:&mut Vec<(usize,SourcePosition)>,publish:bool,limit:usize)->Option<()> {
        while let Some(owner)=self.owners.last().copied().filter(|owner|spans[*owner].span().end==at) {
            // An empty last child can share its parent's end boundary. Enter
            // its source owner before closing the parent or following siblings.
            if spans.get(self.next).map(PrincipalSpanSource::span)
                .is_some_and(|next|next.start==at && next.parent==owner){break;}
            while self.progress.last()?.remaining()>0.0 {
                let remaining=self.progress.last()?.remaining();
                let room=(self.content_edge(column_size)-self.position.offset).max(0.0);
                let extent=remaining.min(room);
                if extent>0.0 {
                    if publish {
                        if gaps.len()>=limit{return None;}gaps.try_reserve(1).ok()?;
                        let order=self.emission_order()?;gaps.push(PrincipalGapPlacement{owner,position:self.position,height:typed_numeric::computed_f32(extent),order});
                    }
                    self.advance_extent(extent)?;
                }
                if self.progress.last()?.remaining()>0.0 {
                    if self.owners.iter().any(|index|spans[*index].span().avoid_inside){return None;}
                    self.start_reserve=self.progress.last()?.remaining().min(f64::from(column_size));
                    self.next_column(column_size,at,publish,limit)?;
                }
            }
            let progress=self.progress.pop()?;self.owners.pop();
            self.position=progress.end().unwrap_or(self.position);
            // A sliced terminal edge participates in the same candidate as
            // its last content source. Retry that source in the next column
            // rather than accepting an overflowing edge as a balanced fit.
            let end=if progress.decoration.2{self.used_decoration_edge(progress.decoration.1,false,column_size)}else{progress.decoration.1};
            if end>0.0 && self.position.offset>0.0 && self.position.offset+f64::from(end)>f64::from(column_size){return None;}
            self.place_decoration(owner,at,self.progress.len(),end,false,publish,limit,progress.decoration.1>0.0)?;
            let end=self.position;
            if publish {if ends.len()>=limit{return None;}ends.try_reserve(1).ok()?;ends.push((owner,end));}
        }
        Some(())
    }
    fn place_area(&mut self,source:usize,height:f32,capacity:f32,column_size:f32,forbidden_end:bool,
        placements:&mut Vec<PrincipalSourcePlacement>,publish:bool,limit:usize)->Option<()> {
        let mut remaining=f64::from(height);let mut started=false;
        if remaining==0.0 {
            // Break3 §4.5: a zero-sized fragment fits at the column edge,
            // but follows overflowing preceding content into the next column.
            if self.position.offset>f64::from(column_size){self.next_column(column_size,source,publish,limit)?;}
            if publish {
                if placements.len()>=limit{return None;}placements.try_reserve(1).ok()?;
                let order=self.emission_order()?;placements.push(PrincipalSourcePlacement{source,position:self.position,height:0.0,order});
            }
            return Some(());
        }
        while remaining>0.0 {
            self.start_reserve=remaining.min(f64::from(column_size));
            let ordinary_room=(self.content_edge(capacity)-self.position.offset).max(0.0);
            let room=(self.content_edge(column_size)-self.position.offset).max(0.0);
            if room==0.0 {
                // Before any area progress this would be its class A boundary,
                // so the whole source group must move before owners enter.
                if !started{return None;}self.next_column(column_size,source,publish,limit)?;continue;
            }
            let extent=remaining.min(room);
            if remaining>ordinary_room && remaining<=room && forbidden_end{return None;}
            if publish {
                if placements.len()>=limit{return None;}placements.try_reserve(1).ok()?;
                let order=self.emission_order()?;placements.push(PrincipalSourcePlacement{source,position:self.position,height:typed_numeric::computed_f32(extent),order});
            }
            self.advance_extent(extent)?;remaining-=extent;started=true;
            if remaining>0.0 && self.position.offset>=self.content_edge(column_size) {
                // Completing a fixed ancestor releases its terminal edge
                // reserve. Parallel child ink continues in that same column
                // before it needs another fragmentainer.
                self.start_reserve=remaining.min(f64::from(column_size));
                self.next_column(column_size,source,publish,limit)?;
            }
        }
        Some(())
    }
    fn copy_from(&mut self,other:&Self)->Option<()> {
        self.next=other.next;self.position=other.position;self.columns=other.columns;self.emission=other.emission;self.truncation=other.truncation;self.start_reserve=other.start_reserve;self.pending_start=other.pending_start;
        self.owners.clear();self.progress.clear();self.owners.try_reserve_exact(other.owners.len()).ok()?;
        self.progress.try_reserve_exact(other.progress.len()).ok()?;
        self.owners.extend_from_slice(&other.owners);self.progress.extend_from_slice(&other.progress);Some(())
    }
}
/// One source trial for atomic/line domains, continuous areas and real end
/// gaps. Its published coordinates and principal endpoints are consumed by
/// the same normal box renderer, including parallel overflow.
pub(crate) fn plan_principal_sources<T,S:PrincipalSpanSource>(items:&mut[T],project:fn(&mut T)->&mut LineFragment,
    spans:&[S],capacity:f32,column_size:f32,
    placements:&mut Vec<PrincipalSourcePlacement>,gaps:&mut Vec<PrincipalGapPlacement>,
    scratch:&mut PrincipalTrialScratch,publish:bool,limit:usize)->Option<usize> {
    let length=items.len();
    plan_principal_source_range(items,project,spans,0..length,capacity,column_size,
        placements,gaps,scratch,publish,limit,true,true)
}

// A multicol row uses the same forest trial. Continuing owners keep their
// actual remaining principal budget; only final source completion requires
// every owner to have closed. No line, style or paint payload is cloned.
fn plan_principal_source_range<T,S:PrincipalSpanSource>(items:&mut[T],project:fn(&mut T)->&mut LineFragment,
    spans:&[S],range:core::ops::Range<usize>,capacity:f32,column_size:f32,
    placements:&mut Vec<PrincipalSourcePlacement>,gaps:&mut Vec<PrincipalGapPlacement>,
    scratch:&mut PrincipalTrialScratch,publish:bool,limit:usize,reset:bool,finish:bool)->Option<usize> {
    if range.start>range.end || range.end>items.len() || limit==0 || items.len()>limit || spans.len()>limit || !capacity.is_finite() || capacity<0.0
        || !column_size.is_finite() || column_size<0.0{return None;}
    let capacity=capacity.max(1.0);let column_size=column_size.max(1.0);
    if reset{placements.clear();gaps.clear();}
    let PrincipalTrialScratch{state,checkpoint,ends,..}=scratch;if reset{state.reset();checkpoint.reset();ends.clear();}let mut at=range.start;
    if state.pending_start {
        let needed=if at<range.end{f64::from(project(&mut items[at]).height)}else{state.progress.last().map_or(0.0,PrincipalProgress::remaining)};
        state.start_reserve=needed.min(f64::from(column_size));
        state.continue_decoration_edges(at,true,column_size,publish,limit)?;state.pending_start=false;
    }
    while at<range.end {
        let start=at;let mut end=at+1;
        while end<range.end && !project(&mut items[end]).forced_start()
            && (!project(&mut items[end-1]).source_cut_allowed || project(&mut items[end-1]).avoids_end()) {end+=1;}
        // Numeric end gaps preceding this source are part of their original
        // owner, so a forced break on the following source cannot move them.
        state.start_reserve=0.0;
        state.enter(start,spans,column_size,true,gaps,ends,publish,limit)?;
        state.close(start,spans,column_size,gaps,ends,publish,limit)?;
        if project(&mut items[start]).forced_start() && (start>range.start || state.position.offset>0.0 || state.columns>1) {
            state.start_reserve=f64::from(project(&mut items[start]).height).min(f64::from(column_size));
            state.next_column(column_size,start,publish,limit)?;
        }
        checkpoint.copy_from(state)?;
        let first_placement=placements.len();let first_gap=gaps.len();let first_end=ends.len();let first_decoration=state.decorations.len();let mut retried=false;
        loop {
            let initial_position=state.position;
            let trial=(||->Option<()> {
                for index in start..end {
                    state.start_reserve=f64::from(project(&mut items[index]).height).min(f64::from(column_size));
                    state.enter(index,spans,column_size,false,gaps,ends,publish,limit)?;
                    state.close(index,spans,column_size,gaps,ends,publish,limit)?;
                    let fragment=project(&mut items[index]);
                    let height=fragment.height;let continuous=fragment.continuous;let avoid_inside=fragment.avoids_inside();
                    let forbidden_end=!fragment.source_cut_allowed || fragment.avoids_end();
                    if !height.is_finite() || height<0.0{return None;}
                    let forced_next=index+1==range.end && !finish
                        || index+1<items.len() && project(&mut items[index+1]).forced_start();
                    if continuous && !avoid_inside {
                        state.place_area(index,height,capacity,column_size,forbidden_end && !forced_next,placements,publish,limit)?;
                    }else{
                        if continuous && height>column_size{return None;}
                        let upper=if height==0.0 || continuous{column_size}else{capacity};
                        if state.position.offset+f64::from(height)>state.content_edge(upper)
                            && (state.position.offset>0.0 || continuous){return None;}
                        if publish {
                            if placements.len()>=limit{return None;}placements.try_reserve(1).ok()?;
                            let order=state.emission_order()?;placements.push(PrincipalSourcePlacement{source:index,position:state.position,height,order});
                        }
                        state.advance(height)?;
                    }
                    state.close(index+1,spans,column_size,gaps,ends,publish,limit)?;
                }
                Some(())
            })();
            if trial.is_some(){break;}
            if retried || initial_position.offset==0.0{return None;}
            state.copy_from(checkpoint)?;placements.truncate(first_placement);gaps.truncate(first_gap);ends.truncate(first_end);state.decorations.truncate(first_decoration);
            state.start_reserve=f64::from(project(&mut items[start]).height).min(f64::from(column_size));
            state.next_column(column_size,start,publish,limit)?;retried=true;
        }
        at=end;
    }
    state.enter(range.end,spans,column_size,false,gaps,ends,publish,limit)?;state.close(range.end,spans,column_size,gaps,ends,publish,limit)?;
    if finish && (state.next!=spans.len() || !state.owners.is_empty()){return None;}
    Some(state.columns)
}


// Snapshot only numeric owner progress while balancing a real source range.
// Each new owner is visited at its source entry; the bounded active ancestry
// is reused across all trials. Previously published rows/ends are untouched.
fn balanced_principal_source_range<T,S:PrincipalSpanSource,F:Fn(f32)->f32>(items:&mut[T],
    project:fn(&mut T)->&mut LineFragment,spans:&[S],range:core::ops::Range<usize>,columns:usize,
    placements:&mut Vec<PrincipalSourcePlacement>,gaps:&mut Vec<PrincipalGapPlacement>,
    scratch:&mut PrincipalTrialScratch,limit:usize,finish:bool,used_size:F)->Option<f32> {
    if columns==0 || range.start>range.end || range.end>items.len(){return None;}
    scratch.row_start.copy_from(&scratch.state)?;
    let first_placement=placements.len();let first_gap=gaps.len();let first_end=scratch.ends.len();
    let restore=|scratch:&mut PrincipalTrialScratch|scratch.state.copy_from(&scratch.row_start);
    let forced=plan_principal_source_range(items,project,spans,range.clone(),f32::MAX,f32::MAX,
        placements,gaps,scratch,false,limit,false,finish)?;
    restore(scratch)?;
    let columns=columns.max(forced);let mut low=0.0f32;let mut total=0.0f64;
    for index in range.clone() {
        let fragment=project(&mut items[index]);let height=fragment.height;
        if !height.is_finite() || height<0.0{return None;}
        if !fragment.continuous || fragment.avoids_inside(){low=low.max(height);}
        total+=f64::from(height);
    }
    for owner in &scratch.row_start.progress {
        total+=owner.remaining();
        // A continuing clone wraps the resumed fragment on both sides;
        // a slice only retains its still-unpublished terminal edge.
        if owner.end.is_none() {
            total+=f64::from(owner.decoration.1);
            if owner.decoration.2{total+=f64::from(owner.decoration.0);}
        }
    }
    for source in spans.iter().skip(scratch.row_start.next) {
        let span=source.span();if span.start>range.end{break;}
        if !span.height.is_finite() || span.height<0.0{return None;}
        let edges=source.decoration_edges();
        if !edges.0.is_finite() || edges.0<0.0 || !edges.1.is_finite() || edges.1<0.0{return None;}
        total+=f64::from(source.minimum_block_constraint().unwrap_or(0.0))+f64::from(edges.0)+f64::from(edges.1);
    }
    let mut high=typed_numeric::computed_f32(total);
    while low<high {
        let middle=typed_numeric::computed_f32((f64::from(low)+f64::from(high))*0.5);
        if middle>=high{break;}
        let fits=plan_principal_source_range(items,project,spans,range.clone(),middle,used_size(middle),
            placements,gaps,scratch,false,limit,false,finish).is_some_and(|used|used<=columns);
        restore(scratch)?;
        if fits{high=middle;}else{low=middle.next_up();}
    }
    placements.truncate(first_placement);gaps.truncate(first_gap);scratch.ends.truncate(first_end);
    Some(high)
}

#[derive(Clone,Copy)]
pub(crate) struct SpannedSourceRow {
    pub source_start:usize,
    pub source_end:usize,
    pub origin:f64,
    pub block_size:f32,
    // Balancing cuts source progress before the final row's used box ends.
    // Root height can extend that box and its rules without changing cuts.
    pub source_block_size:f32,
    pub occupied_columns:usize,
}
pub(crate) trait SpannerSource {
    fn source_index(&self)->usize;
    fn block_extent(&self)->f32;
    fn set_origin(&mut self,origin:f64);
    fn adjoining_margin_adjustment(&self,_previous:&Self)->f64{0.0}
}

/// All column lines share one source forest. A spanner is a zero-progress
/// source event separating ranges; its extent advances the physical row
/// origin, never a fixed ancestor's principal progress.
pub(crate) fn plan_spanned_principal_sources<T,S:PrincipalSpanSource,Q:SpannerSource,F:Fn(f32)->f32>(
    items:&mut[T],project:fn(&mut T)->&mut LineFragment,spans:&[S],spanners:&mut[Q],
    columns:usize,fill_auto:bool,maximum:Option<f32>,used_total:F,
    placements:&mut Vec<PrincipalSourcePlacement>,gaps:&mut Vec<PrincipalGapPlacement>,
    scratch:&mut PrincipalTrialScratch,rows:&mut Vec<SpannedSourceRow>,limit:usize)->Option<f32> {
    if columns==0 || spanners.len()>=limit || items.len()>limit{return None;}
    placements.clear();gaps.clear();scratch.ends.clear();scratch.state.reset();scratch.checkpoint.reset();rows.clear();
    let mut start=0usize;let mut origin=0.0f64;
    for row in 0..=spanners.len() {
        let finish=row==spanners.len();
        let end=if finish{items.len()}else{spanners[row].source_index()};
        if start>end || end>items.len() || !finish && end==items.len(){return None;}
        let range=start..end;
        let remaining=maximum.map(|maximum|typed_numeric::computed_f32((f64::from(maximum)-origin).max(0.0)));
        let requested=if finish && fill_auto && remaining.is_none(){1}else{columns};
        let balanced=balanced_principal_source_range(items,project,spans,range.clone(),requested,
            placements,gaps,scratch,limit,finish,|value|value)?;
        let capacity=if finish && fill_auto{remaining.unwrap_or(balanced)}else{remaining.map_or(balanced,|value|balanced.min(value))};
        // Root min/max/height apply once to the complete container. Pre-spanner
        // rows shorten independently, while the final row owns remaining space.
        let block_size=if finish {
            typed_numeric::computed_f32((f64::from(used_total(typed_numeric::computed_f32(origin+f64::from(capacity))))-origin).max(0.0))
        }else{capacity};
        let first_placement=placements.len();let first_gap=gaps.len();
        let occupied=plan_principal_source_range(items,project,spans,range,capacity,capacity,
            placements,gaps,scratch,true,limit,false,finish)?;
        if rows.len()>=limit{return None;}rows.try_reserve(1).ok()?;
        rows.push(SpannedSourceRow{source_start:start,source_end:end,origin,block_size,source_block_size:capacity,
            occupied_columns:if placements.len()==first_placement && gaps.len()==first_gap{0}else{occupied}});
        origin+=f64::from(block_size);
        if !finish {
            let height=spanners[row].block_extent();if !height.is_finite() || height<0.0{return None;}
            if row>0 && rows.last()?.occupied_columns==0 {
                origin+=spanners[row].adjoining_margin_adjustment(&spanners[row-1]);
            }
            spanners[row].set_origin(origin);origin+=f64::from(height);
            scratch.state.next_row(block_size,end,true,limit)?;
            start=end.checked_add(1)?;
        }
    }
    Some(typed_numeric::computed_f32(origin))
}

/// The upper-bound trial removes numeric column constraints to measure only
/// forced paths through the owner forest. Parallel branches share a maximum,
/// so their forced counts are not incorrectly added together. The actual
/// balancing trials retain the same used-column-size authority as rendering.
pub(crate) fn balanced_principal_source_height<T,S:PrincipalSpanSource,F:Fn(f32)->f32>(items:&mut[T],project:fn(&mut T)->&mut LineFragment,
    spans:&[S],columns:usize,placements:&mut Vec<PrincipalSourcePlacement>,
    gaps:&mut Vec<PrincipalGapPlacement>,scratch:&mut PrincipalTrialScratch,limit:usize,used_size:F)->Option<f32> {
    if columns==0{return None;}
    let forced=plan_principal_sources(items,project,spans,f32::MAX,f32::MAX,placements,gaps,scratch,false,limit)?;
    let columns=columns.max(forced);let mut low=0.0f32;let mut total=0.0f64;
    for item in items.iter_mut() {
        let fragment=project(item);let height=fragment.height;
        if !height.is_finite() || height<0.0{return None;}
        if !fragment.continuous || fragment.avoids_inside(){low=low.max(height);}
        total+=f64::from(height);
    }
    for source in spans {let span=source.span();if !span.height.is_finite() || span.height<0.0{return None;}
        let edges=source.decoration_edges();
        if !edges.0.is_finite() || edges.0<0.0 || !edges.1.is_finite() || edges.1<0.0{return None;}
        total+=f64::from(source.block_constraint().unwrap_or(0.0))+f64::from(edges.0)+f64::from(edges.1);}
    let mut high=typed_numeric::computed_f32(total);
    while low<high {
        let middle=typed_numeric::computed_f32((f64::from(low)+f64::from(high))*0.5);
        if middle>=high{break;}
        let fits=plan_principal_sources(items,project,spans,middle,used_size(middle),placements,gaps,scratch,false,limit)
            .is_some_and(|used|used<=columns);
        if fits{high=middle;}else{low=middle.next_up();}
    }
    Some(high)
}

#[cfg(test)]
mod decoration_source_tests {
    use super::*;
    struct AutomaticDecoration {span:PrincipalSourceSpan,edges:(f32,f32,bool)}
    impl PrincipalSpanSource for AutomaticDecoration {
        fn span(&self)->&PrincipalSourceSpan{&self.span}
        fn block_constraint(&self)->Option<f32>{None}
        fn decoration_edges(&self)->(f32,f32,bool){self.edges}
    }
    #[test]
    fn last_resort_edges_keep_real_source_progress_and_publish_actual_costs() {
        struct Source(PrincipalSourceSpan);
        impl PrincipalSpanSource for Source {
            fn span(&self)->&PrincipalSourceSpan{&self.0}
            fn block_constraint(&self)->Option<f32>{None}
            fn decoration_edges(&self)->(f32,f32,bool){(40.0,40.0,true)}
        }
        let sources=[Source(PrincipalSourceSpan{start:0,end:2,parent:usize::MAX,height:40.0,avoid_inside:false})];
        let mut items=[LineFragment::new(20.0),LineFragment::new(20.0)];
        let mut placements=Vec::new();let mut gaps=Vec::new();let mut scratch=PrincipalTrialScratch::default();
        assert_eq!(plan_principal_sources(&mut items,|item|item,&sources,30.0,30.0,&mut placements,&mut gaps,&mut scratch,true,64),None);
        scratch.truncate_decoration_edges(DecorationTruncation::End);
        assert_eq!(plan_principal_sources(&mut items,|item|item,&sources,30.0,30.0,&mut placements,&mut gaps,&mut scratch,true,64),None);
        scratch.truncate_decoration_edges(DecorationTruncation::Start);
        assert_eq!(plan_principal_sources(&mut items,|item|item,&sources,30.0,30.0,&mut placements,&mut gaps,&mut scratch,true,64),Some(2));
        assert_eq!(placements.iter().map(|piece|(piece.position.column,piece.position.offset,piece.height)).collect::<Vec<_>>(),[(0,10.0,20.0),(1,10.0,20.0)]);
        assert_eq!(scratch.decorations().iter().map(|edge|(edge.position.column,edge.start,edge.height)).collect::<Vec<_>>(),[(0,true,10.0),(0,false,0.0),(1,true,10.0),(1,false,0.0)]);
        assert!(gaps.is_empty());
    }

    #[test]
    fn cloned_edges_share_preferred_minimum_and_maximum_sizing_box_bounds() {
        struct Source{span:PrincipalSourceSpan,minimum:f32,maximum:Option<f32>,border_box:bool}
        impl PrincipalSpanSource for Source {
            fn span(&self)->&PrincipalSourceSpan{&self.span}
            fn decoration_edges(&self)->(f32,f32,bool){(10.0,10.0,true)}
            fn block_constraint(&self)->Option<f32>{self.maximum}
            fn minimum_block_constraint(&self)->Option<f32>{Some(self.minimum)}
            fn cloned_edge_constraints(&self)->[bool;2]{[self.border_box;2]}
        }
        for (minimum,maximum,natural,border_box,columns,last) in [
            (180.0,Some(180.0),0.0,true,2,100.0),
            (180.0,Some(180.0),0.0,false,3,40.0),
            (90.0,Some(90.0),0.0,true,2,20.0),
            (0.0,Some(0.0),0.0,true,1,20.0),
            (30.0,None,180.0,true,3,40.0),
            (0.0,Some(480.0),180.0,true,3,40.0),
            (0.0,Some(200.0),180.0,true,3,20.0),
            (180.0,None,140.0,true,2,100.0)] {
            let sources=[Source{span:PrincipalSourceSpan{start:0,end:usize::from(natural>0.0),parent:usize::MAX,height:minimum,avoid_inside:false},minimum,maximum,border_box}];
            let mut items=Vec::new();if natural>0.0 {let mut line=LineFragment::new(natural);line.continuous=true;items.push(line);}
            let mut placements=Vec::new();let mut gaps=Vec::new();let mut scratch=PrincipalTrialScratch::default();
            assert_eq!(plan_principal_sources(&mut items,|item|item,&sources,100.0,100.0,
                &mut placements,&mut gaps,&mut scratch,true,64),Some(columns));
            assert_eq!(scratch.ends()[0].1,SourcePosition{row:0,column:columns-1,offset:last});
            if minimum==0.0 {assert!(gaps.is_empty(),"maximum does not fill natural source");}
        }
    }

    #[test]
    fn fixed_terminal_edge_release_keeps_parallel_area_progress_in_the_same_column() {
        struct Source(PrincipalSourceSpan);
        impl PrincipalSpanSource for Source {
            fn span(&self)->&PrincipalSourceSpan{&self.0}
            fn decoration_edges(&self)->(f32,f32,bool){(15.0,15.0,true)}
        }
        let sources=[Source(PrincipalSourceSpan{start:0,end:1,parent:usize::MAX,height:70.0,avoid_inside:false})];
        let mut items=[LineFragment::new(185.0)];items[0].continuous=true;
        let mut placements=Vec::new();let mut gaps=Vec::new();let mut scratch=PrincipalTrialScratch::default();
        assert_eq!(plan_principal_sources(&mut items,|item|item,&sources,100.0,100.0,&mut placements,&mut gaps,&mut scratch,true,64),Some(2));
        assert_eq!(placements.iter().map(|piece|(piece.position.column,piece.position.offset,piece.height)).collect::<Vec<_>>(),[(0,15.0,70.0),(0,85.0,15.0),(1,0.0,100.0)]);
        assert_eq!(scratch.ends()[0].1,SourcePosition{row:0,column:0,offset:100.0});
        assert_eq!(scratch.decorations().iter().map(|edge|(edge.position.column,edge.position.offset,edge.start,edge.height)).collect::<Vec<_>>(),[(0,0.0,true,15.0),(0,85.0,false,15.0)]);
        assert!(gaps.is_empty());
    }

    #[test]
    fn cloned_edge_costs_share_automatic_source_forest_and_content_progress() {
        let sources=[AutomaticDecoration{span:PrincipalSourceSpan{start:0,end:1,parent:usize::MAX,height:170.0,avoid_inside:false},edges:(15.0,15.0,true)}];
        let mut items=[LineFragment::new(140.0)];items[0].continuous=true;
        let mut placements=Vec::new();let mut gaps=Vec::new();let mut scratch=PrincipalTrialScratch::default();
        assert_eq!(plan_principal_sources(&mut items,|item|item,&sources,100.0,100.0,&mut placements,&mut gaps,&mut scratch,true,64),Some(2));
        assert_eq!(placements.len(),2);
        assert_eq!((placements[0].position.column,placements[0].position.offset,placements[0].height),(0,15.0,70.0));
        assert_eq!((placements[1].position.column,placements[1].position.offset,placements[1].height),(1,15.0,70.0));
        assert_eq!(scratch.decorations().len(),4);
        assert_eq!(scratch.ends()[0].1,SourcePosition{row:0,column:1,offset:100.0});
        assert!(gaps.is_empty());
        assert_eq!(balanced_principal_source_height(&mut items,|item|item,&sources,2,&mut placements,&mut gaps,&mut scratch,64,|size|size),Some(100.0));
    }
}
