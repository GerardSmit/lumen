//! `MediaQueryListEvent`, the `change` event of a `MediaQueryList`.
use super::*;
use events::DomEvent;
use lumen_host::events::EventInit;

#[lumen_bind::class(name = "MediaQueryListEvent", extends = DomEvent, hint(js(webidl)))]
pub(crate) struct DomMediaQueryListEvent {
    base: DomEvent,
    media: String,
    matches: bool,
}

impl DomMediaQueryListEvent {
    pub(crate) fn change(media: &str, matches: bool) -> Self {
        Self { base: DomEvent::from_init("change", EventInit::default()), media: media.to_owned(), matches }
    }
}

#[lumen_bind::methods]
impl DomMediaQueryListEvent {
    #[constructor(coerce)]
    fn new(ctx: &mut Ctx, kind: &str, options: Option<Value>) -> OpResult<Self> {
        let init = EventInit::read(ctx, &options)?;
        let matches = match ui_events::dictionary_member(ctx, &options, "matches")? {
            Some(value) => ctx.to_boolean(&value),
            None => false,
        };
        let media = match ui_events::dictionary_member(ctx, &options, "media")? {
            Some(value) => ctx.coerce_string(&value).map_err(OpError::thrown)?.to_string(),
            None => String::new(),
        };
        Ok(Self { base: DomEvent::from_init(kind, init), media, matches })
    }

    #[getter]
    fn media(&self) -> String {
        self.media.clone()
    }

    #[getter]
    fn matches(&self) -> bool {
        self.matches
    }
}
