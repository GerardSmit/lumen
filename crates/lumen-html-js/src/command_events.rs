//! Native HTML CommandEvent and its DOM-retargeted source association.
use super::*;
use events::DomEvent;
use lumen::embed::JsHost;
use lumen_bind::{CtorRet, Host, This};

#[lumen_bind::class(name = "CommandEvent", extends = DomEvent, hint(js(webidl)))]
pub(crate) struct DomCommandEvent {
    base: DomEvent,
    command: String,
    source_slot: Option<String>,
}

struct CommandConstructor { event: DomCommandEvent, source: Option<Value> }
impl CommandConstructor {
    fn into_instance(self, ctx: &mut Ctx) -> Result<Value, Value> {
        let slot = self.event.source_slot.clone();
        let instance = ctx.new_instance(self.event);
        if let (Some(slot), Some(source)) = (slot, self.source) { ctx.define_native_private_value_slot(&instance, &slot, source)?; }
        Ok(instance)
    }
}
impl CtorRet<JsHost, DomCommandEvent> for CommandConstructor {
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
impl DomCommandEvent {
    #[constructor(coerce)]
    fn new(ctx: &mut Ctx, kind: &str, options: Option<Value>) -> OpResult<CommandConstructor> {
        let base = DomEvent::new(ctx, kind, options.clone())?;
        let command = ui_events::dictionary_string(ctx, &options, "command", "", false)?;
        let source = ui_events::dictionary_member(ctx, &options, "source")?.unwrap_or(Value::Null);
        let source = element_reflection::nullable_element(ctx, source)?;
        Ok(construct(ctx, base, command, source))
    }
    #[getter]
    fn command(&self) -> String { self.command.clone() }
    #[getter]
    fn source(&self, ctx: &mut Ctx, this: This<Value>) -> Value {
        let Some(source) = self.source_slot.as_deref().and_then(|slot| ctx.native_private_value_slot(&this.0, slot)) else { return Value::Null; };
        element_reflection::retarget_event_source(ctx, source, self.base.active_current_target())
    }
}

fn construct(ctx: &mut Ctx, base: DomEvent, command: String, source: Option<Value>) -> CommandConstructor {
    let source_slot = source.as_ref().map(|_| ctx.allocate_native_private_slot_name());
    CommandConstructor { event: DomCommandEvent { base, command, source_slot }, source }
}

pub(crate) fn dispatch(ctx: &mut Ctx, realm: &Rc<DomRealm>, target: NodeId, command: &str, source: Value) -> OpResult<bool> {
    let base = DomEvent::from_init("command", lumen_host::events::EventInit { cancelable: true, ..Default::default() });
    let event = construct(ctx, base, command.to_owned(), Some(source)).into_instance(ctx).map_err(OpError::thrown)?;
    let target_value = realm.wrap(ctx, target);
    realm.dispatch_event_to_target(ctx, target, target_value, event, true)
}
