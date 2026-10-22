//! CSS Parser API rule and value classes.
use super::*;

const MAX_PARSER_ITEMS: usize = 4096;

#[lumen_bind::class(name="CSSParserRule",hint(js(webidl)))]
pub(super) struct DomCssParserRule {}

#[lumen_bind::class(name="CSSParserValue",hint(js(webidl)))]
pub(super) struct DomCssParserValue {}

#[lumen_bind::class(name="CSSParserAtRule",extends=DomCssParserRule,hint(js(webidl)))]
pub(super) struct DomCssParserAtRule {
    base:DomCssParserRule,
    name:String,
    prelude:Value,
    body:Option<Value>,
}

#[lumen_bind::class(name="CSSParserQualifiedRule",extends=DomCssParserRule,hint(js(webidl)))]
pub(super) struct DomCssParserQualifiedRule {
    base:DomCssParserRule,
    prelude:Value,
    body:Value,
}

#[lumen_bind::class(name="CSSParserDeclaration",extends=DomCssParserRule,hint(js(webidl)))]
pub(super) struct DomCssParserDeclaration {
    base:DomCssParserRule,
    name:String,
    body:Value,
}

#[lumen_bind::class(name="CSSParserBlock",extends=DomCssParserValue,hint(js(webidl)))]
pub(super) struct DomCssParserBlock {
    base:DomCssParserValue,
    name:String,
    body:Value,
}

#[lumen_bind::class(name="CSSParserFunction",extends=DomCssParserValue,hint(js(webidl)))]
pub(super) struct DomCssParserFunction {
    base:DomCssParserValue,
    name:String,
    args:Value,
}

fn frozen(ctx:&mut Ctx,items:Vec<Value>)->Value {
    let array=ctx.make_array(items);
    ctx.freeze_native_object(&array);
    array
}

fn token(ctx:&mut Ctx,value:Value)->OpResult<Value> {
    if ctx.with_instance::<DomCssParserValue,_>(&value,|_|()).is_ok()
        || ctx.with_instance::<DomCssStyleValue,_>(&value,|_|()).is_ok() {
        return Ok(value);
    }
    let text=ctx.coerce_string(&value).map_err(OpError::thrown)?;
    Ok(Value::str(lumen::well_formed_utf8(&text).into_owned()))
}

fn rule(ctx:&mut Ctx,value:Value)->OpResult<Value> {
    ctx.with_instance::<DomCssParserRule,_>(&value,|_|())
        .map_err(|_|OpError::type_error("expected a CSSParserRule"))?;
    Ok(value)
}

fn tokens(ctx:&mut Ctx,source:&Value)->OpResult<Value> {
    let items=ctx.convert_iterable(source,MAX_PARSER_ITEMS,token)?;
    Ok(frozen(ctx,items))
}

fn rules(ctx:&mut Ctx,source:&Value)->OpResult<Value> {
    let items=ctx.convert_iterable(source,MAX_PARSER_ITEMS,rule)?;
    Ok(frozen(ctx,items))
}

fn optional_rules(ctx:&mut Ctx,source:Option<Value>)->OpResult<Option<Value>> {
    match source {
        None|Some(Value::Undefined)|Some(Value::Null)=>Ok(None),
        Some(source)=>rules(ctx,&source).map(Some),
    }
}

fn join(ctx:&mut Ctx,list:&Value)->OpResult<String> {
    let length=ctx.member_get(list,"length").map_err(OpError::thrown)?;
    let length=ctx.coerce_number(&length).map_err(OpError::thrown)? as usize;
    let mut parts=Vec::with_capacity(length);
    for index in 0..length {
        let item=ctx.member_get(list,&index.to_string()).map_err(OpError::thrown)?;
        parts.push(ctx.coerce_string(&item).map_err(OpError::thrown)?.to_string());
    }
    Ok(parts.join(" "))
}

fn block_text(ctx:&mut Ctx,body:&Value)->OpResult<String> {
    let inner=join(ctx,body)?;
    Ok(if inner.is_empty() {"{ }".to_owned()} else {format!("{{ {inner} }}")})
}

#[lumen_bind::methods]
impl DomCssParserRule {}

#[lumen_bind::methods]
impl DomCssParserValue {}

#[lumen_bind::methods]
impl DomCssParserAtRule {
    #[constructor]
    fn new(ctx:&mut Ctx,name:&str,prelude:Value,body:Option<Value>)->OpResult<Self> {
        Ok(Self {
            base:DomCssParserRule {},
            name:name.to_owned(),
            prelude:tokens(ctx,&prelude)?,
            body:optional_rules(ctx,body)?,
        })
    }
    #[getter] fn name(&self)->String {self.name.clone()}
    #[getter] fn prelude(&self)->Value {self.prelude.clone()}
    #[getter] fn body(&self)->Value {self.body.clone().unwrap_or(Value::Null)}
    #[method(name="toString")]
    fn to_string(&self,ctx:&mut Ctx)->OpResult<String> {
        let prelude=join(ctx,&self.prelude)?;
        let head=if prelude.is_empty() {format!("@{}",self.name)} else {format!("@{} {prelude}",self.name)};
        Ok(match &self.body {
            Some(body)=>format!("{head} {}",block_text(ctx,body)?),
            None=>format!("{head};"),
        })
    }
}

