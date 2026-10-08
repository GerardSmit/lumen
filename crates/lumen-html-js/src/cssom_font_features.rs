//! Typed, live CSS Font Feature Values maps over the canonical CSSOM source.
use super::*;
use css::font_feature_values::{FeatureKind,FeatureAlias,MAX_FEATURE_ALIASES,MAX_FEATURE_INDICES};
use lumen::embed::{ArgCx,NativeIdentityOwner};
use lumen_bind::Slot;

#[derive(Default)]
struct FeatureMapState { entries:RefCell<Vec<Option<(String,Arc<[u32]>)>>>, iterators:Cell<usize> }
impl FeatureMapState {
    fn compact(&self) {if self.iterators.get()==0 {self.entries.borrow_mut().retain(Option::is_some);}}
    fn next(&self,cursor:&mut usize)->Option<(String,Arc<[u32]>)> {
        let entries=self.entries.borrow();while *cursor<entries.len(){let at=*cursor;*cursor+=1;if let Some(entry)=&entries[at]{return Some(entry.clone());}}None
    }
}
#[lumen_bind::class(name="CSSFontFeatureValuesRule",extends=DomCssRule,hint(js(webidl)))]
pub(super) struct DomCssFontFeatureValuesRule {
    base:DomCssRule, families:RefCell<Arc<[Arc<str>]>>, display:Option<css::FontDisplay>,
    maps:Vec<Rc<FeatureMapState>>, wrappers:RefCell<Vec<Option<WeakValue>>>,
}
impl NativeIdentityOwner for DomCssFontFeatureValuesRule {
    const TRACES_NATIVE_VALUES:bool=true;
    fn trace_native_identities(&self,_:u64,_:&mut dyn FnMut(&Value)) {}
    fn trace_native_values(&self,visit:&mut dyn FnMut(&Value)) {visit(&self.base.owner);visit(&self.base.sheet_owner);}
}
impl DomCssFontFeatureValuesRule {
    pub(super) fn new(base:DomCssRule)->OpResult<Self> {
        let parsed=css::parse_stylesheet(&base.current_rule()?.css_text).map_err(css_error)?;
        let rule=parsed.family_display.first().ok_or_else(||OpError::new("InvalidStateError","invalid feature values rule"))?;
        let maps:Vec<_>=FeatureKind::ALL.iter().map(|_|Rc::new(FeatureMapState::default())).collect();
        for alias in rule.aliases.iter(){maps[alias.kind as usize].entries.borrow_mut().push(Some((alias.name.to_string(),alias.indices.clone())));}
        Ok(Self{base,families:RefCell::new(rule.families.clone()),display:rule.display_specified.then_some(rule.display),
            maps,wrappers:RefCell::new(vec![None;FeatureKind::ALL.len()])})
    }
    fn aliases(&self)->Vec<FeatureAlias> {let mut aliases=Vec::new();for kind in FeatureKind::ALL {for (name,indices) in self.maps[kind as usize].entries.borrow().iter().flatten(){aliases.push(FeatureAlias{kind,name:Arc::from(name.as_str()),indices:indices.clone()});}}aliases}
    fn text(&self)->String {css::font_feature_values::serialize_rule(&self.families.borrow(),self.display,&self.aliases())}
    fn commit(&self,families:&[Arc<str>],aliases:&[FeatureAlias])->OpResult<()> {
        let text=css::font_feature_values::serialize_rule(families,self.display,aliases);
        let location=rule_location(&self.base.realm,&self.base.source,&self.base.path)?;
        let mut sheet=source_sheet(&self.base.realm,&location.source)?;let before=sheet.clone();
        let parsed=css::nesting::parse_one_source_rule(&text,&[]).map_err(css_error)?;
        let mut replacement=source_rules_to_cssom(&text,&[parsed],false).map_err(css_error)?;
        let replacement=replacement.remove(0);
        sheet.mutate_rule(&location.indices,|target| {*target=replacement;Ok(())}).map_err(css_error)?;
        commit_rule_declaration(&self.base.realm,&location.source,&before,&sheet,&[],None)?;
        let updated=source_sheet(&self.base.realm,&location.source)?;
        remember_rule_path(&location.source,&updated,&self.base.path)
    }
    fn map(&self,ctx:&mut Ctx,owner:Value,kind:FeatureKind)->OpResult<Value> {
        if let Some(value)=self.wrappers.borrow()[kind as usize].as_ref().and_then(WeakValue::upgrade){return Ok(value);}
        let value=ctx.new_instance(DomCssFontFeatureValuesMap{owner,kind,state:self.maps[kind as usize].clone()});
        ctx.set_native_identity_owner::<DomCssFontFeatureValuesMap>(&value)?;
        self.wrappers.borrow_mut()[kind as usize]=ctx.weak_value(&value);Ok(value)
    }
    fn mutate(&self,kind:FeatureKind,name:Option<&str>,values:Option<Arc<[u32]>>)->OpResult<bool> {
        let mut aliases=self.aliases();let found=name.and_then(|name|aliases.iter().position(|alias|alias.kind==kind && alias.name.as_ref()==name));
        let changed=if let Some(values)=values.as_ref() {
            let name=name.expect("set has a name");
            if let Some(at)=found {if aliases[at].indices==*values{return Ok(false);}aliases[at].indices=values.clone();}
            else {if aliases.len()>=MAX_FEATURE_ALIASES{return Err(OpError::new("QuotaExceededError","too many font feature aliases"));}aliases.push(FeatureAlias{kind,name:Arc::from(name),indices:values.clone()});}true
        } else if name.is_some() {let Some(at)=found else{return Ok(false);};aliases.remove(at);true}
        else {let before=aliases.len();aliases.retain(|alias|alias.kind!=kind);before!=aliases.len()};
        if !changed{return Ok(false);}
        if aliases.iter().map(|alias|alias.name.len()+core::mem::size_of_val(alias.indices.as_ref())).sum::<usize>()>1024*1024 {return Err(OpError::new("QuotaExceededError","font feature metadata exceeds limit"));}
        let state=&self.maps[kind as usize];
        if values.is_some() && found.is_none() {state.compact();let mut entries=state.entries.borrow_mut();if entries.len()>=MAX_FEATURE_ALIASES{return Err(OpError::new("QuotaExceededError","font feature iterator metadata exceeds limit"));}entries.try_reserve(1).map_err(|_|OpError::new("QuotaExceededError","font feature allocation failed"))?;}
        let families=self.families.borrow().clone();self.commit(&families,&aliases)?;
        let mut entries=state.entries.borrow_mut();
        if let Some(values)=values {let name=name.unwrap();if let Some(old)=entries.iter_mut().flatten().find(|(key,_)|key==name){old.1=values;}else{entries.push(Some((name.to_owned(),values)));}}
        else if let Some(name)=name {if let Some(old)=entries.iter_mut().find(|entry|entry.as_ref().is_some_and(|(key,_)|key==name)){*old=None;}}
        else {for entry in entries.iter_mut(){*entry=None;}}
        drop(entries);state.compact();Ok(true)
    }
}
macro_rules! feature_rule_methods {($( $get:ident => $kind:ident $(, $name:literal)?; )*)=>{
    #[lumen_bind::methods] impl DomCssFontFeatureValuesRule {
        #[getter(name="fontFamily")] fn font_family(&self)->String {css::font_feature_values::serialize_family_names(&self.families.borrow())}
        #[setter(name="fontFamily",coerce)] fn set_font_family(&self,value:&str)->OpResult<()> {
            let Some(families)=css::parse_font_family_list(value).filter(|families|!families.is_empty() && families.iter().all(|family|family.named().is_some())) else{return Ok(());};
            let names:Arc<[Arc<str>]>=families.iter().filter_map(|family|family.named().cloned()).collect::<Vec<_>>().into();
            self.commit(&names,&self.aliases())?;*self.families.borrow_mut()=names;Ok(())
        }
        #[getter(name="cssText")] fn css_text(&self)->String {self.text()}
        $(#[getter $( (name=$name) )?] fn $get(&self,ctx:&mut Ctx,this:This<Value>)->OpResult<Value>{self.map(ctx,this.0,FeatureKind::$kind)})*
    }
};}
feature_rule_methods!{annotation=>Annotation;ornaments=>Ornaments;stylistic=>Stylistic;swash=>Swash;character_variant=>CharacterVariant,"characterVariant";styleset=>Styleset;historical_forms=>HistoricalForms,"historicalForms";}

