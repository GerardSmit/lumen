//! `DataTransfer`, `DataTransferItemList` and `DataTransferItem`: a script-created drag data
//! store in the read/write mode. Items removed from the store keep their wrapper and turn into
//! disabled items.
use super::*;
use lumen::embed::JsFunction;
use lumen_bind::{Host, Passed};
use lumen::embed::JsHost;

const EFFECTS: &[&str] = &["none", "copy", "link", "move"];
const EFFECTS_ALLOWED: &[&str] =
    &["none", "copy", "copyLink", "copyMove", "link", "linkMove", "move", "all", "uninitialized"];

enum ItemData {
    Text(String),
    File(Value),
}

struct Item {
    data: ItemData,
    kind: String,
    disabled: bool,
    wrapper: Option<WeakValue>,
}

type ItemRef = Rc<RefCell<Item>>;

#[derive(Default)]
struct Store {
    items: Vec<ItemRef>,
    version: u64,
}

type StoreRef = Rc<RefCell<Store>>;

impl Store {
    fn changed(&mut self) {
        self.version += 1;
    }

    fn remove_at(&mut self, index: usize) {
        if index < self.items.len() {
            let item = self.items.remove(index);
            item.borrow_mut().disabled = true;
            self.changed();
        }
    }

    fn remove_text(&mut self, format: Option<&str>) {
        let before = self.items.len();
        self.items.retain(|item| {
            let mut item = item.borrow_mut();
            let matches = matches!(item.data, ItemData::Text(_)) && format.map_or(true, |format| item.kind == format);
            if matches {
                item.disabled = true;
            }
            !matches
        });
        if self.items.len() != before {
            self.changed();
        }
    }

    fn trace(&self, visit: &mut dyn FnMut(&Value)) {
        for item in &self.items {
            if let ItemData::File(file) = &item.borrow().data {
                visit(file);
            }
        }
    }
}

fn normalize_format(format: &str) -> String {
    format.trim_matches(|c: char| c.is_ascii_whitespace()).to_ascii_lowercase()
}

fn first_uri(list: &str) -> String {
    list.lines()
        .map(|line| line.trim_end_matches('\r'))
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .unwrap_or("")
        .to_owned()
}

fn file_type(ctx: &mut Ctx, file: &Value) -> String {
    lumen_host::blob::blob_of(ctx, file).map(|view| view.content_type.to_ascii_lowercase()).unwrap_or_default()
}

fn wrap_item(ctx: &mut Ctx, item: &ItemRef) -> OpResult<Value> {
    if let Some(value) = item.borrow().wrapper.as_ref().and_then(WeakValue::upgrade) {
        return Ok(value);
    }
    let value = ctx.new_instance(DomDataTransferItem { item: item.clone() });
    ctx.set_native_identity_owner::<DomDataTransferItem>(&value)?;
    item.borrow_mut().wrapper = ctx.weak_value(&value);
    Ok(value)
}

#[lumen_bind::class(name = "DataTransfer", hint(js(webidl)))]
pub struct DomDataTransfer {
    store: StoreRef,
    drop_effect: RefCell<String>,
    effect_allowed: RefCell<String>,
    items: RefCell<Option<Value>>,
    types: RefCell<Option<(u64, Value)>>,
    files: RefCell<Option<(u64, Value)>>,
}

impl lumen::embed::NativeIdentityOwner for DomDataTransfer {
    const TRACES_NATIVE_VALUES: bool = true;
    fn trace_native_identities(&self, _: u64, _: &mut dyn FnMut(&Value)) {}
    fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) {
        self.store.borrow().trace(visit);
        if let Some(items) = self.items.borrow().as_ref() { visit(items); }
        if let Some((_, types)) = self.types.borrow().as_ref() { visit(types); }
        if let Some((_, files)) = self.files.borrow().as_ref() { visit(files); }
    }
}

#[lumen_bind::methods]
impl DomDataTransfer {
    #[constructor]
    fn new() -> Self {
        Self {
            store: Rc::default(),
            drop_effect: RefCell::new("none".into()),
            effect_allowed: RefCell::new("none".into()),
            items: RefCell::new(None),
            types: RefCell::new(None),
            files: RefCell::new(None),
        }
    }

    #[getter(name = "dropEffect")]
    fn drop_effect(&self) -> String {
        self.drop_effect.borrow().clone()
    }

    #[setter(name = "dropEffect", coerce)]
    fn set_drop_effect(&self, value: String) {
        if EFFECTS.contains(&value.as_str()) {
            *self.drop_effect.borrow_mut() = value;
        }
    }

