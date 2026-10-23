//! `TimeRanges` and the `AudioTrack` / `VideoTrack` interfaces and lists of `HTMLMediaElement`.
//! No decoder reports in-band tracks, so every list is empty.
use super::*;

#[lumen_bind::class(name = "TimeRanges", hint(js(webidl)))]
pub struct DomTimeRanges {
    ranges: Vec<(f64, f64)>,
}

impl DomTimeRanges {
    pub(crate) fn new_value(ctx: &mut Ctx, ranges: Vec<(f64, f64)>) -> Value {
        ctx.new_instance(DomTimeRanges { ranges })
    }

    fn bound(&self, index: u32, end: bool) -> OpResult<f64> {
        self.ranges
            .get(index as usize)
            .map(|range| if end { range.1 } else { range.0 })
            .ok_or_else(|| OpError::new("IndexSizeError", "the TimeRanges index is out of range"))
    }
}

#[lumen_bind::methods]
impl DomTimeRanges {
    #[getter]
    fn length(&self) -> u32 {
        self.ranges.len() as u32
    }

    #[method(coerce)]
    fn start(&self, index: u32) -> OpResult<f64> {
        self.bound(index, false)
    }

    #[method(coerce)]
    fn end(&self, index: u32) -> OpResult<f64> {
        self.bound(index, true)
    }
}

#[lumen_bind::class(name = "AudioTrack", hint(js(webidl)))]
pub struct DomAudioTrack {
    enabled: std::cell::Cell<bool>,
}

#[lumen_bind::methods]
impl DomAudioTrack {
    #[getter]
    fn id(&self) -> &'static str {
        ""
    }
    #[getter]
    fn kind(&self) -> &'static str {
        ""
    }
    #[getter]
    fn label(&self) -> &'static str {
        ""
    }
    #[getter]
    fn language(&self) -> &'static str {
        ""
    }
    #[getter]
    fn enabled(&self) -> bool {
        self.enabled.get()
    }
    #[setter(coerce)]
    fn set_enabled(&self, value: bool) {
        self.enabled.set(value);
    }
}

#[lumen_bind::class(name = "VideoTrack", hint(js(webidl)))]
pub struct DomVideoTrack {
    selected: std::cell::Cell<bool>,
}

#[lumen_bind::methods]
impl DomVideoTrack {
    #[getter]
    fn id(&self) -> &'static str {
        ""
    }
    #[getter]
    fn kind(&self) -> &'static str {
        ""
    }
    #[getter]
    fn label(&self) -> &'static str {
        ""
    }
    #[getter]
    fn language(&self) -> &'static str {
        ""
    }
    #[getter]
    fn selected(&self) -> bool {
        self.selected.get()
    }
    #[setter(coerce)]
    fn set_selected(&self, value: bool) {
        self.selected.set(value);
    }
}

macro_rules! track_list {
    ($ty:ident, $interface:literal, $($extra:tt)*) => {
        #[lumen_bind::class(name = $interface, extends = DomEventTarget, hint(js(webidl)))]
        pub struct $ty {
            base: DomEventTarget,
        }

        impl $ty {
            fn new_value(ctx: &mut Ctx) -> Value {
                ctx.new_instance($ty { base: DomEventTarget::from_data(TargetData::new(None)) })
            }
        }

        #[lumen_bind::methods]
        impl $ty {
            #[getter]
            fn length(&self) -> u32 {
                0
            }
            #[proto(getitem)]
            fn indexed(&self, _index: usize) -> Value {
                Value::Undefined
            }
            #[method(name = "getTrackById", coerce)]
            fn get_track_by_id(&self, _id: &str) -> Value {
                Value::Null
            }
            #[getter]
            fn onchange(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
                self.base.handler_value(ctx, &this.0, "change")
            }
            #[setter]
            fn set_onchange(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>, callback: crate::events::EventHandler) {
                self.base.set_event_handler(ctx, &this.0, "change", callback);
            }
            #[getter]
            fn onaddtrack(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
                self.base.handler_value(ctx, &this.0, "addtrack")
            }
            #[setter]
            fn set_onaddtrack(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>, callback: crate::events::EventHandler) {
                self.base.set_event_handler(ctx, &this.0, "addtrack", callback);
            }
            #[getter]
            fn onremovetrack(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
                self.base.handler_value(ctx, &this.0, "removetrack")
            }
            #[setter]
            fn set_onremovetrack(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>, callback: crate::events::EventHandler) {
                self.base.set_event_handler(ctx, &this.0, "removetrack", callback);
            }
            $($extra)*
        }
    };
}

track_list!(DomAudioTrackList, "AudioTrackList",);
track_list!(
    DomVideoTrackList,
    "VideoTrackList",
    #[getter(name = "selectedIndex")]
    fn selected_index(&self) -> i32 {
        -1
    }
);

pub(crate) fn audio_track_list(ctx: &mut Ctx) -> Value {
    DomAudioTrackList::new_value(ctx)
}

pub(crate) fn video_track_list(ctx: &mut Ctx) -> Value {
    DomVideoTrackList::new_value(ctx)
}

pub(crate) fn constructors(ctx: &mut Ctx) -> Vec<(&'static str, Value)> {
    vec![
        ("TimeRanges", ctx.class_constructor::<DomTimeRanges>()),
        ("AudioTrack", ctx.class_constructor::<DomAudioTrack>()),
        ("VideoTrack", ctx.class_constructor::<DomVideoTrack>()),
        ("AudioTrackList", ctx.class_constructor::<DomAudioTrackList>()),
        ("VideoTrackList", ctx.class_constructor::<DomVideoTrackList>()),
    ]
}
