//! Isolated CSS painters backed by the shared Canvas2D raster and retained
//! procedural-image requests. JavaScript never runs under a layout borrow.
use super::*;
use lumen_html::css;
use crate::realm_services::{capture_realm_value, RealmServices};
use lumen::embed::{JsHost, RealmHandle};
use lumen_bind::Host;
use lumen_common::limits::ByteBudget;
use lumen_html::paint::PaintWorkletRequest;
use std::collections::{HashMap, HashSet};
use std::rc::Weak;
use std::sync::Arc;

const MAX_DEFINITIONS: usize=1024;
const MAX_INPUTS: usize=256;
const PIXEL_BUDGET: usize=64*1024*1024;
struct PaintPixelBudget(Arc<ByteBudget>);

struct PaintState {
    owner: Weak<DomRealm>,
    scope: Option<RealmHandle>,
    modules: HashSet<String>,
    definitions: HashMap<String,PaintDefinition>,
    failures: Vec<PaintWorkletRequest>,
    pixels: Arc<ByteBudget>,
}
#[derive(Clone)]
struct PaintDefinition {
    constructor: WeakValue,
    paint: WeakValue,
    instance: Option<WeakValue>,
    arguments: Arc<[Arc<str>]>,
    alpha: bool,
    invalid: bool,
}
fn service(ctx:&mut Ctx)->OpResult<Rc<RefCell<PaintState>>> {
    RealmServices::<RefCell<PaintState>>::current(ctx)
        .ok_or_else(||OpError::new("InvalidStateError","paint scope is unavailable"))
}
fn strings(ctx:&mut Ctx,constructor:&Value,name:&str,cap:usize)->OpResult<Vec<Arc<str>>> {
    let value=ctx.member_get(constructor,name).map_err(OpError::thrown)?;
    if matches!(value,Value::Undefined){return Ok(Vec::new());}
    ctx.convert_iterable(&value,cap,|ctx,value|{
        let value=ctx.coerce_string(&value).map_err(OpError::thrown)?;
        if value.len()>1024{return Err(OpError::new("QuotaExceededError","paint descriptor string exceeds budget"));}
        Ok(Arc::from(value.as_ref()))
    })
}
#[lumen_bind::module(name="paint_worklet_scope")]
mod scope_bindings {
    use super::*;
    #[op(rename(js="registerPaint"))]
    pub fn register_paint(ctx:&mut Ctx,name:&str,constructor:Value)->OpResult<()> {
        if name.is_empty()||name.len()>1024{return Err(OpError::type_error("paint name must not be empty or exceed its budget"));}
        if !ctx.value_is_constructor(&constructor){return Err(OpError::type_error("paint definition must be a constructor"));}
        let state=service(ctx)?;
        if state.borrow().definitions.contains_key(name){return Err(OpError::new("NotSupportedError","paint name is already registered"));}
        if state.borrow().definitions.len()>=MAX_DEFINITIONS{return Err(OpError::new("QuotaExceededError","paint definition budget exhausted"));}
        let inputs=strings(ctx,&constructor,"inputProperties",MAX_INPUTS)?;
        let arguments=strings(ctx,&constructor,"inputArguments",64)?;
        if arguments.iter().any(|syntax|syntax.as_ref()!="*" && !css::registered_properties::valid_typed_syntax(syntax)) {
            return Err(OpError::type_error("paint argument syntax is invalid"));
        }
        let prototype=ctx.member_get(&constructor,"prototype").map_err(OpError::thrown)?;
        let paint=ctx.member_get(&prototype,"paint").map_err(OpError::thrown)?;
        if !paint.is_callable(){return Err(OpError::type_error("paint prototype must have a paint method"));}
        let options=ctx.member_get(&constructor,"contextOptions").map_err(OpError::thrown)?;
        let alpha=if matches!(options,Value::Null|Value::Undefined){true}else{
            let value=ctx.member_get(&options,"alpha").map_err(OpError::thrown)?;
            matches!(value,Value::Undefined)||ctx.to_boolean(&value)
        };
        let owner=state.borrow().owner.upgrade().ok_or_else(||OpError::new("InvalidStateError","paint document was retired"))?;
        owner.session.borrow_mut().register_paint_worklet(Arc::from(name),inputs.into())
            .map_err(|_|OpError::new("QuotaExceededError","paint metadata budget exhausted"))?;
        let constructor=capture_realm_value(ctx,constructor)?;
        let paint=capture_realm_value(ctx,paint)?;
        state.borrow_mut().definitions.insert(name.into(),PaintDefinition {constructor,paint,instance:None,
            arguments:arguments.into(),alpha,invalid:false});
        Ok(())
    }
}
fn ensure_scope(ctx:&mut Ctx,state:&Rc<RefCell<PaintState>>)->OpResult<RealmHandle> {
    if let Some(scope)=state.borrow().scope.clone(){return Ok(scope);}
    let owner=state.borrow().owner.upgrade().ok_or_else(||OpError::new("InvalidStateError","paint document was retired"))?;
    let ratio=owner.device_pixel_ratio.get();
    let scope=ctx.create_host_realm();
    ctx.with_host_realm(&scope,|ctx|{
        RealmServices::replace_shared_current(ctx,state.clone());
        let global=ctx.global_object();
        ctx.install_module::<scope_bindings::Module>(&global).map_err(OpError::thrown)?;
        crate::canvas::install_paint_context(ctx)?;
        crate::cssom::install_typed_numeric(ctx)?;
        for (name,constructor) in [
            ("PaintSize",ctx.class_constructor::<DomPaintSize>()),
            ("StylePropertyMapReadOnly",ctx.class_constructor::<DomPaintStyleMap>()),
        ] {crate::install_interface(ctx,&global,name,constructor)?;}
        let descriptor=ctx.new_object_with_proto(&Value::Null);
        for (name,value) in [("value",Value::Num(ratio)),("writable",Value::Bool(false)),
            ("enumerable",Value::Bool(true)),("configurable",Value::Bool(false))] {
            ctx.member_set(&descriptor,name,value).map_err(OpError::thrown)?;
        }
        ctx.define_property_value(&global,Value::str("devicePixelRatio"),&descriptor).map_err(OpError::thrown)?;
        Ok::<_,OpError>(())
    }).map_err(crate::browsing_context::host_realm_error)??;
    state.borrow_mut().scope=Some(scope.clone());Ok(scope)
}
fn load_module(ctx:&mut Ctx,state:&Rc<RefCell<PaintState>>,input:&str,credentials:&str)->OpResult<Value> {
    let owner=state.borrow().owner.upgrade().ok_or_else(||OpError::new("InvalidStateError","paint document was retired"))?;
    let url=lumen_common::url::parse(input,Some(&owner.base_url()))
        .map_err(|_|OpError::new("SyntaxError","invalid paint module URL"))?.href();
    if state.borrow().modules.len()>=MAX_DEFINITIONS && !state.borrow().modules.contains(&url) {
        return Err(OpError::new("QuotaExceededError","paint module budget exhausted"));
    }
    let scope=ensure_scope(ctx,state)?;
    let promise=crate::animation_worklet::load_module_in_scope(ctx,&owner,&scope,&url,credentials)?;
    state.borrow_mut().modules.insert(url);Ok(promise)
}
#[lumen_bind::class(name="PaintSize",hint(js(webidl)))]
struct DomPaintSize {width:f64,height:f64}
#[lumen_bind::methods]
impl DomPaintSize {
    #[getter] fn width(&self)->f64{self.width}
    #[getter] fn height(&self)->f64{self.height}
}
#[lumen_bind::class(name="StylePropertyMapReadOnly",hint(js(webidl)))]
struct DomPaintStyleMap {values:Vec<(Arc<str>,Option<Arc<str>>,Option<Arc<str>>)>}
#[lumen_bind::methods]
impl DomPaintStyleMap {
    #[getter] fn size(&self)->usize{self.values.len()}
    fn get(&self,ctx:&mut Ctx,property:&str)->OpResult<Value> {
        let Some((_,Some(value),syntax))=self.values.iter().find(|(name,_,_)|name.as_ref()==property)else{return Ok(Value::Undefined);};
        crate::cssom::registered_style_value(ctx,property,value,syntax.as_deref())
    }
    fn get_all(&self,ctx:&mut Ctx,property:&str)->OpResult<Value> {
        let value=self.get(ctx,property)?;
        Ok(JsHost::from_list(ctx,if matches!(value,Value::Undefined){Vec::new()}else{vec![value]}))
    }
    fn has(&self,property:&str)->bool{self.values.iter().any(|(name,value,_)|name.as_ref()==property && value.is_some())}
}

