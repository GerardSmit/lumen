//! Native InterestEvent and its DOM-retargeted source association.
use super::*;
use events::DomEvent;
use lumen::embed::JsHost;
use lumen_bind::{CtorRet, Host, This};

#[lumen_bind::class(name = "InterestEvent", extends = DomEvent, hint(js(webidl)))]
pub(crate) struct DomInterestEvent {
    base: DomEvent,
    source_slot: Option<String>,
}

struct InterestConstructor { event: DomInterestEvent, source: Option<Value> }
impl CtorRet<JsHost, DomInterestEvent> for InterestConstructor {
    fn into_ctor(self, cx: &<JsHost as Host>::Cx<'_>) -> Result<Value, Value> {
        let slot = self.event.source_slot.clone();
        let instance = <JsHost as Host>::construct(cx, self.event)?;
        <JsHost as Host>::with_ctx(cx, |ctx: &mut Ctx| {
            if let (Some(slot), Some(source)) = (slot, self.source) { ctx.define_native_private_value_slot(&instance, &slot, source)?; }
            Ok(())
        })?;
        Ok(instance)
    }
}

#[lumen_bind::methods]
impl DomInterestEvent {
    #[constructor(coerce)]
    fn new(ctx: &mut Ctx, kind: &str, options: Option<Value>) -> OpResult<InterestConstructor> {
        let base = DomEvent::new(ctx, kind, options.clone())?;
        let source = ui_events::dictionary_member(ctx, &options, "source")?.unwrap_or(Value::Null);
        let source = element_reflection::nullable_element(ctx, source)?;
        let source_slot = source.as_ref().map(|_| ctx.allocate_native_private_slot_name());
        Ok(InterestConstructor { event: DomInterestEvent { base, source_slot }, source })
    }
    #[getter]
    fn source(&self, ctx: &mut Ctx, this: This<Value>) -> Value {
        let Some(source) = self.source_slot.as_deref().and_then(|slot| ctx.native_private_value_slot(&this.0, slot)) else { return Value::Null; };
        element_reflection::retarget_event_source(ctx, source, self.base.active_current_target())
    }
}
