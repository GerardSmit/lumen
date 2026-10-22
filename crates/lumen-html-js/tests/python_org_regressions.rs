use lumen::embed::Value;

fn check(source: &str) {
    let mut runtime = lumen_runtime::Runtime::new_browser();
    let engine = runtime.engine();
    let _realm = lumen_html_js::install(engine.ctx(), "<!doctype html><main></main>", 512).unwrap();
    let result = engine.eval_value(source).unwrap().ok().expect("guard threw");
    assert!(matches!(result, Value::Bool(true)));
}

#[test]
fn desktop_window_does_not_advertise_touch_events() {
    check(
        r#"(() => {
            for (const name of ['ontouchstart', 'ontouchend', 'ontouchmove', 'ontouchcancel']) {
                if (name in window || name in document.body) throw new Error(name);
            }
            return !window.DocumentTouch;
        })()"#,
    );
}

#[test]
fn svg_root_creates_svg_rects_for_feature_detection() {
    check(
        r#"(() => {
            const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
            const rect = svg.createSVGRect();
            return typeof svg.createSVGRect === 'function' && rect instanceof SVGRect &&
                rect.x === 0 && rect.y === 0 && rect.width === 0 && rect.height === 0;
        })()"#,
    );
}
