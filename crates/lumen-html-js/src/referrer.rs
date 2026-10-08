use super::*;
use lumen_common::referrer::{Referrer, ReferrerPolicy};
use lumen_html::Document;

impl DomRealm {
    pub fn script_fetch_referrer(&self) -> Referrer { self.navigation_referrer() }
    pub fn script_fetch_origin(&self) -> String {
        self.document_origin()
            .or_else(|| self.browsing_context().map(|context| browsing_context::context_origin(&context)))
            .map_or_else(|| "null".to_owned(), |origin| origin.serialize())
    }
    /// Response language fallback is shared with core selectors and text handling.
    pub fn set_content_language_headers(&self, headers: &[(String, String)]) -> OpResult<()> {
        self.session.borrow_mut().document_mut().set_content_language_headers(headers).map_err(dom_error)
    }

    /// Install transport metadata before executing the document parser.
    pub fn set_navigation_referrer(&self, sent: Option<&str>, headers: &[(String, String)]) {
        *self.document_referrer.borrow_mut() = sent.unwrap_or("").to_owned();
        for (name, value) in headers {
            if name.eq_ignore_ascii_case("referrer-policy") {
                if let Some(policy) = ReferrerPolicy::parse_header(value) { self.referrer_policy.set(policy); }
            }
        }
    }

    pub(crate) fn navigation_referrer(&self) -> Referrer {
        let mut source = self.document_url().unwrap_or_else(|| "about:blank".into());
        let mut context = self.browsing_context();
        while source == "about:srcdoc" {
            let Some(parent) = context.and_then(|context| context.parent_context()) else { break; };
            let Some(document) = parent.document() else { break; };
            source = document.document_url().unwrap_or_else(|| "about:blank".into());
            context = Some(parent);
        }
        Referrer { source, policy: self.referrer_policy.get() }
    }
}

fn process_meta(realm: &DomRealm, document: &Document, node: NodeId) {
    if !document.is_connected_element(node)
        || lumen_html::forms::html_element_local_name(document, node) != Some("meta")
        || !document.get_attribute_ns_ref(node, None, "name").ok().flatten()
            .is_some_and(|name| name.eq_ignore_ascii_case("referrer")) { return; }
    let mut root = node;
    while let Ok(Some(parent)) = document.parent(root) { root = parent; }
    if root != document.root() { return; }
    let Some(value) = document.get_attribute_ns_ref(node, None, "content").ok().flatten() else { return; };
    let value = value.to_ascii_lowercase();
    let value = match value.as_str() {
        "never" => "no-referrer", "default" => "strict-origin-when-cross-origin",
        "always" => "unsafe-url", "origin-when-crossorigin" => "origin-when-cross-origin",
        _ => &value,
    };
    if let Some(policy) = ReferrerPolicy::parse(value) { realm.referrer_policy.set(policy); }
}

