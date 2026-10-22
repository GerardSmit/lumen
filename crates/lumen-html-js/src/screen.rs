//! `window.screen` and `screen.orientation`: the display the window is on.
//!
//! The embedder owns the display, so the values come from [`ScreenMetrics`]
//! passed to [`DomRealm::set_screen`]. The default is a 1280x720 landscape
//! display with 24-bit colour and no reserved system UI.
use super::*;
use std::rc::Weak;

const SCREEN_SLOT: &str = "#lumen_window\u{1}screen";
const ORIENTATION_SLOT: &str = "#lumen_screen\u{1}orientation";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScreenOrientationKind {
    PortraitPrimary,
    PortraitSecondary,
    LandscapePrimary,
    LandscapeSecondary,
}

impl ScreenOrientationKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::PortraitPrimary => "portrait-primary",
            Self::PortraitSecondary => "portrait-secondary",
            Self::LandscapePrimary => "landscape-primary",
            Self::LandscapeSecondary => "landscape-secondary",
        }
    }
}

/// Display geometry reported through `window.screen`, in CSS pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScreenMetrics {
    pub width: u32,
    pub height: u32,
    pub avail_width: u32,
    pub avail_height: u32,
    pub avail_left: i32,
    pub avail_top: i32,
    pub color_depth: u32,
    pub orientation: ScreenOrientationKind,
    pub orientation_angle: u16,
}

impl ScreenMetrics {
    /// A display of `width` x `height` whose whole area is available to the page.
    pub fn new(width: u32, height: u32) -> Self {
        let orientation = if width >= height {
            ScreenOrientationKind::LandscapePrimary
        } else {
            ScreenOrientationKind::PortraitPrimary
        };
        Self {
            width,
            height,
            avail_width: width,
            avail_height: height,
            avail_left: 0,
            avail_top: 0,
            color_depth: 24,
            orientation,
            orientation_angle: 0,
        }
    }
}

impl Default for ScreenMetrics {
    fn default() -> Self {
        Self::new(1280, 720)
    }
}

#[lumen_bind::class(name = "Screen", hint(js(webidl)))]
pub struct DomScreen {
    realm: Weak<DomRealm>,
}

impl DomScreen {
    fn metrics(&self) -> ScreenMetrics {
        self.realm.upgrade().map_or_else(ScreenMetrics::default, |realm| *realm.screen.borrow())
    }
}

#[lumen_bind::methods]
impl DomScreen {
    #[getter(name = "availWidth")]
    fn avail_width(&self) -> u32 {
        self.metrics().avail_width
    }

    #[getter(name = "availHeight")]
    fn avail_height(&self) -> u32 {
        self.metrics().avail_height
    }

    #[getter(name = "availLeft")]
    fn avail_left(&self) -> i32 {
        self.metrics().avail_left
    }

    #[getter(name = "availTop")]
    fn avail_top(&self) -> i32 {
        self.metrics().avail_top
    }

    #[getter]
    fn width(&self) -> u32 {
        self.metrics().width
    }

    #[getter]
    fn height(&self) -> u32 {
        self.metrics().height
    }

    #[getter(name = "colorDepth")]
    fn color_depth(&self) -> u32 {
        self.metrics().color_depth
    }

    #[getter(name = "pixelDepth")]
    fn pixel_depth(&self) -> u32 {
        self.metrics().color_depth
    }

    #[getter]
    fn orientation(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> Value {
        ctx.native_private_value_slot(&this.0, ORIENTATION_SLOT).unwrap_or(Value::Undefined)
    }
}

#[lumen_bind::class(name = "ScreenOrientation", hint(js(webidl)))]
pub struct DomScreenOrientation {
    realm: Weak<DomRealm>,
}

#[lumen_bind::methods]
impl DomScreenOrientation {
    #[getter(name = "type")]
    fn kind(&self) -> String {
        let metrics = self.realm.upgrade().map_or_else(ScreenMetrics::default, |realm| *realm.screen.borrow());
        metrics.orientation.as_str().to_owned()
    }

