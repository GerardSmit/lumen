//! Focused host regressions for real HTML image request policy preparation.
use super::*;
use lumen_html::layout::ImageState;
use std::sync::Arc;
#[test]
fn csp_frames_report_only_censors_cross_origin_but_allows_navigation() {
    let mut runtime=lumen_runtime::Runtime::new_browser();let engine=runtime.engine();
    let realm=crate::install(engine.ctx(),"<head></head><body></body>",256).unwrap();realm.set_document_url("https://example.test/page");
    realm.set_content_security_policy_headers(&[("Content-Security-Policy-Report-Only".into(),"frame-src 'none'".into())]).unwrap();
    assert!(matches!(engine.eval_value("globalThis.frameReport=null;document.addEventListener('securitypolicyviolation',e=>frameReport=[e.blockedURI,e.effectiveDirective,e.disposition]);"),Ok(Ok(_))));
    assert!(realm.prepare_frame_csp(engine.ctx(),"https://other.test/private/path?q=1").unwrap());assert!(scheduling::run_tasks(runtime.engine(),16).is_empty());
    assert!(matches!(runtime.engine().eval_value("frameReport[0]==='https://other.test' && frameReport[1]==='frame-src' && frameReport[2]==='report'"),Ok(Ok(Value::Bool(true)))));
    realm.set_content_security_policy_headers(&[("Content-Security-Policy".into(),"frame-src 'none'".into())]).unwrap();
    assert!(!realm.prepare_frame_csp(runtime.engine().ctx(),"https://example.test/blocked?q=1").unwrap());assert!(scheduling::run_tasks(runtime.engine(),16).is_empty());
    assert!(matches!(runtime.engine().eval_value("frameReport[0]==='https://example.test/blocked?q=1' && frameReport[2]==='enforce'"),Ok(Ok(Value::Bool(true)))));
}
#[test]
fn csp_images_response_blocks_redirect_and_reports_original_url() {
    struct Redirected;
    impl lumen_html::layout::ImageResolver for Redirected {
        fn resolve(&self,_source:&str)->ImageState{ImageState::Ready(Arc::new(lumen_html::paint::ImageData{width:1,height:1,pixels:vec![0,0,0,255]}))}
        fn response_metadata(&self,_node:NodeId,_base:&str,_source:&str)->Option<lumen_html::layout::ImageResponseMetadata>{Some(lumen_html::layout::ImageResponseMetadata{final_url:"https://denied.test/pixel.png".into(),redirect_count:1})}
    }
    let mut runtime=lumen_runtime::Runtime::new_browser();let engine=runtime.engine();
    let realm=crate::install(engine.ctx(),"<head></head><body></body>",256).unwrap();realm.set_document_url("https://example.test/page");
    realm.set_content_security_policy_headers(&[("Content-Security-Policy".into(),"img-src https://allowed.test".into())]).unwrap();realm.set_image_resolver(Rc::new(Redirected));
    assert!(matches!(engine.eval_value("globalThis.redirectErrors=0;globalThis.redirectReport=null;document.addEventListener('securitypolicyviolation',e=>redirectReport=e.blockedURI);const redirectedImage=new Image();redirectedImage.onerror=()=>redirectErrors++;redirectedImage.src='https://allowed.test/redirect?version=1';"),Ok(Ok(_))));
    realm.queue_image_tasks(engine.ctx()).unwrap();assert!(scheduling::run_tasks(runtime.engine(),16).is_empty());
    assert!(matches!(runtime.engine().eval_value("redirectErrors===1 && redirectReport==='https://allowed.test/redirect?version=1' && redirectedImage.naturalWidth===0"),Ok(Ok(Value::Bool(true)))));
    // A later policy must not retroactively change a request already prepared.
    let mut later_runtime=lumen_runtime::Runtime::new_browser();let engine=later_runtime.engine();
    let later=crate::install(engine.ctx(),"<head></head><body></body>",256).unwrap();later.set_document_url("https://example.test/page");later.set_image_resolver(Rc::new(Redirected));
    assert!(matches!(engine.eval_value("globalThis.oldRequestLoads=0;const oldRequest=new Image();oldRequest.onload=()=>oldRequestLoads++;oldRequest.src='https://allowed.test/request';oldRequest.complete;"),Ok(Ok(_))));
    later.set_content_security_policy_headers(&[("Content-Security-Policy".into(),"img-src 'none'".into())]).unwrap();
    later.queue_image_tasks(engine.ctx()).unwrap();assert!(scheduling::run_tasks(later_runtime.engine(),16).is_empty());
    assert!(matches!(later_runtime.engine().eval_value("oldRequestLoads===1 && oldRequest.naturalWidth===1"),Ok(Ok(Value::Bool(true)))));
}
#[test]
fn csp_images_block_before_provider_and_queue_document_violation_and_error() {
    let mut runtime=lumen_runtime::Runtime::new_browser();let engine=runtime.engine();
    let realm=crate::install(engine.ctx(),"<head></head><body></body>",256).unwrap();
    realm.set_document_url("https://example.test/page");
    realm.set_content_security_policy_headers(&[("Content-Security-Policy".into(),"img-src 'none'".into())]).unwrap();
    let calls=Rc::new(Cell::new(0));let count=calls.clone();
    realm.set_image_resolver(Rc::new(move|_:&str|{count.set(count.get()+1);ImageState::Failed}));
    assert!(matches!(engine.eval_value("globalThis.imageErrors=0;globalThis.imageViolations=[];document.addEventListener('securitypolicyviolation',e=>imageViolations.push([e.violatedDirective,e.target===document,e.blockedURI,e.isTrusted]));const blockedImage=new Image();blockedImage.onerror=()=>imageErrors++;blockedImage.src='/blocked.png';!blockedImage.complete && imageErrors===0 && imageViolations.length===0"),Ok(Ok(Value::Bool(true)))));
    realm.queue_image_tasks(engine.ctx()).unwrap();assert_eq!(calls.get(),0);
    assert!(scheduling::run_tasks(runtime.engine(),16).is_empty());
    assert!(matches!(runtime.engine().eval_value("imageErrors===1 && imageViolations.length===1 && imageViolations[0][0]==='img-src' && imageViolations[0][1] && imageViolations[0][2]==='https://example.test/blocked.png' && imageViolations[0][3] && blockedImage.complete && blockedImage.naturalWidth===0"),Ok(Ok(Value::Bool(true)))));
    realm.queue_image_tasks(runtime.engine().ctx()).unwrap();assert!(scheduling::run_tasks(runtime.engine(),16).is_empty());
    assert!(matches!(runtime.engine().eval_value("imageErrors===1 && imageViolations.length===1"),Ok(Ok(Value::Bool(true)))));
}
#[test]
fn csp_image_report_only_allows_decode_and_new_policy_blocks_available_reuse() {
    let mut runtime=lumen_runtime::Runtime::new_browser();let engine=runtime.engine();
    let realm=crate::install(engine.ctx(),"<head></head><body></body>",256).unwrap();
    realm.set_document_url("https://example.test/page");
    realm.set_content_security_policy_headers(&[("Content-Security-Policy-Report-Only".into(),"img-src 'none'".into())]).unwrap();
    let calls=Rc::new(Cell::new(0));let count=calls.clone();
    realm.set_image_resolver(Rc::new(move|_:&str|{count.set(count.get()+1);ImageState::Ready(Arc::new(lumen_html::paint::ImageData{width:1,height:1,pixels:vec![255,0,0,255]}))}));
    assert!(matches!(engine.eval_value("globalThis.imageLoads=0;globalThis.imageErrors=0;globalThis.imageReports=0;document.addEventListener('securitypolicyviolation',()=>imageReports++);const reusableImage=new Image();reusableImage.onload=()=>imageLoads++;reusableImage.onerror=()=>imageErrors++;reusableImage.src='/cached.png';true"),Ok(Ok(Value::Bool(true)))));
    realm.queue_image_tasks(engine.ctx()).unwrap();assert!(scheduling::run_tasks(runtime.engine(),16).is_empty());
    assert_eq!(calls.get(),1);
    assert!(matches!(runtime.engine().eval_value("imageLoads===1 && imageReports===1 && reusableImage.naturalWidth===1"),Ok(Ok(Value::Bool(true)))));
    realm.set_content_security_policy_headers(&[("Content-Security-Policy".into(),"img-src 'none'".into())]).unwrap();
    assert!(matches!(runtime.engine().eval_value("reusableImage.src='/cached.png';true"),Ok(Ok(Value::Bool(true)))));
    realm.queue_image_tasks(runtime.engine().ctx()).unwrap();assert!(scheduling::run_tasks(runtime.engine(),16).is_empty());
    assert_eq!(calls.get(),1,"blocked cached reuse must not access provider");
    assert!(matches!(runtime.engine().eval_value("imageLoads===1 && imageErrors===1 && imageReports===3 && reusableImage.naturalWidth===0 && reusableImage.complete"),Ok(Ok(Value::Bool(true)))));
}
