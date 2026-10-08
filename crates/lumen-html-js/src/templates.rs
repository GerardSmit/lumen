use super::*;
use lumen::embed::{OpError, OpResult};
use std::rc::Weak;

struct TemplateData {
    realm: Rc<DomRealm>,
    root: NodeId,
}
impl Drop for TemplateData {
    fn drop(&mut self) {
        self.realm.defer_detached_root(self.root);
    }
}

#[derive(Default)]
struct TemplateCache {
    entries: HashMap<String, Weak<TemplateData>>,
    sweep_at: usize,
}

#[lumen_bind::class(name = "DOMTemplate")]
pub struct Template {
    data: Rc<TemplateData>,
}
#[lumen_bind::methods]
impl Template {}

pub(crate) fn template(ctx: &mut Ctx, markup: &str) -> OpResult<Template> {
    if let Some(data) = ctx
        .op_state()
        .get::<TemplateCache>()
        .and_then(|cache| cache.entries.get(markup))
        .and_then(Weak::upgrade)
    {
        return Ok(Template { data });
    }
    let realm = reactive::runtime(ctx)?
        .realm
        .upgrade()
        .ok_or_else(|| OpError::new("Error", "DOM realm was dropped"))?;
    let root = html::parse_fragment(realm.session.borrow_mut().document_mut(), markup).map_err(
        |error| {
            OpError::new(
                "SyntaxError",
                format!("template HTML at {}: {}", error.offset, error.message),
            )
        },
    )?;
    let inert_scripts = {
        let session = realm.session.borrow();
        super::script_loading::scripts_in_subtree(session.document(), root)
    };
    for node in inert_scripts {
        realm.scripts.borrow_mut().mark_started(node);
    }
    let data = Rc::new(TemplateData { realm, root });
    if !ctx.op_state().has::<TemplateCache>() {
        ctx.op_state().put(TemplateCache::default());
    }
    let cache = ctx.op_state().get_mut::<TemplateCache>().unwrap();
    if cache.entries.len() >= cache.sweep_at.max(256) {
        cache
            .entries
            .retain(|_, template| template.strong_count() > 0);
        cache.sweep_at = cache.entries.len().saturating_mul(2).max(256);
    }
    cache.entries.insert(markup.into(), Rc::downgrade(&data));
    Ok(Template { data })
}

pub(crate) fn instantiate(ctx: &mut Ctx, template: &Template) -> OpResult<Value> {
    let mut session = template.data.realm.session.borrow_mut();
    let document = session.document_mut();
    let mut root = document
        .clone_subtree(template.data.root)
        .map_err(|error| OpError::from(dom_error(error)))?;
    if let Some(child) = document
        .first_child(root)
        .map_err(|error| OpError::from(dom_error(error)))?
    {
        if document
            .next_sibling(child)
            .map_err(|error| OpError::from(dom_error(error)))?
            .is_none()
        {
            document
                .remove(child)
                .map_err(|error| OpError::from(dom_error(error)))?;
            document
                .destroy_subtree(root)
                .map_err(|error| OpError::from(dom_error(error)))?;
            root = child;
        }
    }
    drop(session);
    Ok(template.data.realm.wrap(ctx, root))
}

pub(crate) fn node_at(ctx: &mut Ctx, root: &DomNode, path: &[usize]) -> OpResult<Value> {
    let session = root.realm.session.borrow();
    let document = session.document();
    let mut node = root.id;
    for index in path {
        let mut child = document
            .first_child(node)
            .map_err(|error| OpError::from(dom_error(error)))?;
        for _ in 0..*index {
            child = child
                .map(|id| document.next_sibling(id))
                .transpose()
                .map_err(|error| OpError::from(dom_error(error)))?
                .flatten();
        }
        node =
            child.ok_or_else(|| OpError::new("RangeError", "template slot path does not exist"))?;
    }
    drop(session);
    Ok(root.realm.wrap(ctx, node))
}