#[lumen_bind::methods]
impl DomCssParserQualifiedRule {
    #[constructor]
    fn new(ctx:&mut Ctx,prelude:Value,body:Option<Value>)->OpResult<Self> {
        let prelude=tokens(ctx,&prelude)?;
        let body=match optional_rules(ctx,body)? {Some(body)=>body,None=>frozen(ctx,Vec::new())};
        Ok(Self {base:DomCssParserRule {},prelude,body})
    }
    #[getter] fn prelude(&self)->Value {self.prelude.clone()}
    #[getter] fn body(&self)->Value {self.body.clone()}
    #[method(name="toString")]
    fn to_string(&self,ctx:&mut Ctx)->OpResult<String> {
        let prelude=join(ctx,&self.prelude)?;
        let body=block_text(ctx,&self.body)?;
        Ok(if prelude.is_empty() {body} else {format!("{prelude} {body}")})
    }
}

#[lumen_bind::methods]
impl DomCssParserDeclaration {
    #[constructor]
    fn new(ctx:&mut Ctx,name:&str,body:Option<Value>)->OpResult<Self> {
        let body=match body {
            None|Some(Value::Undefined)=>frozen(ctx,Vec::new()),
            Some(source)=>tokens(ctx,&source)?,
        };
        Ok(Self {base:DomCssParserRule {},name:name.to_owned(),body})
    }
    #[getter] fn name(&self)->String {self.name.clone()}
    #[getter] fn body(&self)->Value {self.body.clone()}
    #[method(name="toString")]
    fn to_string(&self,ctx:&mut Ctx)->OpResult<String> {
        let body=join(ctx,&self.body)?;
        Ok(format!("{}: {body}",self.name))
    }
}

#[lumen_bind::methods]
impl DomCssParserBlock {
    #[constructor]
    fn new(ctx:&mut Ctx,name:&str,body:Value)->OpResult<Self> {
        if !matches!(name,"[]"|"{}"|"()") {
            return Err(OpError::new("SyntaxError","CSSParserBlock name must be \"[]\", \"{}\" or \"()\""));
        }
        Ok(Self {base:DomCssParserValue {},name:name.to_owned(),body:tokens(ctx,&body)?})
    }
    #[getter] fn name(&self)->String {self.name.clone()}
    #[getter] fn body(&self)->Value {self.body.clone()}
    #[method(name="toString")]
    fn to_string(&self,ctx:&mut Ctx)->OpResult<String> {
        let inner=join(ctx,&self.body)?;
        let mut brackets=self.name.chars();
        let open=brackets.next().unwrap_or('{');
        let close=brackets.next().unwrap_or('}');
        Ok(format!("{open}{inner}{close}"))
    }
}

#[lumen_bind::methods]
impl DomCssParserFunction {
    #[constructor]
    fn new(ctx:&mut Ctx,name:&str,args:Value)->OpResult<Self> {
        let lists=ctx.convert_iterable(&args,MAX_PARSER_ITEMS,|ctx,list|tokens(ctx,&list))?;
        Ok(Self {base:DomCssParserValue {},name:name.to_owned(),args:frozen(ctx,lists)})
    }
    #[getter] fn name(&self)->String {self.name.clone()}
    #[getter] fn args(&self)->Value {self.args.clone()}
    #[method(name="toString")]
    fn to_string(&self,ctx:&mut Ctx)->OpResult<String> {
        let length=ctx.member_get(&self.args,"length").map_err(OpError::thrown)?;
        let length=ctx.coerce_number(&length).map_err(OpError::thrown)? as usize;
        let mut parts=Vec::with_capacity(length);
        for index in 0..length {
            let list=ctx.member_get(&self.args,&index.to_string()).map_err(OpError::thrown)?;
            parts.push(join(ctx,&list)?);
        }
        Ok(format!("{}({})",self.name,parts.join(", ")))
    }
}

pub(super) fn install(ctx:&mut Ctx)->OpResult<()> {
    let global=ctx.global_object();
    macro_rules! interface {($name:literal,$class:ty)=>{
        let constructor=ctx.class_constructor::<$class>();
        crate::install_interface(ctx,&global,$name,constructor).map_err(|_|OpError::new("Error",concat!($name," installation failed")))?;
    };}
    interface!("CSSParserRule",DomCssParserRule);
    interface!("CSSParserAtRule",DomCssParserAtRule);
    interface!("CSSParserQualifiedRule",DomCssParserQualifiedRule);
    interface!("CSSParserDeclaration",DomCssParserDeclaration);
    interface!("CSSParserValue",DomCssParserValue);
    interface!("CSSParserBlock",DomCssParserBlock);
    interface!("CSSParserFunction",DomCssParserFunction);
    Ok(())
}
