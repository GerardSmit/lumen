//! `Document.execCommand` and the `queryCommand*` methods. The command algorithms are the W3C
//! Editing APIs reference implementation in `editing_commands.js`, instantiated once per
//! document inside the document's own realm and cached on the `Document`.
use super::*;

const SOURCE: &str = include_str!("editing_commands.js");

fn instance(document: &DomDocument, ctx: &mut Ctx) -> OpResult<Value> {
    if let Some(instance) = document.editing.borrow().clone() {
        return Ok(instance);
    }
    let global = ctx.global_object();
    let factory = ctx
        .eval_in_realm(&global, SOURCE)
        .map_err(|abrupt| OpError::thrown(lumen::embed::abrupt_value(abrupt)))?;
    let wrapper = document.realm.wrap(ctx, document.base.id);
    let instance = ctx
        .invoke(factory, Value::Undefined, &[wrapper])
        .map_err(OpError::thrown)?;
    *document.editing.borrow_mut() = Some(instance.clone());
    Ok(instance)
}

pub(crate) fn call(
    document: &DomDocument,
    ctx: &mut Ctx,
    method: &str,
    args: &[Value],
) -> OpResult<Value> {
    let instance = instance(document, ctx)?;
    let function = ctx
        .get_member(&instance, method)
        .map_err(|abrupt| OpError::thrown(lumen::embed::abrupt_value(abrupt)))?;
    ctx.invoke(function, instance, args).map_err(OpError::thrown)
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    struct Mono;
    impl lumen_html::paint::TextShaper for Mono {
        fn shape(&self, text: &str, size: f32) -> Result<lumen_html::paint::ShapedRun, ()> {
            let glyphs: Vec<lumen_html::paint::Glyph> = text
                .char_indices()
                .map(|(at, _)| lumen_html::paint::Glyph {
                    id: 1,
                    face: 1,
                    cluster: at as u32,
                    caps_expansion: 0, x_offset: 0.0,
                    x: at as f32 * size * 0.5,
                    y: 0.0,
                    size_scale: 1.0,
                })
                .collect();
            let width = glyphs.len() as f32 * size * 0.5;
            Ok(lumen_html::paint::ShapedRun { glyphs: glyphs.into(), width })
        }
        fn ascent(&self, size: f32) -> f32 {
            size * 0.8
        }
        fn line_height(&self, size: f32) -> f32 {
            size * 1.2
        }
    }

    fn run(html: &str, script: &str) {
        let mut engine = lumen::Engine::new();
        let realm = crate::install(engine.ctx(), html, 4096).unwrap();
        realm.set_layout_flusher(Rc::new(|session| {
            session
                .display_list(400, 300, &Mono)
                .map(|_| ())
                .map_err(|error| format!("{error:?}"))
        }));
        let source = format!(
            r#"(() => {{
                const check = (ok, message) => {{ if (!ok) throw Error(message); }};
                const host = document.getElementById('e');
                const select = (a, ao, b, bo) => {{
                    const range = document.createRange();
                    range.setStart(a, ao);
                    range.setEnd(b, bo);
                    const selection = document.getSelection();
                    selection.removeAllRanges();
                    selection.addRange(range);
                }};
                {script}
                return true;
            }})()"#
        );
        let result = engine.eval_value(&source).unwrap();
        let result = result.unwrap_or_else(|error| {
            let ctx = engine.ctx();
            let shown = ctx.member_get(&error, "stack").ok().filter(|stack| !matches!(stack, crate::Value::Undefined)).unwrap_or(error);
            panic!("editing commands: {}", ctx.coerce_string(&shown).map(|value| value.to_string()).unwrap_or_default())
        });
        assert!(matches!(result, crate::Value::Bool(true)));
    }

    #[test]
    fn supported_table_and_methods_exist_on_every_document() {
        run(
            "<div id=e contenteditable>foo</div>",
            r#"
            const supported = ['bold', 'italic', 'underline', 'strikethrough', 'subscript', 'superscript',
                'forecolor', 'hilitecolor', 'backcolor', 'fontname', 'fontsize', 'createlink', 'unlink',
                'removeformat', 'formatblock', 'indent', 'outdent', 'insertorderedlist', 'insertunorderedlist',
                'insertparagraph', 'insertlinebreak', 'inserttext', 'inserthtml', 'insertimage',
                'inserthorizontalrule', 'delete', 'forwarddelete', 'justifyleft', 'justifycenter',
                'justifyright', 'justifyfull', 'selectall', 'stylewithcss', 'usecss', 'defaultparagraphseparator'];
            for (const name of supported) {
                check(document.queryCommandSupported(name), name + ' supported');
                check(document.queryCommandSupported(name.toUpperCase()), name + ' is case-insensitive');
            }
            check(!document.queryCommandSupported('nonexistent'), 'unknown command');
            const other = document.implementation.createHTMLDocument('x');
            for (const method of ['execCommand', 'queryCommandEnabled', 'queryCommandIndeterm',
                'queryCommandState', 'queryCommandSupported', 'queryCommandValue']) {
                check(typeof document[method] === 'function' && typeof other[method] === 'function', method);
            }
            check(document.execCommand('nonexistent') === false, 'unknown command execCommand');
            "#,
        );
    }

    #[test]
    fn commands_never_throw_for_collapsed_and_range_selections() {
        run(
            "<div id=e contenteditable><p>foo</p><p>bar</p></div>",
            r#"
            const names = ['bold', 'italic', 'underline', 'strikethrough', 'subscript', 'superscript',
                'forecolor', 'hilitecolor', 'backcolor', 'fontname', 'fontsize', 'createlink', 'unlink',
                'removeformat', 'formatblock', 'indent', 'outdent', 'insertorderedlist', 'insertunorderedlist',
                'insertparagraph', 'insertlinebreak', 'inserttext', 'inserthtml', 'insertimage',
                'inserthorizontalrule', 'delete', 'forwarddelete', 'justifyleft', 'justifycenter',
                'justifyright', 'justifyfull', 'stylewithcss', 'defaultparagraphseparator'];
            const values = { forecolor: 'red', hilitecolor: 'blue', backcolor: 'blue', fontname: 'serif',
                fontsize: '3', createlink: 'http://example.com/', formatblock: '<p>', inserttext: 'x',
                inserthtml: '<i>y</i>', insertimage: 'http://example.com/a.png', stylewithcss: 'true',
                defaultparagraphseparator: 'p' };
            const points = [
                () => { const t = host.firstChild.firstChild; select(t, 1, t, 1); },
                () => select(host.firstChild.firstChild, 1, host.lastChild.firstChild, 2),
            ];
            for (const place of points) {
                for (const name of names) {
                    host.innerHTML = '<p>foo</p><p>bar</p>';
                    place();
                    document.execCommand(name, false, values[name] || '');
                    document.queryCommandEnabled(name);
                    document.queryCommandIndeterm(name);
                    document.queryCommandState(name);
                    document.queryCommandValue(name);
                }
            }
            "#,
        );
    }

    #[test]
    fn bold_wraps_selected_text() {
        run(
            "<div id=e contenteditable>foobar</div>",
            r#"
            const text = host.firstChild;
            select(text, 0, text, 3);
            check(document.execCommand('bold') === true, 'bold returns true');
            check(host.innerHTML === '<b>foo</b>bar', 'bold output: ' + host.innerHTML);
            check(document.queryCommandState('bold') === true, 'bold state');
            "#,
        );
    }

    #[test]
    fn justifycenter_aligns_the_block() {
        run(
            "<div id=e contenteditable><p>foobar</p></div>",
            r#"
            const text = host.firstChild.firstChild;
            select(text, 1, text, 3);
            check(document.execCommand('justifycenter') === true, 'justifycenter returns true');
            check(/center/.test(host.innerHTML), 'justifycenter output: ' + host.innerHTML);
            check(document.queryCommandState('justifycenter') === true, 'justifycenter state');
            "#,
        );
    }

    #[test]
    fn delete_across_text_nodes_joins_the_remaining_text() {
        run(
            "<div id=e contenteditable>ab<i>cd</i></div>",
            r#"
            const first = host.firstChild, second = host.lastChild.firstChild;
            select(first, 1, second, 1);
            check(document.execCommand('delete') === true, 'delete returns true');
            check(host.textContent === 'ad', 'delete output: ' + host.innerHTML);
            "#,
        );
    }

    #[test]
    fn insertparagraph_splits_text_in_the_middle() {
        run(
            "<div id=e contenteditable><p>foobar</p></div>",
            r#"
            const text = host.firstChild.firstChild;
            select(text, 3, text, 3);
            check(document.execCommand('insertparagraph') === true, 'insertparagraph returns true');
            check(host.innerHTML === '<p>foo</p><p>bar</p>', 'insertparagraph output: ' + host.innerHTML);
            "#,
        );
    }
}
