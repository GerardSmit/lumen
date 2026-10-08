//! Browser timer settings and classic-script execution over the shared heap.
use super::*;

pub(crate) fn install(ctx: &mut Ctx) -> OpResult<()> {
    lumen_timers::install_browser(ctx, lumen_timers::BrowserHooks {
        capture, eligible, script: execute,
        report: |ctx, error| DomRealm::report_browser_exception(ctx, error).is_some(),
    })
}

fn capture(ctx: &mut Ctx, source: &str, repeat: bool) -> OpResult<Rc<lumen::ClassicScriptContext>> {
    let realm = window_globals::current_dom_realm(ctx)
        .ok_or_else(|| OpError::new("InvalidStateError", "Window timer settings unavailable"))?;
    realm.prepare_timer_string(ctx, source, repeat)?;
    Ok(ctx.current_classic_script_context().unwrap_or_else(|| Rc::new(lumen::ClassicScriptContext {
        base_url: realm.base_url(), credentials_mode: "same-origin".into(), ..Default::default()
    })))
}

fn eligible(ctx: &mut Ctx) -> bool {
    window_globals::current_dom_realm(ctx).is_some_and(|realm| {
        realm.has_browsing_context && !realm.lifecycle.sandboxed_automatic_features.get()
            && realm.browsing_context().is_some_and(|context| browsing_context::is_active_document(&context, &realm))
    })
}

fn execute(ctx: &mut Ctx, source: &str, context: Rc<lumen::ClassicScriptContext>) -> Result<(), Value> {
    let realm = window_globals::current_dom_realm(ctx)
        .ok_or_else(|| ctx.make_error("InvalidStateError", "Window timer settings unavailable"))?;
    realm.prepare_timer_compilation(ctx, source).map_err(|error| error.to_value(ctx))?;
    let _current = realm.enter_script(None);
    ctx.run_classic_script(source, context).map(|_| ()).map_err(lumen::embed::abrupt_value)
}

impl DomRealm {
    /// A captured script context contains immutable fetch metadata, not an
    /// element lease; later attribute edits cannot rewrite a pending timer.
    pub fn classic_script_context(&self, node: NodeId, base_url: Option<&str>) -> Rc<lumen::ClassicScriptContext> {
        let session = self.session.borrow();
        let document = session.document();
        let attr = |name| document.get_attribute_ns_ref(node, None, name).ok().flatten().unwrap_or("").to_owned();
        let credentials_mode = if attr("crossorigin").eq_ignore_ascii_case("use-credentials") { "include" } else { "same-origin" };
        let referrer_policy = lumen_common::referrer::ReferrerPolicy::parse(&attr("referrerpolicy"))
            .map_or("", |policy| policy.name()).to_owned();
        Rc::new(lumen::ClassicScriptContext { base_url: base_url.map(str::to_owned).unwrap_or_else(|| self.base_url()),
            nonce: document.cryptographic_nonce(node).unwrap_or("").to_owned(), credentials_mode: credentials_mode.into(), referrer_policy })
    }
}