struct FeatureIndices(Arc<[u32]>);
impl<'a> FromArg<'a,JsHost> for FeatureIndices {
    fn from_arg(cx:&'a ArgCx<'_>,value:&'a Value,_:Slot)->Result<Self,Value> {
        JsHost::with_ctx(cx,|ctx| {
            let method=if value.object_identity().is_some() {
                let key=ctx.well_known_symbol("iterator").expect("installed iterator symbol");
                let method=ctx.reflect_get(value,&key,value).map_err(OpError::thrown)?;
                (!matches!(method,Value::Null|Value::Undefined)).then_some(method)
            }else{None};
            let values=if let Some(method)=method {ctx.convert_iterable_with_method(value,method,MAX_FEATURE_INDICES,|ctx,value|ctx.webidl_long(&value).map(|value|value as u32).map_err(OpError::thrown))?}
                else {vec![ctx.webidl_long(value).map_err(OpError::thrown)? as u32]};
            Ok::<_,OpError>(FeatureIndices(values.into()))
        }).map_err(|error|JsHost::with_ctx(cx,|ctx|error.to_value(ctx)))
    }
}
#[lumen_bind::class(name="CSSFontFeatureValuesMap",hint(js(webidl)))]
pub(super) struct DomCssFontFeatureValuesMap {owner:Value,kind:FeatureKind,state:Rc<FeatureMapState>}
impl NativeIdentityOwner for DomCssFontFeatureValuesMap {
    const TRACES_NATIVE_VALUES:bool=true;
    fn trace_native_identities(&self,_:u64,_:&mut dyn FnMut(&Value)) {}
    fn trace_native_values(&self,visit:&mut dyn FnMut(&Value)){visit(&self.owner);}
}
fn sequence(ctx:&mut Ctx,indices:&[u32])->Value {JsHost::from_list(ctx,indices.iter().map(|&value|Value::Num(f64::from(value))).collect())}
impl DomCssFontFeatureValuesMap {
    fn iterator(&self,ctx:&mut Ctx,owner:Value,mode:u8)->OpResult<Value> {
        self.state.iterators.set(self.state.iterators.get()+1);
        let value=ctx.new_instance(FeatureMapIterator{owner,state:self.state.clone(),cursor:Cell::new(0),done:Cell::new(false),mode});
        ctx.set_native_identity_owner::<FeatureMapIterator>(&value)?;Ok(value)
    }
}
#[lumen_bind::methods] impl DomCssFontFeatureValuesMap {
    #[getter] fn size(&self)->usize {self.state.entries.borrow().iter().flatten().count()}
    #[method(coerce)] fn get(&self,ctx:&mut Ctx,name:String)->Value {self.state.entries.borrow().iter().flatten().find(|(key,_)|key==&name).map_or(Value::Undefined,|(_,indices)|sequence(ctx,indices))}
    #[method(coerce)] fn has(&self,name:String)->bool {self.state.entries.borrow().iter().flatten().any(|(key,_)|key==&name)}
    #[method(coerce)] fn set(&self,ctx:&mut Ctx,name:String,values:FeatureIndices)->OpResult<()> {
        if values.0.len()>self.kind.maximum(){return Err(crate::error_reporting::dom_exception(ctx,"InvalidAccessError","too many values for this feature block"));}
        ctx.with_instance::<DomCssFontFeatureValuesRule,_>(&self.owner,|rule|rule.mutate(self.kind,Some(&name),Some(values.0)))??;Ok(())
    }
    #[method(coerce)] fn delete(&self,ctx:&mut Ctx,name:String)->OpResult<bool> {ctx.with_instance::<DomCssFontFeatureValuesRule,_>(&self.owner,|rule|rule.mutate(self.kind,Some(&name),None))?}
    fn clear(&self,ctx:&mut Ctx)->OpResult<()> {ctx.with_instance::<DomCssFontFeatureValuesRule,_>(&self.owner,|rule|rule.mutate(self.kind,None,None))??;Ok(())}
    fn keys(&self,ctx:&mut Ctx,this:This<Value>)->OpResult<Value>{self.iterator(ctx,this.0,0)}
    fn values(&self,ctx:&mut Ctx,this:This<Value>)->OpResult<Value>{self.iterator(ctx,this.0,1)}
    #[method(hint(js(also_iterator)))] fn entries(&self,ctx:&mut Ctx,this:This<Value>)->OpResult<Value>{self.iterator(ctx,this.0,2)}
    fn for_each(&self,ctx:&mut Ctx,this:This<Value>,callback:JsFunction,#[default(Value::Undefined)] this_arg:Value)->OpResult<()> {
        self.state.iterators.set(self.state.iterators.get()+1);
        let guard=FeatureMapIterator{owner:this.0.clone(),state:self.state.clone(),cursor:Cell::new(0),done:Cell::new(false),mode:2};
        let mut cursor=0;while let Some((name,indices))=self.state.next(&mut cursor){let values=sequence(ctx,&indices);callback.call(ctx,this_arg.clone(),&[values,Value::from_string(name),this.0.clone()])?;}drop(guard);Ok(())
    }
}
#[lumen_bind::class(name="CSSFontFeatureValuesIterator")]
struct FeatureMapIterator {owner:Value,state:Rc<FeatureMapState>,cursor:Cell<usize>,done:Cell<bool>,mode:u8}
impl FeatureMapIterator {fn release(&self){if !self.done.replace(true){self.state.iterators.set(self.state.iterators.get().saturating_sub(1));self.state.compact();}}}
impl Drop for FeatureMapIterator {fn drop(&mut self){self.release();}}
impl NativeIdentityOwner for FeatureMapIterator {
    const TRACES_NATIVE_VALUES:bool=true;
    fn trace_native_identities(&self,_:u64,_:&mut dyn FnMut(&Value)) {}
    fn trace_native_values(&self,visit:&mut dyn FnMut(&Value)){visit(&self.owner);}
}
#[lumen_bind::methods] impl FeatureMapIterator {
    #[proto(iter)] fn iter(&self,this:This<Value>)->Value {this.0}
    #[proto(next)] fn next(&self,ctx:&mut Ctx)->OpResult<Option<Value>> {
        if self.done.get(){return Ok(None);}let mut cursor=self.cursor.get();let next=self.state.next(&mut cursor);self.cursor.set(cursor);
        let Some((name,indices))=next else{self.release();return Ok(None);};
        Ok(Some(match self.mode {0=>Value::from_string(name),1=>sequence(ctx,&indices),_=>{let value=sequence(ctx,&indices);JsHost::from_list(ctx,vec![Value::from_string(name),value])}}))
    }
}
pub(super) fn wrap(ctx:&mut Ctx,base:DomCssRule)->OpResult<Value> {let value=ctx.new_instance(DomCssFontFeatureValuesRule::new(base)?);ctx.set_native_identity_owner::<DomCssFontFeatureValuesRule>(&value)?;Ok(value)}
pub(super) fn install(ctx:&mut Ctx)->OpResult<()> {
    ctx.class_constructor::<FeatureMapIterator>();
    let global=ctx.global_object();
    let rule=ctx.class_constructor::<DomCssFontFeatureValuesRule>();crate::install_interface(ctx,&global,"CSSFontFeatureValuesRule",rule).map_err(|_|OpError::new("Error","feature rule installation failed"))?;
    let map=ctx.class_constructor::<DomCssFontFeatureValuesMap>();crate::install_interface(ctx,&global,"CSSFontFeatureValuesMap",map).map_err(|_|OpError::new("Error","feature map installation failed"))?;Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn specification_font_family_scopes_preserve_inheritance_contents_and_slotted_origins() {
        use lumen_html::{selector,css};
        let mut engine=lumen::Engine::new();
        let realm=crate::install(engine.ctx(),r#"<style>
            @font-face{font-family:Fancy;src:url(outer.otf)}
            @font-feature-values Fancy{@stylistic{pick:1}}
            #host{font-family:Fancy;font-variant-alternates:stylistic(pick)}
            #host::part(external){font-family:Fancy}
        </style><div id=host><span class=slotted slot=source>slot</span></div>"#,256).unwrap();
        assert!(engine.eval_value(r#"(()=>{
            const root=document.getElementById('host').attachShadow({mode:'open'});
            root.innerHTML=`<style>
                @font-face{font-family:Fancy;src:url(inner.otf)}
                @font-feature-values Fancy{@stylistic{pick:2}}
                .local,.contents{font-family:Fancy}.contents{display:contents}
                ::slotted(.slotted){font-family:Fancy}
              </style><span id=inherited>outer</span><span id=local class=local>inner</span>
              <div class=contents><span id=contents>inner</span></div><span id=part part=external>outer</span><slot name=source></slot>`;
            return true;
        })()"#).is_ok());
        realm.with_session(|session| {
            let document=session.document();
            let host=selector::query_selector(document,document.root(),"#host").unwrap().unwrap();
            let shadow=document.shadow_root(host).unwrap().unwrap();
            let find=|name|selector::query_selector(document,shadow,name).unwrap().unwrap();
            let ids=[find("#inherited"),find("#local"),find("#contents"),find("#part")];
            let slot=selector::query_selector(document,document.root(),".slotted").unwrap().unwrap();
            let inherited=session.computed_style(ids[0]).unwrap();
            let local=session.computed_style(ids[1]).unwrap();
            let contents=session.computed_style(ids[2]).unwrap();
            let part=session.computed_style(ids[3]).unwrap();
            let slotted=session.computed_style(slot).unwrap();
            assert!(inherited.font_spec().family_scope.is_none());
            assert!(part.font_spec().family_scope.is_none(),"::part uses the declaring outer sheet's scope");
            let local_scope=local.font_spec().family_scope.as_ref().unwrap();
            assert_eq!(local_scope.as_ref(),[shadow]);
            assert!(std::sync::Arc::ptr_eq(local_scope,contents.font_spec().family_scope.as_ref().unwrap()),"contents inheritance reuses the interned chain");
            assert!(std::sync::Arc::ptr_eq(local_scope,slotted.font_spec().family_scope.as_ref().unwrap()),"::slotted captures its declaring shadow scope");
            let faces=session.font_faces().unwrap();
            for (style,url,index) in [(&inherited,"outer.otf",1),(&local,"inner.otf",2)] {
                let selected=css::matching_font_faces(style.font_spec(),&faces,"A");
                assert_eq!(selected.len(),1);
                let rule=&faces[selected[0]];
                assert!(matches!(rule.sources.first(),Some(css::FontFaceSource::Url(source)) if source.as_ref()==url));
                let features=css::font_feature_values::resolve_features(style.font_spec().alternates.as_deref(),&rule.family,&rule.family_display,session.media_environment(),style.font_spec().family_scope.as_deref());
                assert_eq!(features,[lumen_html::paint::FontFeature{tag:*b"salt",value:index}]);
            }
        });
    }

    #[test]
    fn specification_shadow_styles_follow_custom_host_connection_and_family_provenance() {
        let mut engine=lumen::Engine::new();
        let realm=crate::install(engine.ctx(),"<body></body>",256).unwrap();
        assert!(matches!(engine.eval_value(r#"(() => {
            customElements.define('font-host',class extends HTMLElement {
                constructor(){super();this.attachShadow({mode:'open'}).innerHTML='<style>:host{color:red;border:3px solid blue;font-family:ShadowFancy;font-variant-alternates:stylistic(pick)}:host(:state(active)){color:green}@font-feature-values ShadowFancy{@stylistic{pick:2}}</style><span>ink</span>';this.internals=this.attachInternals()}
            });
            globalThis.h=document.createElement('font-host');
            document.body.append(h);return true;
        })()"#),Ok(Ok(Value::Bool(true)))));
        realm.queue_stylesheet_tasks(engine.ctx()).unwrap();
        let host=realm.with_session(|session|lumen_html::selector::query_selector(session.document(),session.document().root(),"font-host").unwrap().unwrap());
        let check=|expected|realm.with_session(|session| {
            let style=session.computed_style(host).unwrap();
            assert_eq!(style.color,expected);assert_eq!(style.border_width,3.0);
            let shadow=session.document().shadow_root(host).unwrap().unwrap();
            assert_eq!(style.font_spec().family_scope.as_deref(),Some([shadow].as_slice()));
        });
        check(lumen_html::paint::Rgba{r:255,g:0,b:0,a:255});
        assert!(engine.eval_value("h.internals.states.add('active')").is_ok());
        check(lumen_html::paint::Rgba{r:0,g:128,b:0,a:255});
        assert!(engine.eval_value("h.remove();document.body.append(h)").is_ok());
        realm.queue_stylesheet_tasks(engine.ctx()).unwrap();
        check(lumen_html::paint::Rgba{r:0,g:128,b:0,a:255});
    }

    #[test]
    fn specification_font_feature_values_maps_keep_live_identity_iteration_and_detached_sources() {
        let mut engine=lumen::Engine::new();
        let _realm=crate::install(engine.ctx(),"<style>@font-feature-values Fancy { @stylistic { first:1; } @styleset { pair:1 3; } }</style><body></body>",128).unwrap();
        let script=r#"(() => {
          const rule=document.styleSheets[0].cssRules[0];
          const check=(ok,message)=>{if(!ok)throw Error(message)};
          check(rule instanceof CSSFontFeatureValuesRule && rule.type===CSSRule.FONT_FEATURE_VALUES_RULE,'typed rule');
          const map=rule.stylistic;
          check(map===rule.stylistic && map instanceof CSSFontFeatureValuesMap,'SameObject typed map');
          check(map.entries===map[Symbol.iterator],'maplike iterator aliases the entries function');
          check(map.get('first')[0]===1 && map.size===1,'parsed alias');
          check(Object.getOwnPropertyDescriptor(CSSRule.prototype,'cssText').get.call(rule).includes('first: 1;'),'base rule serialization preserves aliases');
          let closed=0,conversionFailure;
          try{map.set('bad',{[Symbol.iterator](){return {next(){return {done:false,value:Symbol()}},return(){closed++;return {done:true}}}}})}catch(error){conversionFailure=error}
          check(conversionFailure instanceof TypeError && closed===1 && !map.has('bad'),'failed unsigned conversion closes the iterator transactionally');
          let probes=0;
          map.set('iterable',{get [Symbol.iterator](){probes++; return function*(){yield 4294967297}}});
          check(probes===1 && map.get('iterable')[0]===1,'one iterator lookup and unsigned long conversion');
          let failure;
          try{map.set('first',[1,2])}catch(error){failure=error}
          check(failure?.name==='InvalidAccessError' && map.get('first')[0]===1,'invalid cardinality is transactional');
          const entries=map.entries();
          check(entries.next().value[0]==='first','insertion order');
          map.delete('iterable');map.set('later',3);
          check(entries.next().value[0]==='later' && entries.next().done,'live iterator deletion and append');
          const visited=[];
          map.forEach((value,key,owner)=>{check(owner===map,'callback owner');visited.push(key);if(key==='first')map.set('tail',4)});
          check(visited.join(',')==='first,later,tail','live forEach');
          const copy=map.get('first');copy[0]=9;check(map.get('first')[0]===1,'sequence getter returns a copy');
          rule.fontFamily='Other, "serif"';
          check(rule.fontFamily==='Other, "serif"' && rule.cssText.includes('first: 1;'),'canonical live source');
          const sheet=rule.parentStyleSheet;sheet.deleteRule(0);
          map.set('detached',5);
          check(sheet.cssRules.length===0 && map.get('detached')[0]===5 && rule.cssText.includes('detached: 5;'),'retained map edits its detached rule only');
          return true;
        })()"#;
        match engine.eval_value(script).expect("feature map fixture parses") {
            Ok(Value::Bool(true))=>{},
            Ok(value)=>{
                let message=engine.ctx().coerce_string(&value).map(|value|value.to_string()).unwrap_or_else(|_|"unprintable result".into());
                panic!("feature map fixture returned {message}");
            },
            Err(error)=>{
                let message=engine.ctx().coerce_string(&error).map(|value|value.to_string()).unwrap_or_else(|_|"unprintable exception".into());
                panic!("actual typed feature maps and canonical source transactions: {message}");
            }
        }
    }
}
