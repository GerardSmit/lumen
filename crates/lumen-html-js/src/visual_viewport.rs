//! `window.visualViewport`: the part of the layout viewport a person sees.
//!
//! Without a host that reports otherwise it is the layout viewport. A host
//! with an on-screen keyboard that resizes only the visual viewport (Chrome
//! on Android's `interactive-widget=resizes-visual`) reports its offset and
//! height with [`set_visual_viewport`], which fires `resize` and `scroll`.
use super::*;
use crate::realm_services::RealmServices;
use lumen::embed::JsObject;
use lumen_bind::This;
use std::{cell::Cell, rc::Weak};

const SLOT: &str = "#lumen_window\u{1}visualViewport";

pub(crate) struct VisualViewportState {
    realm: Weak<DomRealm>,
    target: DomEventTarget,
    offset_top: Cell<f64>,
    height: Cell<Option<f64>>,
}

#[lumen_bind::class(name = "VisualViewport", extends = DomEventTarget, hint(js(webidl)))]
pub struct DomVisualViewport {
    base: DomEventTarget,
    state: Rc<VisualViewportState>,
}

impl DomVisualViewport {
    fn layout_size(&self) -> OpResult<(f64, f64)> {
        let Some(realm) = self.state.realm.upgrade() else { return Ok((0.0, 0.0)) };
        if realm.layout_flusher.borrow().is_none() && realm.session.borrow().viewport_size().is_none() {
            return Ok((0.0, 0.0));
        }
        realm.flush_layout()?;
        let (width, height) = realm.session.borrow().viewport_size().unwrap_or((0, 0));
        Ok((f64::from(width), f64::from(height)))
    }

    fn scroll(&self) -> OpResult<(f64, f64)> {
        let Some(realm) = self.state.realm.upgrade() else { return Ok((0.0, 0.0)) };
        let root = realm.session.borrow().document().root();
        scrolling::position(&realm, root)
    }
}

#[lumen_bind::methods]
impl DomVisualViewport {
    #[getter(name = "offsetLeft")]
    fn offset_left(&self) -> f64 {
        0.0
    }

    #[getter(name = "offsetTop")]
    fn offset_top(&self) -> f64 {
        self.state.offset_top.get()
    }

    #[getter(name = "pageLeft")]
    fn page_left(&self) -> OpResult<f64> {
        Ok(self.scroll()?.0)
    }

    #[getter(name = "pageTop")]
    fn page_top(&self) -> OpResult<f64> {
        Ok(self.scroll()?.1 + self.state.offset_top.get())
    }

    #[getter]
    fn width(&self) -> OpResult<f64> {
        Ok(self.layout_size()?.0)
    }

    #[getter]
    fn height(&self) -> OpResult<f64> {
        match self.state.height.get() {
            Some(height) => Ok(height),
            None => Ok(self.layout_size()?.1),
        }
    }

    #[getter]
    fn scale(&self) -> f64 {
        1.0
    }

    #[getter]
    fn onresize(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.base.handler_value(ctx, &this.0, "resize")
    }

    #[setter]
    fn set_onresize(&self, ctx: &mut Ctx, this: This<Value>, callback: crate::events::EventHandler) {
        self.base.set_event_handler(ctx, &this.0, "resize", callback);
    }

    #[getter]
    fn onscroll(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.base.handler_value(ctx, &this.0, "scroll")
    }

    #[setter]
    fn set_onscroll(&self, ctx: &mut Ctx, this: This<Value>, callback: crate::events::EventHandler) {
        self.base.set_event_handler(ctx, &this.0, "scroll", callback);
    }
}

/// Creates `window.visualViewport` for a document realm.
pub(crate) fn install(ctx: &mut Ctx, realm: &Rc<DomRealm>) -> OpResult<()> {
    let constructor = ctx.class_constructor::<DomVisualViewport>();
    let global = ctx.global_object();
    crate::install_interface(ctx, &global, "VisualViewport", constructor)
        .map_err(|_| OpError::new("Error", "VisualViewport installation failed"))?;
    let state = Rc::new(VisualViewportState {
        realm: Rc::downgrade(realm),
        target: DomEventTarget::independent(realm),
        offset_top: Cell::new(0.0),
        height: Cell::new(None),
    });
    let value = ctx.new_instance(DomVisualViewport {
        base: DomEventTarget::from_data(state.target.data_handle()),
        state: state.clone(),
    });
    ctx.set_native_identity_owner::<DomEventTarget>(&value)?;
    ctx.set_native_internal_value_slot(&global, SLOT, value).map_err(OpError::thrown)?;
    RealmServices::replace_shared_current(ctx, state);
    Ok(())
}

