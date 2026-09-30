use lumen_runtime::{Completion, Runtime};

#[test]
fn client_resource_timing_records_real_measurements_and_delivers_observers() {
    let mut runtime = Runtime::new();
    let source = r#"
      const {performance:p, PerformanceResourceTiming, PerformanceObserver}=require('node:perf_hooks');
      let observed=[];
      const observer=new PerformanceObserver(list=>observed.push(...list.getEntries().map(e=>e.name)));
      observer.observe({entryTypes:['resource']});
      p.setResourceTimingBufferSize(1);
      const t={startTime:10,endTime:20,postRedirectStartTime:11,encodedBodySize:50,decodedBodySize:70,
        finalConnectionTimingInfo:{connectionStartTime:12,connectionEndTime:13,ALPNNegotiatedProtocol:'h2'}};
      const mark=p.markResourceTiming;
      const a=mark(t,'https://example.com/a','fetch',globalThis,'',{},201);
      const b=mark(t,'https://example.com/b','fetch',globalThis,'local',{},200);
      globalThis.result=[a instanceof PerformanceResourceTiming,a.duration,a.connectStart,a.nextHopProtocol,
        a.transferSize,b.transferSize,a.responseStatus,a.deliveryType,a.toJSON().encodedBodySize,
        p.getEntriesByType('resource').length];
      queueMicrotask(()=>{result.push(observed);p.clearResourceTimings();result.push(p.getEntriesByType('resource').length);observer.disconnect()});
    "#;
    assert!(matches!(
        runtime.eval(source).expect("parse"),
        Completion::Value(_)
    ));
    runtime.run_to_completion();
    match runtime.eval("JSON.stringify(result)").expect("result") {
        Completion::Value(value) => assert_eq!(
            value,
            r#"[true,10,12,"h2",350,0,201,"",50,1,["https://example.com/a","https://example.com/b"],0]"#
        ),
        Completion::Throw { name, message } => panic!("{name}: {message}"),
    }
}
