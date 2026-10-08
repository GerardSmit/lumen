use std::rc::Rc;
use lumen::embed::{Ctx, OpResult};
use lumen_html::NodeId;
use crate::{DomRealm,script_loading,browsing_context};
impl DomRealm {
    pub fn script_has_integrity_attribute(&self,node:NodeId)->bool {
        self.session.borrow().document().get_attribute_ns_ref(node,None,"integrity").ok().flatten().is_some()
    }
    /// Prepare and register an actual inline import-map script in its own Window settings.
    pub fn process_import_map_script(self:&Rc<Self>,ctx:&mut Ctx,node:NodeId)->OpResult<()> {
        if self.script_started(node) {return Ok(());}
        let kind={let session=self.session.borrow();script_loading::prepare_kind(session.document(),node,self.is_html_document)};
        match kind {
            Some(script_loading::ScriptKind::ImportMapInline(source))=>self.execute_prepared_import_map(ctx,node,source),
            Some(script_loading::ScriptKind::InvalidSource)=>{self.mark_script_started(node);self.queue_script_preparation_error(ctx,node)},
            _=>Ok(()),
        }
    }
    pub(crate) fn execute_prepared_import_map(self:&Rc<Self>,ctx:&mut Ctx,node:NodeId,source:String)->OpResult<()> {
        if self.script_started(node) {return Ok(());}
        let Some(context)=self.browsing_context().filter(|context|browsing_context::is_active_document(context,self)) else {return Ok(());};
        if !self.session.borrow().document().scripting_enabled() {self.mark_script_started(node);return Ok(());}
        let parser_inserted=self.scripts.borrow().parser_inserted(node);
        if !self.prepare_script_csp(ctx,node,&source,None,parser_inserted)? {self.mark_script_started(node);return Ok(());}
        let base=self.base_url();
        self.mark_script_started(node);
        let result=lumen_common::import_maps::ImportMap::parse(&source,&base);
        let handle=browsing_context::context_realm_handle(&context);
        ctx.with_host_realm(&handle,|ctx| {
            let result=result.and_then(|parsed|ctx.ensure_import_map_for_host().lock()
                .map_err(|_|lumen_common::import_maps::Error::Type("import-map settings are unavailable".into()))?.register(parsed));
            if let Err(error)=result {
                let name=match &error {lumen_common::import_maps::Error::Syntax(_)=>"SyntaxError",lumen_common::import_maps::Error::Type(_)=>"TypeError",lumen_common::import_maps::Error::Limit=>"QuotaExceededError"};
                let exception=ctx.make_error(name,error.to_string());
                Self::report_exception(ctx,exception);
            }
        }).map_err(browsing_context::host_realm_error)?;
        Ok(())
    }
}
