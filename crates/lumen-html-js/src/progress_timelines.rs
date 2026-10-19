//! Native progress timeline objects. Registered by the animation adapter once
//! its timeline argument and percentage time bindings share these records.
use super::*;

#[derive(Clone)]
pub(super) struct ProgressTimeline {
    pub(super) id: u32,
    pub(super) realm: Weak<DomRealm>,
    pub(super) binding: Rc<RefCell<ProgressBinding>>,
    axis: String,
    view: bool,
    input_node: Option<NodeId>,
    _retention: Option<Rc<NodeRetention>>,
    geometry_revision: Rc<Cell<Option<(u64,u64,u64)>>>,
}

impl ProgressTimeline {
    fn from_options(ctx: &mut Ctx, options: Option<Value>, view: bool) -> OpResult<Self> {
        let owner=hub(ctx)?.borrow().default_realm.upgrade()
            .ok_or_else(||OpError::new("InvalidStateError","timeline document was destroyed"))?;
        // Older Animation Worklet clients use scrollSource/orientation;
        // normalize those names into the same current scroll geometry.
        let orientation=crate::ui_events::dictionary_string(ctx,&options,"orientation","block",false)?;
        let legacy_axis=match orientation.as_str(){"vertical"=>"y","horizontal"=>"x",value=>value};
        let axis=crate::ui_events::dictionary_string(ctx,&options,"axis",legacy_axis,false)?;
        if !matches!(axis.as_str(),"block"|"inline"|"x"|"y") {
            return Err(OpError::type_error("invalid progress timeline axis"));
        }
        let field=if view{"subject"}else{"source"};
        let mut supplied=crate::ui_events::dictionary_member(ctx,&options,field)?;
        if !view && supplied.as_ref().is_none_or(|value|matches!(value,Value::Undefined)) {
            supplied=crate::ui_events::dictionary_member(ctx,&options,"scrollSource")?;
        }
        let default=if view{None}else{crate::scrolling::document_scrolling_element(&owner)?};
        let node=match supplied {
            None|Some(Value::Undefined)=>default,
            Some(Value::Null)=>None,
            Some(value)=>{
                let (realm,node)=ctx.with_instance::<DomNode,_>(&value,|node|(node.realm.clone(),node.id))
                    .map_err(|_|OpError::type_error("timeline source or subject must be an Element"))?;
                if !Rc::ptr_eq(&realm,&owner) {return Err(OpError::new("NotSupportedError","timeline source belongs to another document"));}
                if !matches!(realm.session.borrow().document().kind(node),Ok(lumen_html::NodeKind::Element{..})) {
                    return Err(OpError::type_error("timeline source or subject must be an Element"));
                }
                Some(node)
            }
        };
        let id={let hub=hub(ctx)?;let mut state=hub.borrow_mut();state.next_timeline_id=state.next_timeline_id.wrapping_add(1).max(1);state.next_timeline_id};
        let timeline=Self{id,realm:Rc::downgrade(&owner),binding:Rc::new(RefCell::new(ProgressBinding{
            sampled_time:None,source:if view{None}else{node},subject:view.then_some(node).flatten(),
            horizontal:false,range:animation::ProgressRange::parse("normal").unwrap(),insets:None,duration_auto:true,
        })),axis,view,input_node:node,_retention:node.map(|node|Rc::new(NodeRetention::new(&owner,node))),geometry_revision:Rc::new(Cell::new(None))};
        Ok(timeline)
    }

    pub(super) fn update_geometry(&self) -> OpResult<()> {
        let Some(realm)=self.realm.upgrade() else {self.binding.borrow_mut().sampled_time=None;return Ok(())};
        realm.flush_layout()?;
        let revision={let session=realm.session.borrow();(session.document().version(),session.paint_revision(),session.frame_id())};
        if self.geometry_revision.get()==Some(revision){return Ok(());}
        let mut binding=self.binding.borrow().clone();
        let document_scroller=crate::scrolling::document_scrolling_element(&realm)?;
        if let Some(node)=self.input_node {
            let mut session=realm.session.borrow_mut();
            if self.view {
                let(source,horizontal)=nearest_progress_scroll_source(&mut session,node,&self.axis)?;
                binding.source=source;binding.horizontal=horizontal;
            }else {
                binding.horizontal=progress_axis_horizontal(&mut session,node,&self.axis)?;
                binding.source=if document_scroller==self.input_node{Some(session.document().root())}else{self.input_node};
            }
        }
        binding.sampled_time=progress_time(&realm,&binding);
        *self.binding.borrow_mut()=binding;
        self.geometry_revision.set(Some(revision));
        Ok(())
    }

    fn source(&self,ctx:&mut Ctx)->OpResult<Value>{
        self.update_geometry()?;
        let Some(realm)=self.realm.upgrade() else{return Ok(Value::Null)};
        let mut source=self.binding.borrow().source;
        if source==Some(realm.session.borrow().document().root()){source=crate::scrolling::document_scrolling_element(&realm)?;}
        Ok(realm.wrap_option(ctx,source))
    }

    fn current_time(&self,ctx:&mut Ctx)->OpResult<Value>{
        self.update_geometry()?;
        Ok(self.binding.borrow().sampled_time.map_or(Value::Null,|time|crate::cssom::timeline_percent(ctx,time/10.0)))
    }
}

#[derive(Clone)]
#[lumen_bind::class(name="ScrollTimeline",hint(js(webidl)))]
pub(super) struct DomScrollTimeline {pub(super) timeline:ProgressTimeline}

#[lumen_bind::methods]
impl DomScrollTimeline {
    #[constructor]
    fn new(ctx:&mut Ctx,options:Option<Value>)->OpResult<Self>{Ok(Self{timeline:ProgressTimeline::from_options(ctx,options,false)?})}
    #[getter]
    fn source(&self,ctx:&mut Ctx)->OpResult<Value>{self.timeline.source(ctx)}
    #[getter]
    fn scroll_source(&self,ctx:&mut Ctx)->OpResult<Value>{self.timeline.source(ctx)}
    #[getter]
    fn axis(&self)->String{self.timeline.axis.clone()}
    #[getter(name="currentTime")]
    fn current_time(&self,ctx:&mut Ctx)->OpResult<Value>{self.timeline.current_time(ctx)}
    #[getter]
    fn duration(&self,ctx:&mut Ctx)->Value{crate::cssom::timeline_percent(ctx,100.0)}
}

#[derive(Clone)]
#[lumen_bind::class(name="ViewTimeline",extends=DomScrollTimeline,hint(js(webidl)))]
pub(super) struct DomViewTimeline {pub(super) base:DomScrollTimeline}

#[lumen_bind::methods]
impl DomViewTimeline {
    #[constructor]
    fn new(ctx:&mut Ctx,options:Option<Value>)->OpResult<Self>{Ok(Self{base:DomScrollTimeline{timeline:ProgressTimeline::from_options(ctx,options,true)?}})}
    #[getter]
    fn subject(&self,ctx:&mut Ctx)->Value{self.base.timeline.realm.upgrade().map_or(Value::Null,|realm|realm.wrap_option(ctx,self.base.timeline.binding.borrow().subject))}
}