pub(crate) fn install(ctx:&mut Ctx,realm:&Rc<DomRealm>)->OpResult<()> {
    if let Some(previous)=RealmServices::<RefCell<PaintState>>::current(ctx) {
        if previous.borrow().owner.upgrade().is_some_and(|owner|Rc::ptr_eq(&owner,realm)){return Ok(());}
        if let Some(owner)=previous.borrow().owner.upgrade(){retire_document(ctx,&owner)?;}
    }
    let pixels=match ctx.op_state().get::<PaintPixelBudget>() {
        Some(budget)=>budget.0.clone(),
        None=>{let pixels=ByteBudget::new(PIXEL_BUDGET);ctx.op_state().put(PaintPixelBudget(pixels.clone()));pixels}
    };
    let state=RealmServices::replace_current(ctx,RefCell::new(PaintState {owner:Rc::downgrade(realm),scope:None,
        modules:HashSet::new(),definitions:HashMap::new(),failures:Vec::new(),pixels}));
    let loader=Rc::new(move |ctx:&mut Ctx,url:&str,credentials:&str|load_module(ctx,&state,url,credentials));
    let worklet=crate::animation_worklet::new_worklet(ctx,loader);
    let global=ctx.global_object();
    let css=ctx.member_get(&global,"CSS").map_err(OpError::thrown)?;
    ctx.member_set(&css,"paintWorklet",worklet).map_err(OpError::thrown)?;Ok(())
}