/// The `visualViewport` object of the window whose global is `global`.
pub(crate) fn value(ctx: &mut Ctx, global: &Value) -> Value {
    ctx.native_private_value_slot(global, SLOT).unwrap_or(Value::Undefined)
}

/// Reports where the visible part of the page is: `offset_top` CSS pixels
/// below the top of the layout viewport and `height` tall. Fires `resize`
/// when the height changed and `scroll` when the offset did.
pub fn set_visual_viewport(ctx: &mut Ctx, offset_top: f64, height: f64) -> OpResult<()> {
    let Some(state) = RealmServices::<VisualViewportState>::current(ctx) else { return Ok(()) };
    if !offset_top.is_finite() || !height.is_finite() {
        return Err(OpError::type_error("visual viewport values must be finite"));
    }
    let layout_height = state
        .realm
        .upgrade()
        .and_then(|realm| realm.session.borrow().viewport_size())
        .map(|size| f64::from(size.1));
    let previous_height = state.height.replace(Some(height)).or(layout_height).unwrap_or(height);
    let previous_offset = state.offset_top.replace(offset_top);
    let global = ctx.global_object();
    let wrapper = value(ctx, &global);
    if !matches!(wrapper, Value::Obj(_)) {
        return Ok(());
    }
    for (kind, changed) in [("resize", previous_height != height), ("scroll", previous_offset != offset_top)] {
        if !changed {
            continue;
        }
        let options = Value::Obj(ctx.new_object());
        ctx.set_member(&options, "bubbles", Value::Bool(false))
            .and_then(|_| ctx.set_member(&options, "cancelable", Value::Bool(false)))
            .map_err(|_| OpError::new("TypeError", "visual viewport event setup failed"))?;
        let event = DomEvent::new(ctx, kind, Some(options))?;
        let event = ctx.new_instance(event);
        if let Some(event) = JsObject::from_value(event) {
            crate::events::dispatch_user_agent_event(ctx, This(wrapper.clone()), event)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_html::paint::{ShapedRun, TextShaper};

    struct NoText;
    impl TextShaper for NoText {
        fn shape(&self, _: &str, _: f32) -> Result<ShapedRun, ()> {
            Err(())
        }
        fn ascent(&self, size: f32) -> f32 {
            size * 0.8
        }
        fn line_height(&self, size: f32) -> f32 {
            size * 1.2
        }
    }

    fn eval(engine: &mut lumen::Engine, source: &str) -> Value {
        match engine.eval_value(source) {
            Ok(Ok(value)) => value,
            _ => panic!("script failed: {source}"),
        }
    }

    #[test]
    fn the_visual_viewport_follows_the_layout_viewport_until_the_host_reports_otherwise() {
        let mut engine = lumen::Engine::new();
        let realm = crate::install(engine.ctx(), "<style>body{margin:0}</style><div style='height:600px'></div>", 128).unwrap();
        realm.set_layout_flusher(Rc::new(|session| session.display_list(80, 200, &NoText).map(|_| ()).map_err(|error| format!("{error:?}"))));
        assert!(matches!(
            eval(
                &mut engine,
                "const vv = window.visualViewport; vv instanceof VisualViewport && vv instanceof EventTarget && vv === visualViewport \
                 && vv.width === 80 && vv.height === 200 && vv.offsetTop === 0 && vv.offsetLeft === 0 && vv.pageTop === 0 && vv.scale === 1"
            ),
            Value::Bool(true)
        ));
        eval(
            &mut engine,
            "globalThis.log = []; visualViewport.addEventListener('resize', e => log.push('resize:' + e.isTrusted + ':' + visualViewport.height)); \
             visualViewport.onscroll = () => log.push('scroll:' + visualViewport.offsetTop + ':' + visualViewport.pageTop); 0",
        );
        set_visual_viewport(engine.ctx(), 0.0, 150.0).unwrap();
        set_visual_viewport(engine.ctx(), 0.0, 150.0).unwrap();
        set_visual_viewport(engine.ctx(), 30.0, 150.0).unwrap();
        assert!(matches!(
            eval(&mut engine, "log.join() === 'resize:true:150,scroll:30:30' && innerHeight === 200 && visualViewport.height === 150 && visualViewport.offsetTop === 30"),
            Value::Bool(true)
        ));
        eval(&mut engine, "scrollTo(0, 100); 0");
        assert!(matches!(eval(&mut engine, "visualViewport.pageTop === scrollY + 30 && visualViewport.offsetTop === 30"), Value::Bool(true)));
        set_visual_viewport(engine.ctx(), 0.0, 200.0).unwrap();
        assert!(matches!(
            eval(&mut engine, "log.slice(2).join() === 'resize:true:200,scroll:0:' + scrollY && matchMedia('(height:200px)').matches"),
            Value::Bool(true)
        ));
    }
}
