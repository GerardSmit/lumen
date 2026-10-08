//! Per-realm traced adapter for the shared Performance Timeline. The JS facade only owns
//! WebIDL conversion, entry prototypes and callback dispatch; retained state lives here.
use crate::{Ctx, OpError, Value};
use lumen::embed::OpResult;
use lumen_common::performance::Timeline;
use std::cell::RefCell;

const RESOURCE_PUBLISHER: &str = "#lumen_resource_timeline\u{1}publisher";

#[lumen_bind::module(name = "__performance_timeline")]
pub mod bindings {
    use super::*;

    #[op]
    pub fn install_resource_publisher(ctx:&mut Ctx,callback:lumen::embed::JsFunction)->OpResult<()> {
        let global=ctx.global_object();
        ctx.define_native_internal_value_slot(&global,RESOURCE_PUBLISHER,callback.into_value()).map_err(OpError::thrown)
    }

    #[class(name = "Timeline", hint(js(invalid_this)))]
    pub struct PerformanceTimeline {
        timeline: RefCell<Timeline<Value>>,
    }

    #[op]
    pub fn create(ctx: &mut Ctx) -> OpResult<Value> {
        let value = ctx.new_instance(PerformanceTimeline { timeline: RefCell::new(Timeline::default()) });
        ctx.set_native_identity_owner::<PerformanceTimeline>(&value)?;
        Ok(value)
    }

    #[methods]
    impl PerformanceTimeline {
        /// User Timing uses the host's structured clone algorithm, independently of a
        /// script replacing the public structuredClone function. No transfer list applies.
        fn clone_detail(&self, ctx: &mut Ctx, detail: Value) -> OpResult<Value> {
            crate::structured_clone::structured_clone(ctx, &detail, Vec::new())
        }

        fn add(&self, name: String, kind: String, start: f64, value: Value, retain: bool) -> bool {
            self.timeline.borrow_mut().add(name, kind, start, value, retain)
        }

        fn entries(&self, ctx: &mut Ctx, name: Option<String>, kind: Option<String>) -> Value {
            let entries = self.timeline.borrow().entries(name.as_deref(), kind.as_deref());
            ctx.make_array(entries)
        }

        fn resolve(&self, name: String) -> Option<f64> { self.timeline.borrow().resolve(&name) }

        fn clear(&self, kind: String, name: Option<String>) {
            self.timeline.borrow_mut().clear(&kind, name.as_deref());
        }

        fn observe(&self, id: u32, owner: Value, types: Vec<String>, replace: bool, buffered: bool) -> OpResult<u32> {
            self.timeline.borrow_mut().observe(id, owner, types, replace, buffered)
                .ok_or_else(|| OpError::new("QuotaExceededError", "Performance observer limit exceeded"))
        }

        fn disconnect(&self, id: u32) { self.timeline.borrow_mut().disconnect(id); }
        fn buffer(&self, id: u32, name: String, kind: String, start: f64, value: Value) {
            self.timeline.borrow_mut().buffer(id, name, kind, start, value);
        }
        fn pending(&self, ctx: &mut Ctx) -> Value {
            let pending = self.timeline.borrow_mut().pending();
            ctx.make_array(pending)
        }
        fn take(&self, ctx: &mut Ctx, id: u32) -> Value {
            let entries = self.timeline.borrow_mut().take(id);
            ctx.make_array(entries)
        }
        fn dropped(&self, id: u32) -> f64 { self.timeline.borrow_mut().dropped(id) as f64 }
    }

    impl lumen::embed::NativeIdentityOwner for PerformanceTimeline {
        const TRACES_NATIVE_VALUES: bool = true;
        fn trace_native_identities(&self, _: u64, _: &mut dyn FnMut(&Value)) {}
        fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) {
            self.timeline.borrow().visit(visit);
        }
    }
}

/// Realm-local provider factory. Node's adapter calls the same factory with its error and GC
/// hooks; Browser never loads Node glue or installs process/Buffer/require.
pub const SOURCE: &str = include_str!("performance_timeline.js");

/// Publish through the original realm-local timeline provider. Author changes
/// to public performance methods cannot replace the resource admission path.
pub fn record_resource(ctx:&mut Ctx,name:&str,initiator:&str,start:f64,end:f64,encoded:u64,decoded:u64,timing_allowed:bool)->OpResult<()> {
    let global=ctx.global_object();
    let Some(callback)=ctx.native_private_value_slot(&global,RESOURCE_PUBLISHER) else {return Ok(())};
    let args=[Value::from_string(name.into()),Value::from_string(initiator.into()),Value::Num(start),Value::Num(end),
        Value::Num(encoded as f64),Value::Num(decoded as f64),Value::Bool(timing_allowed)];
    ctx.invoke(callback,Value::Undefined,&args).map_err(OpError::thrown)?;
    Ok(())
}