pub(crate) fn update(ctx:&mut Ctx,realm:&Rc<DomRealm>)->OpResult<()> {
    let Some(state)=RealmServices::<RefCell<PaintState>>::current(ctx)else{return Ok(());};
    if !state.borrow().owner.upgrade().is_some_and(|owner|Rc::ptr_eq(&owner,realm)){return Ok(());}
    let requests=realm.session.borrow().paint_worklet_requests();
    state.borrow_mut().failures.retain(|request|requests.contains(request));
    let mut completed=Vec::new();
    for request in requests {
        let definition=state.borrow().definitions.get(request.name.as_ref()).cloned();
        let Some(definition)=definition.filter(|definition|!definition.invalid)else{continue;};
        if state.borrow().failures.contains(&request){continue;}
        if request.arguments.len()!=definition.arguments.len() || request.width<=0.0 || request.height<=0.0 {continue;}
        if request.width>8192.0 || request.height>8192.0 {continue;}
        let values=request.properties.iter().map(|(name,value)|
            (name.clone(),value.clone(),realm.session.borrow().registered_custom_property_syntax(name).map(Arc::from)))
            .collect::<Vec<_>>();
        let scope=ensure_scope(ctx,&state)?;
        let result=ctx.with_host_realm(&scope,|ctx|{
            let instance=match definition.instance.as_ref().and_then(WeakValue::upgrade) {
                Some(instance)=>instance,
                None=>{
                    let constructor=definition.constructor.upgrade().ok_or_else(||OpError::new("InvalidStateError","paint definition was collected"))?;
                    let instance=ctx.construct_value(constructor,&[]).map_err(OpError::thrown)?;
                    let retained=capture_realm_value(ctx,instance.clone())?;
                    if let Some(definition)=state.borrow_mut().definitions.get_mut(request.name.as_ref()){definition.instance=Some(retained);}
                    instance
                }
            };
            let mut arguments=Vec::new();
            for (syntax,value) in definition.arguments.iter().zip(request.arguments.iter()) {
                if !css::registered_properties::accepts_value(syntax,value){return Err(OpError::type_error("paint argument does not match descriptor syntax"));}
                arguments.push(crate::cssom::registered_style_value(ctx,"--paint-argument",value,Some(syntax))?);
            }
            let context=crate::canvas::paint_context(ctx,request.width.ceil() as u32,request.height.ceil() as u32,definition.alpha,&state.borrow().pixels)?;
            let size=ctx.new_instance(DomPaintSize {width:request.width as f64,height:request.height as f64});
            let properties=ctx.new_instance(DomPaintStyleMap {values});
            let arguments=JsHost::from_list(ctx,arguments);
            let paint=definition.paint.upgrade().ok_or_else(||OpError::new("InvalidStateError","paint method was collected"))?;
            ctx.call(paint,instance,&[context.clone(),size,properties,arguments])
                .map_err(|error|OpError::thrown(lumen::embed::abrupt_value(error)))?;
            crate::canvas::paint_context_snapshot(ctx,&context,&state.borrow().pixels)
        }).map_err(crate::browsing_context::host_realm_error)?;
        match result {
            Ok(pixels)=>completed.push((request,pixels)),
            Err(error)=>{
                if state.borrow().failures.len()<256{state.borrow_mut().failures.push(request);}
                let reason=error.to_value(ctx);DomRealm::report_exception(ctx, reason);
            }
        }
    }
    if !completed.is_empty(){realm.session.borrow_mut().complete_paint_worklets(completed)
        .map_err(|_|OpError::new("QuotaExceededError","paint image cache budget exhausted"))?;}
    Ok(())
}