    #[getter(name = "effectAllowed")]
    fn effect_allowed(&self) -> String {
        self.effect_allowed.borrow().clone()
    }

    #[setter(name = "effectAllowed", coerce)]
    fn set_effect_allowed(&self, value: String) {
        if EFFECTS_ALLOWED.contains(&value.as_str()) {
            *self.effect_allowed.borrow_mut() = value;
        }
    }

    #[getter]
    fn items(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        if let Some(items) = self.items.borrow().as_ref() {
            return Ok(items.clone());
        }
        let list = ctx.new_instance(DomDataTransferItemList { store: self.store.clone(), owner: this.0 });
        ctx.set_native_identity_owner::<DomDataTransferItemList>(&list)?;
        *self.items.borrow_mut() = Some(list.clone());
        Ok(list)
    }

    #[getter]
    fn types(&self, ctx: &mut Ctx) -> Value {
        let version = self.store.borrow().version;
        if let Some((cached, types)) = self.types.borrow().as_ref() {
            if *cached == version {
                return types.clone();
            }
        }
        let mut names = Vec::new();
        let mut has_file = false;
        for item in &self.store.borrow().items {
            let item = item.borrow();
            match item.data {
                ItemData::Text(_) => names.push(Value::from_string(item.kind.clone())),
                ItemData::File(_) => has_file = true,
            }
        }
        if has_file {
            names.push(Value::str("Files"));
        }
        let array = JsHost::from_list(ctx, names);
        ctx.freeze_native_object(&array);
        *self.types.borrow_mut() = Some((version, array.clone()));
        array
    }

    #[getter]
    fn files(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let version = self.store.borrow().version;
        if let Some((cached, files)) = self.files.borrow().as_ref() {
            if *cached == version {
                return Ok(files.clone());
            }
        }
        let files: Vec<Value> = self
            .store
            .borrow()
            .items
            .iter()
            .filter_map(|item| match &item.borrow().data {
                ItemData::File(file) => Some(file.clone()),
                ItemData::Text(_) => None,
            })
            .collect();
        let list = crate::forms::file_list_from_values(ctx, files)?;
        *self.files.borrow_mut() = Some((version, list.clone()));
        Ok(list)
    }

    #[method(name = "getData", coerce)]
    fn get_data(&self, format: String) -> String {
        let mut format = normalize_format(&format);
        let mut as_url = false;
        if format == "text" {
            format = "text/plain".into();
        } else if format == "url" {
            format = "text/uri-list".into();
            as_url = true;
        }
        let text = self.store.borrow().items.iter().find_map(|item| {
            let item = item.borrow();
            match &item.data {
                ItemData::Text(text) if item.kind == format => Some(text.clone()),
                _ => None,
            }
        });
        match text {
            Some(text) if as_url => first_uri(&text),
            Some(text) => text,
            None => String::new(),
        }
    }

    #[method(name = "setData", coerce)]
    fn set_data(&self, format: String, data: String) {
        let mut format = normalize_format(&format);
        if format == "text" {
            format = "text/plain".into();
        } else if format == "url" {
            format = "text/uri-list".into();
        }
        let mut store = self.store.borrow_mut();
        store.remove_text(Some(&format));
        store.items.push(Rc::new(RefCell::new(Item {
            data: ItemData::Text(data),
            kind: format,
            disabled: false,
            wrapper: None,
        })));
        store.changed();
    }

    #[method(name = "clearData", coerce)]
    fn clear_data(&self, format: Passed<String>) {
        let format = format.0.map(|format| match normalize_format(&format).as_str() {
            "text" => "text/plain".to_owned(),
            "url" => "text/uri-list".to_owned(),
            other => other.to_owned(),
        });
        self.store.borrow_mut().remove_text(format.as_deref());
    }

    #[method(name = "setDragImage")]
    fn set_drag_image(&self, _element: Value, _x: i32, _y: i32) {}
}

#[lumen_bind::class(name = "DataTransferItemList", hint(js(webidl)))]
pub struct DomDataTransferItemList {
    store: StoreRef,
    owner: Value,
}

impl lumen::embed::NativeIdentityOwner for DomDataTransferItemList {
    const TRACES_NATIVE_VALUES: bool = true;
    fn trace_native_identities(&self, _: u64, _: &mut dyn FnMut(&Value)) {}
    fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) {
        visit(&self.owner);
    }
}