pub(crate) fn install(realm: &Rc<DomRealm>) {
    let weak = Rc::downgrade(realm);
    realm.add_mutation_sink(Rc::new(move |document, mutation| {
        let Some(realm) = weak.upgrade() else { return; };
        if matches!(&mutation.kind, lumen_html::observe::ObservedKind::Attribute { name, namespace_uri, .. }
            if namespace_uri.is_none() && matches!(name.as_str(), "name" | "content")) {
            process_meta(&realm, document, mutation.target);
        }
        if matches!(&mutation.kind, lumen_html::observe::ObservedKind::ChildListReplacement { .. }) { return; }
        for root in mutation.kind.added_nodes() {
            process_meta(&realm, document, root);
            let mut node = root;
            while let Ok(Some(next)) = lumen_html::selector::next_descendant(document, root, node) {
                process_meta(&realm, document, next);
                node = next;
            }
        }
    }));
    let session = realm.session.borrow();
    let document = session.document();
    let mut node = document.root();
    while let Ok(Some(next)) = lumen_html::selector::next_descendant(document, document.root(), node) {
        process_meta(realm, document, next);
        node = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn specification_window_referrer_metadata_tracks_meta_chronology_and_hyperlink_capture() {
        let mut engine = lumen::Engine::new();
        let realm = crate::install(engine.ctx(), "<!doctype html><meta id=first name=referrer content=origin><meta id=last name=referrer content=no-referrer><a id=link href=https://destination.test/page target=_self></a>", 128).unwrap();
        realm.set_document_url("https://source.test/path?query#fragment");
        realm.set_navigation_referrer(Some("https://previous.test/"), &[]);
        assert_eq!(realm.navigation_referrer().policy, ReferrerPolicy::NoReferrer);
        let result = engine.eval_value(r#"(() => {
            if(document.referrer!=='https://previous.test/')throw new Error('transport referrer not exposed');
            const first=document.getElementById('first'),last=document.getElementById('last');
            last.remove();first.content='unsafe-url';
            const inserted=document.createElement('meta');
            inserted.name='referrer';inserted.content='strict-origin';
            inserted.httpEquiv='refresh';inserted.scheme='legacy';
            if(inserted.getAttribute('http-equiv')!=='refresh' || inserted.getAttribute('scheme')!=='legacy')throw new Error('metadata reflectors must update real attributes');
            document.head.append(inserted);inserted.remove();
            return true;
        })()"#).unwrap().ok().expect("referrer script");
        assert!(matches!(result, Value::Bool(true)));
        assert_eq!(realm.navigation_referrer().policy, ReferrerPolicy::StrictOrigin,
            "removing the most recently inserted meta does not restore earlier policy");
        engine.eval_value("document.getElementById('first').content='unsafe-url';const link=document.getElementById('link');link.referrerPolicy='origin';link.click()").unwrap().ok().expect("referrer capture script");
        assert_eq!(realm.navigation_referrer().policy, ReferrerPolicy::UnsafeUrl);
        let request = realm.browsing_context().unwrap().captured_navigation_metadata();
        assert_eq!(request.referrer.source, "https://source.test/path?query#fragment");
        assert_eq!(request.referrer.policy, ReferrerPolicy::Origin);
        assert!(request.source_element.is_some());
        assert_eq!(request.user_involvement, browsing_context::UserNavigationInvolvement::None);
        engine.eval_value("document.getElementById('first').content='no-referrer';document.getElementById('link').rel='noreferrer';document.getElementById('link').referrerPolicy='unsafe-url';document.getElementById('link').click()").unwrap().ok().expect("noreferrer script");
        assert_eq!(realm.browsing_context().unwrap().captured_navigation_metadata().referrer.policy, ReferrerPolicy::NoReferrer);
        assert_eq!(request.referrer.policy, ReferrerPolicy::Origin, "queued request is immutable after source metadata changes");
    }

    #[test]
    fn specification_window_iframe_referrer_capture_is_generation_qualified_and_srcdoc_inherits_policy() {
        let mut engine = lumen::Engine::new();
        let realm = crate::install(engine.ctx(), "<!doctype html><body></body>", 128).unwrap();
        realm.set_document_url("https://parent.test/path?q#fragment");
        engine.eval_value("const iframe=document.createElement('iframe');iframe.id='child';iframe.src='https://child.test/first';iframe.referrerPolicy='origin';document.body.append(iframe)").unwrap().ok().expect("iframe creation");
        let frame = realm.frame_contexts(engine.ctx()).unwrap().remove(0);
        assert_eq!(&*frame.current_document().unwrap().document_referrer.borrow(), "https://parent.test/path?q#fragment",
            "initial about:blank referrer is direct creator URL serialization, independently of request policy");
        let first = frame.navigation_request();
        assert_eq!(first.metadata.referrer.policy, ReferrerPolicy::Origin);
        engine.eval_value("document.getElementById('child').referrerPolicy='unsafe-url'").unwrap().ok().expect("iframe policy mutation");
        assert!(frame.request_is_current(&first), "changing referrer policy alone does not navigate or mutate an already captured request");
        engine.eval_value("document.getElementById('child').src='https://child.test/second'").unwrap().ok().expect("iframe source mutation");
        let second = frame.navigation_request();
        assert!(!frame.request_is_current(&first));
        assert_eq!(second.metadata.referrer.policy, ReferrerPolicy::UnsafeUrl);
        engine.eval_value("document.getElementById('child').srcdoc='<body>srcdoc</body>'").unwrap().ok().expect("srcdoc mutation");
        let request = frame.navigation_request();
        frame.install_response_for_request(engine.ctx(), &request, "about:srcdoc", "text/html", "<body>srcdoc</body>", 128).unwrap();
        let child = frame.current_document().unwrap();
        assert_eq!(child.navigation_referrer().source, "https://parent.test/path?q#fragment");
        assert_eq!(child.navigation_referrer().policy, ReferrerPolicy::StrictOriginWhenCrossOrigin,
            "the iframe request override does not replace the inherited policy container");
    }
    #[test]
    fn specification_language_http_and_meta_insertion_chronology_use_live_parser_and_selectors() {
        let mut engine = lumen::Engine::new();
        let realm = crate::install_live_html(engine.ctx(), "<!doctype html><meta id=languageMeta http-equiv=content-language content='nl extra'><script>0</script><body><span id=target></span></body>", 128).unwrap();
        realm.set_content_language_headers(&[("Content-Language".into(), "fr".into())]).unwrap();
        assert!(realm.next_document_parser_script(engine.ctx()).unwrap().is_some());
        let check = |engine: &mut lumen::Engine, source: &str| {
            let value = engine.eval_value(source).expect("compile real language guard").ok().expect("execute real language guard");
            assert!(matches!(value, Value::Bool(true)), "language guard: {source}");
        };
        check(&mut engine, "document.documentElement.matches(':lang(nl)')");
        assert!(realm.next_document_parser_script(engine.ctx()).unwrap().is_none());
        check(&mut engine, "globalThis.savedLanguageMeta=languageMeta;savedLanguageMeta.content='de';savedLanguageMeta.remove();target.matches(':lang(nl)')");
        check(&mut engine, "document.head.append(savedLanguageMeta);target.matches(':lang(de)')");
        check(&mut engine, "const rejected=document.createElement('meta');rejected.httpEquiv='content-language';rejected.content='fr, en';document.body.append(rejected);target.matches(':lang(de)')");
        check(&mut engine, "rejected.content='fr';target.matches(':lang(de)')");
        check(&mut engine, "rejected.remove();document.body.prepend(rejected);target.matches(':lang(fr)')");
        check(&mut engine, "document.documentElement.lang='xyzzy';target.matches(':lang(xyzzy)') && !target.matches(':lang(abcde)')");
        check(&mut engine, "document.documentElement.lang='';target.matches(':lang(\"\")') && !target.matches(':lang(fr)')");
        realm.set_content_language_headers(&[("Content-Language".into(), "it".into()), ("content-language".into(), "de".into())]).unwrap();
        check(&mut engine, "document.documentElement.removeAttribute('lang');target.matches(':lang(fr)')");
        let fresh = crate::install(engine.ctx(), "<!doctype html><p id=fallback></p>", 64).unwrap();
        fresh.set_content_language_headers(&[("Content-Language".into(), "it".into())]).unwrap();
        check(&mut engine, "fallback.matches(':lang(it)')");
        fresh.set_content_language_headers(&[("Content-Language".into(), "it,de".into())]).unwrap();
        check(&mut engine, "fallback.matches(':lang(\"\")') && !fallback.matches(':lang(it)')");
    }

}
