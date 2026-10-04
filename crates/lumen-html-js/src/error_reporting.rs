//! HTML exception reporting, shared by script evaluation and event dispatch.
use super::*;
use crate::realm_services::RealmServices;
use lumen::embed::{JsFunction, JsObject, OpError, OpResult};

#[derive(Clone)]
struct Reporter {
    realm: std::rc::Weak<DomRealm>,
    constructor: Value,
    legacy_constructors: HashMap<&'static str, Value>,
    console: Option<(Value, JsFunction)>,
    active: Rc<Cell<bool>>,
}

pub(crate) fn install(ctx: &mut Ctx, realm: &Rc<DomRealm>) -> OpResult<()> {
    let global = ctx.global_object();
    let constructor = ctx.class_constructor::<ui_events::DomErrorEvent>();
    let mut legacy_constructors = HashMap::new();
    for name in [
        "BeforeUnloadEvent",
        "CompositionEvent",
        "CustomEvent",
        "DeviceMotionEvent",
        "DeviceOrientationEvent",
        "DragEvent",
        "FocusEvent",
        "HashChangeEvent",
        "KeyboardEvent",
        "MessageEvent",
        "MouseEvent",
        "StorageEvent",
        "TextEvent",
        "TouchEvent",
        "UIEvent",
    ] {
        if let Ok(constructor) = ctx.member_get(&global, name) {
            if constructor.is_callable() {
                legacy_constructors.insert(name, constructor);
            }
        }
    }
    let console = ctx.member_get(&global, "console").ok().and_then(|console| {
        let report = ctx.member_get(&console, "error").ok()?;
        Some((console, JsFunction::from_value(report)?))
    });
    RealmServices::replace_current(
        ctx,
        Reporter {
            realm: Rc::downgrade(realm),
            constructor,
            legacy_constructors,
            console,
            active: Rc::new(Cell::new(false)),
        },
    );
    let function = ctx.bound_function(&lumen_bind::FnItem::of::<report_error::Op>());
    ctx.member_set(&global, "reportError", function)
        .map_err(OpError::thrown)?;
    Ok(())
}

/// Require the native ErrorEvent data brand. Prototype lookalikes and
/// author-replaced interface globals do not receive special Window handling.
pub(crate) fn is_error_event(ctx: &mut Ctx, event: &Value) -> bool {
    ui_events::error_event_handler_arguments(ctx, event).is_some()
}

/// Share the realm's existing captured DOMException factory with DOM services.
pub(crate) fn dom_exception(ctx: &mut Ctx, name: &'static str, message: &str) -> OpError {
    let realm = RealmServices::<Reporter>::current(ctx).and_then(|state| state.realm.upgrade());
    font_loading::font_dom_exception(
        ctx,
        realm.as_ref().map(|realm| &realm.font_loading),
        name,
        message,
    )
}

#[lumen_bind::op(name = "reportError")]
fn report_error(ctx: &mut Ctx, exception: Value) {
    report_exception(ctx, exception);
}

pub(crate) fn create_legacy_event(ctx: &mut Ctx, interface: &str) -> OpResult<Value> {
    let name = match interface.to_ascii_lowercase().as_str() {
        "event" | "events" | "htmlevents" | "svgevents" => "Event",
        "beforeunloadevent" => "BeforeUnloadEvent",
        "compositionevent" => "CompositionEvent",
        "customevent" => "CustomEvent",
        "devicemotionevent" => "DeviceMotionEvent",
        "deviceorientationevent" => "DeviceOrientationEvent",
        "dragevent" => "DragEvent",
        "focusevent" => "FocusEvent",
        "hashchangeevent" => "HashChangeEvent",
        "keyboardevent" => "KeyboardEvent",
        "messageevent" => "MessageEvent",
        "mouseevent" | "mouseevents" => "MouseEvent",
        "storageevent" => "StorageEvent",
        "textevent" => "TextEvent",
        "touchevent" => "TouchEvent",
        "uievent" | "uievents" => "UIEvent",
        _ => {
            return Err(dom_exception(
                ctx,
                "NotSupportedError",
                "unknown legacy event interface",
            ));
        }
    };
    if name == "Event" {
        let event = DomEvent::legacy_uninitialized(ctx)?;
        return Ok(ctx.new_instance(event));
    }
    let constructor = RealmServices::<Reporter>::current(ctx)
        .and_then(|state| state.legacy_constructors.get(name).cloned())
        .ok_or_else(|| dom_exception(ctx, "NotSupportedError", "event interface is not exposed"))?;
    let event = ctx
        .construct(constructor, &[Value::str("")])
        .map_err(lumen::embed::abrupt_value)
        .map_err(OpError::thrown)?;
    events::mark_event_uninitialized(ctx, &event)?;
    Ok(event)
}