pub(crate) fn retire_document(ctx:&mut Ctx,realm:&Rc<DomRealm>)->OpResult<()> {
    let Some(state)=RealmServices::<RefCell<PaintState>>::current(ctx)else{return Ok(());};
    if !state.borrow().owner.upgrade().is_some_and(|owner|Rc::ptr_eq(&owner,realm)){return Ok(());}
    let scope=state.borrow_mut().scope.take();
    {let mut state=state.borrow_mut();state.definitions.clear();state.modules.clear();state.failures.clear();state.owner=Weak::new();}
    realm.session.borrow_mut().clear_paint_worklets();
    if let Some(scope)=scope{
        ctx.cancel_async_module_imports_for_realm(&scope);
        ctx.dispose_host_realm(&scope).map_err(|_|OpError::new("InvalidStateError","paint realm could not be retired"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen::Engine;
    fn eval(engine:&mut Engine,source:&str)->Value {
        match engine.eval_value(source).expect("paint test parses") {
            Ok(value)=>value,
            Err(error)=>panic!("paint script exception: {}",engine.ctx().coerce_string(&error).unwrap_or_else(|_|"<unprintable>".into())),
        }
    }
    fn setup(module:&'static str)->(Engine,Rc<DomRealm>) {
        setup_module(module,true)
    }
    fn setup_module(module:&'static str,settled:bool)->(Engine,Rc<DomRealm>) {
        let mut engine=Engine::new();
        let realm=crate::install(engine.ctx(),"<style>html,body{margin:0}#target{width:16px;height:16px;background:blue;--extent:8}</style><div id=target></div>",192).unwrap();
        realm.set_document_url("https://paint.test/page.html");
        realm.set_layout_flusher(Rc::new(|session|session.display_list(32,32,crate::canvas::canvas_fallback_fonts())
            .map(|_|()).map_err(|error|format!("{error:?}"))));
        install(engine.ctx(),&realm).unwrap();
        engine.ctx().install_module_fetch_loader(Rc::new(move |request|{
            Some(lumen::ModuleFetchResult {key:request.specifier,source:module.into(),script_context:request.script_context})
        }));
        eval(&mut engine,"globalThis.loaded=false;globalThis.modulePromise=CSS.paintWorklet.addModule('painter.js');modulePromise.then(value=>{if(value!==undefined)throw Error('module fulfillment');loaded=true;});");
        engine.ctx().drain_microtasks_for_host();
        assert!(matches!(eval(&mut engine,"modulePromise instanceof Promise && CSS.paintWorklet instanceof Worklet"),Value::Bool(true)));
        assert!(matches!(eval(&mut engine,"loaded"),Value::Bool(value) if value==settled));
        (engine,realm)
    }
    fn frame(engine:&mut Engine,realm:&Rc<DomRealm>,time:f64) {
        let host_realm=engine.ctx().current_host_realm();
        assert!(crate::scheduling::run_animation_frame_in_realm_at(engine,&host_realm,time).is_empty());
        realm.update_rendered_focus(engine.ctx()).unwrap();
    }
    fn pixels(realm:&Rc<DomRealm>)->lumen_html_image::Rgba8Image {
        realm.with_session(|session|{
            let list=session.display_list(32,32,crate::canvas::canvas_fallback_fonts()).unwrap();
            lumen_html_image::render(list,32,32,1.0,false).unwrap()
        })
    }
    fn pixel(image:&lumen_html_image::Rgba8Image,x:usize,y:usize)->[u8;4] {
        image.pixels[(y*image.width as usize+x)*4..][..4].try_into().unwrap()
    }
    #[test]
    fn paint_worklet_pending_module_replaces_real_before_load_background() {
        // setup_module returns the driver by value while its actual ESM
        // coroutine is suspended. Resuming it verifies Engine move safety.
        let (mut engine,realm)=setup_module("await new Promise(resolve=>globalThis.resume=resolve);registerPaint('late',class{paint(ctx,size){ctx.fillStyle='green';ctx.fillRect(0,0,size.width,size.height)}})",false);
        eval(&mut engine,"document.getElementById('target').style.backgroundImage='paint(late)'");
        frame(&mut engine,&realm,1000.0);
        assert_eq!(pixel(&pixels(&realm),2,2),[0,0,255,255]);
        let scope=service(engine.ctx()).unwrap().borrow().scope.clone().unwrap();
        engine.ctx().with_host_realm(&scope,|ctx|{
            let global=ctx.global_object();
            let resume=ctx.member_get(&global,"resume").unwrap_or_else(|_|panic!("author module resolver lookup failed"));
            ctx.call(resume,Value::Undefined,&[]).unwrap_or_else(|_|panic!("author module resolver call failed"));
        }).unwrap();
        engine.ctx().drain_microtasks_for_host();
        assert!(matches!(eval(&mut engine,"loaded"),Value::Bool(true)));
        frame(&mut engine,&realm,1016.0);
        assert_eq!(pixel(&pixels(&realm),2,2),[0,128,0,255]);
    }
    #[test]
    fn paint_worklet_scope_real_canvas_pixels_and_declared_property_invalidation() {
        let (mut engine,realm)=setup(r#"
            if(typeof window!=='undefined'||typeof document!=='undefined'||typeof OffscreenCanvas!=='undefined'||typeof registerAnimator!=='undefined')throw Error('Window globals leaked');
            registerPaint('geometry',class {
                static get inputProperties(){return ['--extent'];}
                paint(ctx,size,properties){
                    if(!(ctx instanceof PaintRenderingContext2D)||ctx.canvas!==undefined||ctx.fillText!==undefined||ctx.getImageData!==undefined)throw Error('paint context interface');
                    if(!(properties instanceof StylePropertyMapReadOnly)||properties.size!==1||!properties.has('--extent'))throw Error('paint map');
                    ctx.fillStyle='green';const extent=parseFloat(properties.get('--extent').toString());
                    ctx.fillRect(0,0,Math.min(size.width,extent),Math.min(size.height,extent));
                }
            });
        "#);
        eval(&mut engine,"document.getElementById('target').style.backgroundImage='paint(geometry)'");
        frame(&mut engine,&realm,1000.0);
        let first=pixels(&realm);assert_eq!(pixel(&first,2,2),[0,128,0,255]);assert_eq!(pixel(&first,12,12),[0,0,255,255]);
        eval(&mut engine,"document.getElementById('target').style.setProperty('--extent','14')");
        frame(&mut engine,&realm,1016.0);
        let changed=pixels(&realm);assert_eq!(pixel(&changed,12,12),[0,128,0,255]);
    }
    #[test]
    fn paint_worklet_registered_initial_value_and_real_animation_sample_update_pixels() {
        let (mut engine,realm)=setup("registerPaint('geometry',class{static get inputProperties(){return ['--extent']}paint(ctx,size,properties){const value=properties.get('--extent');if(!(value instanceof CSSUnitValue))throw Error('typed registered value');ctx.fillStyle='green';ctx.fillRect(0,0,value.value,value.value)}})");
        eval(&mut engine,"CSS.registerProperty({name:'--extent',syntax:'<number>',inherits:false,initialValue:'4'});document.getElementById('target').style.backgroundImage='paint(geometry)';globalThis.effect=new KeyframeEffect(document.getElementById('target'),{'--extent':['0','16']},{duration:1000,fill:'both'});globalThis.animation=new Animation(effect,document.timeline);animation.currentTime=500;");
        frame(&mut engine,&realm,1000.0);
        let image=pixels(&realm);assert_eq!(pixel(&image,4,4),[0,128,0,255]);assert_eq!(pixel(&image,12,12),[0,0,255,255]);
        assert!(matches!(eval(&mut engine,"getComputedStyle(document.getElementById('target')).getPropertyValue('--extent')==='8'"),Value::Bool(true)));
    }
    #[test]
    fn paint_worklet_registration_repaints_identical_text_with_new_typed_value() {
        let (mut engine,realm)=setup("registerPaint('typed',class{static get inputProperties(){return ['--extent']}paint(ctx,size,properties){let value=properties.get('--extent');ctx.fillStyle=value instanceof CSSUnitValue && value.value===8 && value.unit==='px' ? 'green' : 'red';ctx.fillRect(0,0,size.width,size.height)}})");
        eval(&mut engine,"document.getElementById('target').style.setProperty('--extent','8px');document.getElementById('target').style.backgroundImage='paint(typed)'");
        frame(&mut engine,&realm,1000.0);
        assert_eq!(pixel(&pixels(&realm),2,2),[255,0,0,255]);
        eval(&mut engine,"CSS.registerProperty({name:'--extent',syntax:'<length>',initialValue:'0px',inherits:false})");
        frame(&mut engine,&realm,1016.0);
        assert_eq!(pixel(&pixels(&realm),2,2),[0,128,0,255]);
        assert!(matches!(eval(&mut engine,"getComputedStyle(document.getElementById('target')).getPropertyValue('--extent')==='8px'"),Value::Bool(true)));
    }
    #[test]
    fn paint_worklet_registration_repaints_previous_missing_value() {
        let (mut engine,realm)=setup("registerPaint('tone',class{static get inputProperties(){return ['--tone']}paint(ctx,size,properties){ctx.fillStyle=properties.get('--tone').toString();ctx.fillRect(0,0,size.width,size.height)}})");
        eval(&mut engine,"document.getElementById('target').style.backgroundImage='paint(tone)'");
        frame(&mut engine,&realm,1000.0);
        assert_eq!(pixel(&pixels(&realm),2,2),[0,0,255,255]);
        eval(&mut engine,"CSS.registerProperty({name:'--tone',syntax:'<color>',initialValue:'green',inherits:false})");
        frame(&mut engine,&realm,1016.0);
        assert_eq!(pixel(&pixels(&realm),2,2),[0,128,0,255]);
    }
    #[test]
    fn paint_worklet_typed_arguments_alpha_and_module_top_level_await() {
        let (mut engine,realm)=setup("await Promise.resolve();registerPaint('args',class{static get inputArguments(){return ['<number>','<color>']}static get contextOptions(){return {alpha:false}}paint(ctx,size,properties,args){ctx.fillStyle=args[1].toString();ctx.fillRect(0,0,args[0].value,size.height)}})");
        eval(&mut engine,"document.getElementById('target').style.backgroundImage='paint(args, 8, green)'");
        frame(&mut engine,&realm,1000.0);
        let image=pixels(&realm);assert_eq!(pixel(&image,2,2),[0,128,0,255]);assert_eq!(pixel(&image,12,12),[0,0,0,255]);
    }
    #[test]
    fn paint_worklet_retained_pixels_hold_budget_until_actual_reclamation() {
        let (mut engine,realm)=setup("registerPaint('solid',class{paint(ctx,size){ctx.fillStyle='green';ctx.fillRect(0,0,size.width,size.height)}})");
        let state=service(engine.ctx()).unwrap();let budget=state.borrow().pixels.clone();
        eval(&mut engine,"document.getElementById('target').style.backgroundImage='paint(solid)'");
        frame(&mut engine,&realm,1000.0);
        let retained=realm.with_session(|session|session.display_list(32,32,crate::canvas::canvas_fallback_fonts()).unwrap().clone());
        assert!(budget.reserved()>=1024);
        retire_document(engine.ctx(),&realm).unwrap();engine.collect_garbage();engine.collect_garbage();
        assert_eq!(pixel(&lumen_html_image::render(&retained,32,32,1.0,false).unwrap(),2,2),[0,128,0,255]);
        assert!(budget.reserved()>=1024,"retained real pixels remain reserved");
        drop(retained);engine.collect_garbage();engine.collect_garbage();
        assert_eq!(budget.reserved(),0);
    }
}
