use super::*;
use lumen::embed::{JsFunction, JsObject, OpError, OpResult};

fn invoke(ctx: &mut Ctx, object: &Value, name: &str, args: &[Value]) -> OpResult<Value> {
    let function = ctx
        .get_member(object, name)
        .map_err(|_| OpError::new("TypeError", "DOM method lookup failed"))?;
    ctx.invoke(function, object.clone(), args)
        .map_err(OpError::thrown)
}

fn keys(ctx: &mut Ctx, object: &Value) -> OpResult<Vec<String>> {
    let global = ctx.global_object();
    let constructor = ctx
        .get_member(&global, "Object")
        .map_err(|_| OpError::new("Error", "Object missing"))?;
    let keys = invoke(ctx, &constructor, "keys", &[object.clone()])?;
    let count = ctx
        .get_member(&keys, "length")
        .map_err(|_| OpError::new("Error", "object keys failed"))?;
    let Value::Num(count) = count else {
        return Err(OpError::new("TypeError", "invalid keys list"));
    };
    let mut names = Vec::with_capacity(count as usize);
    for index in 0..count as usize {
        let value = ctx
            .get_member(&keys, &index.to_string())
            .map_err(|_| OpError::new("Error", "object keys failed"))?;
        names.push(
            ctx.coerce_string(&value)
                .map_err(OpError::thrown)?
                .to_string(),
        );
    }
    Ok(names)
}

fn children(ctx: &mut Ctx, parent: &Value, value: Value, depth: usize) -> OpResult<()> {
    if depth > 256 {
        return Err(OpError::new(
            "RangeError",
            "JSX child nesting limit exceeded",
        ));
    }
    if matches!(value, Value::Null | Value::Undefined | Value::Bool(_)) {
        return Ok(());
    }
    if ctx.is_array_value(&value).map_err(OpError::thrown)? {
        let count = ctx
            .get_member(&value, "length")
            .map_err(|_| OpError::new("Error", "JSX children length failed"))?;
        let Value::Num(count) = count else {
            return Err(OpError::new("TypeError", "invalid JSX children"));
        };
        for index in 0..count as usize {
            let child = ctx
                .get_member(&value, &index.to_string())
                .map_err(|_| OpError::new("Error", "JSX child read failed"))?;
            children(ctx, parent, child, depth + 1)?;
        }
        return Ok(());
    }
    let child = if matches!(value, Value::Str(_) | Value::Num(_)) {
        let global = ctx.global_object();
        let document = ctx
            .get_member(&global, "document")
            .map_err(|_| OpError::new("Error", "document missing"))?;
        let text = ctx.coerce_string(&value).map_err(OpError::thrown)?;
        invoke(ctx, &document, "createTextNode", &[Value::str(text)])?
    } else {
        value
    };
    invoke(ctx, parent, "appendChild", &[child])?;
    Ok(())
}

pub(crate) fn fragment(ctx: &mut Ctx, props: Option<JsObject>) -> OpResult<Value> {
    let global = ctx.global_object();
    let document = ctx
        .get_member(&global, "document")
        .map_err(|_| OpError::new("Error", "document missing"))?;
    let fragment = invoke(ctx, &document, "createDocumentFragment", &[])?;
    if let Some(props) = props {
        let value = props.get(ctx, "children")?;
        children(ctx, &fragment, value, 0)?;
    }
    Ok(fragment)
}

pub(crate) fn apply_style(ctx: &mut Ctx, element: &Value, value: &Value) -> OpResult<()> {
    let style = ctx
        .get_member(element, "style")
        .map_err(|_| OpError::new("Error", "style missing"))?;
    invoke(ctx, element, "removeAttribute", &[Value::str("style")])?;
    for property in keys(ctx, value)? {
        let field = ctx
            .get_member(value, &property)
            .map_err(|_| OpError::new("Error", "style property read failed"))?;
        let mut name = String::new();
        if property.starts_with("--") {
            name.push_str(&property);
        } else {
            for character in property.chars() {
                if character.is_ascii_uppercase() {
                    name.push('-');
                    name.push(character.to_ascii_lowercase());
                } else {
                    name.push(character);
                }
            }
        }
        let mut text = ctx
            .coerce_string(&field)
            .map_err(OpError::thrown)?
            .to_string();
        if matches!(field, Value::Num(_))
            && matches!(
                name.as_str(),
                "width"
                    | "height"
                    | "margin"
                    | "padding"
                    | "font-size"
                    | "border-width"
                    | "border-radius"
            )
        {
            text.push_str("px");
        }
        invoke(
            ctx,
            &style,
            "setProperty",
            &[Value::str(name), Value::str(text)],
        )?;
    }
    Ok(())
}

pub(crate) fn jsx(ctx: &mut Ctx, tag: Value, props: Option<JsObject>) -> OpResult<Value> {
    if let Some(component) = JsFunction::from_value(tag.clone()) {
        return component.call(
            ctx,
            Value::Undefined,
            &[props.map_or(Value::Null, JsObject::into_value)],
        );
    }
    let Value::Str(_) = tag else {
        return Err(OpError::new(
            "TypeError",
            "JSX type must be a tag name or component",
        ));
    };
    let global = ctx.global_object();
    let document = ctx
        .get_member(&global, "document")
        .map_err(|_| OpError::new("Error", "document missing"))?;
    let element = invoke(ctx, &document, "createElement", &[tag])?;
    if let Some(props) = props {
        let mut reference = None;
        for name in keys(ctx, props.value())? {
            let value = props.get(ctx, &name)?;
            match name.as_str() {
                "children" => children(ctx, &element, value, 0)?,
                "key" => {}
                "ref" => reference = JsFunction::from_value(value),
                "dangerouslySetInnerHTML" => {
                    let html = ctx
                        .get_member(&value, "__html")
                        .map_err(|_| OpError::new("TypeError", "invalid inner HTML value"))?;
                    let html = ctx.coerce_string(&html).map_err(OpError::thrown)?;
                    ctx.set_member(&element, "innerHTML", Value::str(html))
                        .map_err(|_| OpError::new("Error", "inner HTML write failed"))?;
                }
                "style" if matches!(value, Value::Obj(_)) => {
                    apply_style(ctx, &element, &value)?;
                }
                name if name.starts_with("on") && name.len() > 2 => {
                    if value.is_callable() {
                        invoke(
                            ctx,
                            &element,
                            "addEventListener",
                            &[Value::str(name[2..].to_ascii_lowercase()), value],
                        )?;
                    }
                }
                name => {
                    if !matches!(value, Value::Null | Value::Undefined | Value::Bool(false)) {
                        let name = match name {
                            "className" => "class",
                            "htmlFor" => "for",
                            name => name,
                        };
                        let text = if matches!(value, Value::Bool(true)) {
                            Rc::from("")
                        } else {
                            ctx.coerce_string(&value).map_err(OpError::thrown)?
                        };
                        invoke(
                            ctx,
                            &element,
                            "setAttribute",
                            &[Value::str(name), Value::str(text)],
                        )?;
                    }
                }
            }
        }
        if let Some(reference) = reference {
            reference.call(ctx, Value::Undefined, &[element.clone()])?;
        }
    }
    Ok(element)
}