/// Reporting an exception must not become an exception from dispatchEvent.
pub(crate) fn report_exception(ctx: &mut Ctx, exception: Value) {
    let Some(reporter) = RealmServices::<Reporter>::current(ctx) else {
        return;
    };
    let mut not_handled = true;
    if !reporter.active.replace(true) {
        let result = (|| -> OpResult<bool> {
            let Some(realm) = reporter.realm.upgrade() else {
                return Ok(true);
            };
            let window = realm
                .window_wrapper
                .borrow()
                .as_ref()
                .and_then(WeakValue::upgrade)
                .ok_or_else(|| OpError::new("InvalidStateError", "Window is unavailable"))?;
            let message_value = match &exception {
                Value::Obj(_) => ctx
                    .member_get(&exception, "message")
                    .unwrap_or(Value::Undefined),
                _ => exception.clone(),
            };
            let message = if matches!(message_value, Value::Undefined) {
                "Uncaught exception".to_owned()
            } else {
                ctx.coerce_string(&message_value)
                    .map(|value| value.to_string())
                    .unwrap_or_else(|_| "Uncaught exception".into())
            };
            let options = ctx.new_object_with_proto(&Value::Null);
            for (key, value) in [
                ("cancelable", Value::Bool(true)),
                ("message", Value::str(&message)),
                ("filename", Value::str(&realm.base_url())),
                ("lineno", Value::Num(0.0)),
                ("colno", Value::Num(0.0)),
                ("error", exception.clone()),
            ] {
                ctx.member_set(&options, key, value)
                    .map_err(OpError::thrown)?;
            }
            let event = ctx
                .construct(
                    reporter.constructor.clone(),
                    &[Value::str("error"), options],
                )
                .map_err(lumen::embed::abrupt_value)
                .map_err(OpError::thrown)?;
            let event = JsObject::from_value(event)
                .ok_or_else(|| OpError::type_error("ErrorEvent object required"))?;
            events::dispatch_user_agent_event(ctx, lumen_bind::This(window), event)
        })();
        reporter.active.set(false);
        if let Ok(allowed) = result {
            not_handled = allowed;
        }
    }
    if not_handled {
        if let Some((console, report)) = reporter.console.clone() {
            let _ = report.call(ctx, console, &[exception]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_runtime::Runtime;

    const THROWING_LISTENER: &str = r#"
        (() => {
          const reported = [];
          const original = new Error('realm marker');
          window.addEventListener('error', event => reported.push(event), { once: true });
          const target = document.createElement('div');
          target.addEventListener('probe', () => { throw original; });
          target.dispatchEvent(new Event('probe'));
          return reported.length === 1 && reported[0] instanceof ErrorEvent &&
            reported[0].message === 'realm marker' && reported[0].error === original;
        })()
    "#;

    #[test]
    fn exception_reporting_uses_the_active_host_realms_reporter() {
        let mut runtime = Runtime::new();
        let engine = runtime.engine();
        let ctx = engine.ctx();
        crate::install(ctx, "<main></main>", 64).expect("install parent document");
        let parent = ctx.global_object();
        let parent_realm = ctx.current_host_realm();
        let parent_result = ctx
            .with_host_realm(&parent_realm, |ctx| {
                ctx.eval_in_realm(&parent, THROWING_LISTENER)
            })
            .ok()
            .expect("enter registered parent realm");
        assert!(matches!(parent_result, Ok(Value::Bool(true))));

        let child = ctx.create_host_realm();
        let child_global = child.global();
        ctx.with_host_realm(&child, |ctx| {
            crate::install(ctx, "<main></main>", 64).expect("install child document");
            let child_realm = ctx.current_host_realm();
            let child_result = ctx
                .with_host_realm(&child_realm, |ctx| {
                    ctx.eval_in_realm(&child_global, THROWING_LISTENER)
                })
                .ok()
                .expect("enter registered child realm");
            assert!(matches!(child_result, Ok(Value::Bool(true))));
        })
        .ok()
        .expect("enter child realm");

        let parent_realm = ctx.current_host_realm();
        let parent_result = ctx
            .with_host_realm(&parent_realm, |ctx| {
                ctx.eval_in_realm(&parent, THROWING_LISTENER)
            })
            .ok()
            .expect("re-enter registered parent realm");
        assert!(matches!(parent_result, Ok(Value::Bool(true))));
    }
}