    #[getter]
    fn angle(&self) -> u32 {
        let metrics = self.realm.upgrade().map_or_else(ScreenMetrics::default, |realm| *realm.screen.borrow());
        u32::from(metrics.orientation_angle)
    }
}

/// Creates `window.screen` for a document realm and publishes the `Screen`
/// and `ScreenOrientation` interfaces.
pub(crate) fn install(ctx: &mut Ctx, realm: &Rc<DomRealm>) -> OpResult<()> {
    let global = ctx.global_object();
    let screen_constructor = ctx.class_constructor::<DomScreen>();
    let orientation_constructor = ctx.class_constructor::<DomScreenOrientation>();
    crate::install_interface(ctx, &global, "Screen", screen_constructor)
        .and_then(|_| crate::install_interface(ctx, &global, "ScreenOrientation", orientation_constructor))
        .map_err(|_| OpError::new("Error", "Screen installation failed"))?;
    let orientation = ctx.new_instance(DomScreenOrientation { realm: Rc::downgrade(realm) });
    let screen = ctx.new_instance(DomScreen { realm: Rc::downgrade(realm) });
    ctx.set_native_internal_value_slot(&screen, ORIENTATION_SLOT, orientation).map_err(OpError::thrown)?;
    ctx.set_native_internal_value_slot(&global, SCREEN_SLOT, screen).map_err(OpError::thrown)
}

/// The `screen` object of the window whose global is `global`.
pub(crate) fn value(ctx: &mut Ctx, global: &Value) -> Value {
    ctx.native_private_value_slot(global, SCREEN_SLOT).unwrap_or(Value::Undefined)
}

impl DomRealm {
    /// Display geometry reported by `window.screen`.
    pub fn set_screen(&self, metrics: ScreenMetrics) {
        *self.screen.borrow_mut() = metrics;
    }

    pub fn screen_metrics(&self) -> ScreenMetrics {
        *self.screen.borrow()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen::Engine;

    fn eval_bool(engine: &mut Engine, source: &str) -> bool {
        matches!(engine.eval_value(source), Ok(Ok(Value::Bool(true))))
    }

    #[test]
    fn screen_reports_defaults_and_embedder_metrics() {
        let mut engine = Engine::new();
        let realm = crate::install(engine.ctx(), "<body></body>", 64).unwrap();
        assert!(eval_bool(&mut engine, "screen === window.screen && screen instanceof Screen"));
        assert!(eval_bool(
            &mut engine,
            "screen.width === 1280 && screen.height === 720 && screen.availWidth === 1280 && screen.availHeight === 720 \
             && screen.availLeft === 0 && screen.availTop === 0 && screen.colorDepth === 24 && screen.pixelDepth === 24"
        ));
        assert!(eval_bool(
            &mut engine,
            "screen.orientation instanceof ScreenOrientation && screen.orientation === screen.orientation \
             && screen.orientation.type === 'landscape-primary' && screen.orientation.angle === 0"
        ));
        assert!(eval_bool(&mut engine, "try { new Screen(); false } catch (e) { e instanceof TypeError }"));

        let mut metrics = ScreenMetrics::new(800, 1200);
        metrics.avail_height = 1160;
        metrics.avail_top = 40;
        metrics.color_depth = 30;
        realm.set_screen(metrics);
        assert!(eval_bool(
            &mut engine,
            "screen.width === 800 && screen.height === 1200 && screen.availHeight === 1160 && screen.availTop === 40 \
             && screen.colorDepth === 30 && screen.pixelDepth === 30 && screen.orientation.type === 'portrait-primary'"
        ));
    }

    #[test]
    fn screen_is_replaceable_on_window() {
        let mut engine = Engine::new();
        crate::install(engine.ctx(), "<body></body>", 64).unwrap();
        assert!(eval_bool(&mut engine, "window.screen = 5; window.screen === 5"));
    }
}