#[lumen_bind::methods]
impl DomDataTransferItemList {
    #[proto(len)]
    fn length(&self) -> usize {
        self.store.borrow().items.len()
    }

    #[proto(getitem)]
    fn indexed(&self, ctx: &mut Ctx, index: usize) -> OpResult<Value> {
        let item = self.store.borrow().items.get(index).cloned();
        match item {
            Some(item) => wrap_item(ctx, &item),
            None => Ok(Value::Undefined),
        }
    }

    #[method(coerce)]
    fn add(&self, ctx: &mut Ctx, data: Value, kind: Passed<String>) -> OpResult<Value> {
        let item = match kind.0 {
            Some(kind) => {
                let format = normalize_format(&kind);
                let text = ctx.coerce_string(&data).map_err(OpError::thrown)?.to_string();
                if self.store.borrow().items.iter().any(|item| {
                    let item = item.borrow();
                    matches!(item.data, ItemData::Text(_)) && item.kind == format
                }) {
                    return Err(OpError::new("NotSupportedError", "an item of that type is already in the list"));
                }
                Item { data: ItemData::Text(text), kind: format, disabled: false, wrapper: None }
            }
            None => {
                if !lumen_host::blob::is_file(ctx, &data) {
                    return Err(OpError::type_error("DataTransferItemList.add: the argument is not a File"));
                }
                let kind = file_type(ctx, &data);
                Item { data: ItemData::File(data), kind, disabled: false, wrapper: None }
            }
        };
        let item = Rc::new(RefCell::new(item));
        {
            let mut store = self.store.borrow_mut();
            store.items.push(item.clone());
            store.changed();
        }
        wrap_item(ctx, &item)
    }

    #[method(coerce)]
    fn remove(&self, index: u32) {
        self.store.borrow_mut().remove_at(index as usize);
    }

    fn clear(&self) {
        let mut store = self.store.borrow_mut();
        if store.items.is_empty() {
            return;
        }
        for item in store.items.drain(..) {
            item.borrow_mut().disabled = true;
        }
        store.changed();
    }
}

#[lumen_bind::class(name = "DataTransferItem", hint(js(webidl)))]
pub struct DomDataTransferItem {
    item: ItemRef,
}

impl lumen::embed::NativeIdentityOwner for DomDataTransferItem {
    const TRACES_NATIVE_VALUES: bool = true;
    fn trace_native_identities(&self, _: u64, _: &mut dyn FnMut(&Value)) {}
    fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) {
        if let ItemData::File(file) = &self.item.borrow().data {
            visit(file);
        }
    }
}

#[lumen_bind::methods]
impl DomDataTransferItem {
    #[getter]
    fn kind(&self) -> String {
        let item = self.item.borrow();
        if item.disabled {
            return String::new();
        }
        match item.data {
            ItemData::Text(_) => "string".into(),
            ItemData::File(_) => "file".into(),
        }
    }

    #[getter(name = "type")]
    fn kind_type(&self) -> String {
        let item = self.item.borrow();
        if item.disabled { String::new() } else { item.kind.clone() }
    }

    #[method(name = "getAsString")]
    fn get_as_string(&self, ctx: &mut Ctx, callback: Option<JsFunction>) -> OpResult<()> {
        let Some(callback) = callback else { return Ok(()) };
        let text = {
            let item = self.item.borrow();
            match &item.data {
                ItemData::Text(text) if !item.disabled => text.clone(),
                _ => return Ok(()),
            }
        };
        crate::scheduling::queue_task(ctx, move |ctx| {
            callback.call(ctx, Value::Undefined, &[Value::from_string(text)]).map(|_| ()).map_err(|error| OpError::thrown(error.to_value(ctx)))
        })
    }

    #[method(name = "getAsFile")]
    fn get_as_file(&self) -> Value {
        let item = self.item.borrow();
        match &item.data {
            ItemData::File(file) if !item.disabled => file.clone(),
            _ => Value::Null,
        }
    }
}

pub(crate) fn install(ctx: &mut Ctx) -> OpResult<()> {
    let global = ctx.global_object();
    for (name, constructor) in [
        ("DataTransfer", ctx.class_constructor::<DomDataTransfer>()),
        ("DataTransferItemList", ctx.class_constructor::<DomDataTransferItemList>()),
        ("DataTransferItem", ctx.class_constructor::<DomDataTransferItem>()),
    ] {
        crate::install_interface(ctx, &global, name, constructor)
            .map_err(|_| OpError::new("Error", "DataTransfer installation failed"))?;
    }
    Ok(())
}
