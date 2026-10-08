//! Native SVG prototype identities with one inherited Node/wrapper state.
//! Animated properties and geometry operations are separate from these DOM identities.
use super::{dom_error, DomElement, DomNode};
use lumen::embed::{Ctx, OpResult, Value};
use lumen_html::{selector, svg_dom, NodeId};

trait FromNode {
    fn from_node(node: DomNode) -> Self;
}

impl FromNode for DomElement {
    fn from_node(node: DomNode) -> Self { Self { base: node } }
}

macro_rules! svg_class {
    ($ty:ident, $name:literal, $base:ident) => {
        #[lumen_bind::class(name = $name, extends = $base, hint(js(webidl)))]
        pub(crate) struct $ty { base: $base }
        impl FromNode for $ty {
            fn from_node(node: DomNode) -> Self { Self { base: $base::from_node(node) } }
        }
    };
}

svg_class!(DomSvgElement, "SVGElement", DomElement);
crate::event_content_handlers::bind_namespace_handlers! { DomSvgElement {
    #[getter]
    fn dataset(&self, ctx: &mut Ctx) -> OpResult<Value> {
        crate::dataset::for_element(ctx, &self.base.base)
    }

    #[getter]
    fn nonce(&self)->OpResult<String> {self.base.base.cryptographic_nonce()}
    #[setter(coerce)]
    fn set_nonce(&self,value:&str)->OpResult<()> {self.base.base.set_cryptographic_nonce(value)}


    #[getter]
    fn style(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> Value {
        self.base.base.style(ctx, this)
    }

    #[setter(coerce)]
    fn set_style(&self, value: &str) -> OpResult<()> {
        self.base.base.set_style(value)
    }

    #[getter(name = "ownerSVGElement")]
    fn owner_svg_element(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let node = &self.base.base;
        let owner = svg_dom::owner_svg_element(node.realm.session.borrow().document(), node.id)
            .map_err(dom_error)?;
        Ok(node.realm.wrap_option(ctx, owner))
    }
} }

svg_class!(DomSvgGraphicsElement, "SVGGraphicsElement", DomSvgElement);
#[lumen_bind::methods]
impl DomSvgGraphicsElement {}
svg_class!(DomSvgGeometryElement, "SVGGeometryElement", DomSvgGraphicsElement);
#[lumen_bind::methods]
impl DomSvgGeometryElement {}
svg_class!(DomSvgTextContentElement, "SVGTextContentElement", DomSvgGraphicsElement);
#[lumen_bind::methods]
impl DomSvgTextContentElement {}
svg_class!(DomSvgTextPositioningElement, "SVGTextPositioningElement", DomSvgTextContentElement);
#[lumen_bind::methods]
impl DomSvgTextPositioningElement {}
svg_class!(DomSvgGradientElement, "SVGGradientElement", DomSvgElement);
#[lumen_bind::methods]
impl DomSvgGradientElement {}
svg_class!(DomSvgSvgElement, "SVGSVGElement", DomSvgGraphicsElement);
#[lumen_bind::methods]
impl DomSvgSvgElement {
    #[method(coerce)]
    fn get_element_by_id(&self, ctx: &mut Ctx, id: &str) -> OpResult<Value> {
        let node = &self.base.base.base.base;
        let found = selector::get_element_by_id(node.realm.session.borrow().document(), node.id, id)
            .map_err(dom_error)?;
        Ok(node.realm.wrap_option(ctx, found))
    }
}

macro_rules! svg_element_classes {
    ($( $ty:ident, $interface:literal, $base:ident => $tag:literal; )+) => {
        $(
            svg_class!($ty, $interface, $base);
            #[lumen_bind::methods]
            impl $ty {}
        )+
        pub(crate) fn wrap(ctx: &mut Ctx, id: NodeId, node: DomNode, local: &str) -> Value {
            match local {
                "svg" => ctx.cached_instance(id, || DomSvgSvgElement::from_node(node)),
                $( $tag => ctx.cached_instance(id, || $ty::from_node(node)), )+
                _ => ctx.cached_instance(id, || DomSvgElement::from_node(node)),
            }
        }
        pub(crate) fn constructors(ctx: &mut Ctx) -> Vec<(&'static str, Value)> {
            vec![
                ("SVGElement", ctx.class_constructor::<DomSvgElement>()),
                ("SVGGraphicsElement", ctx.class_constructor::<DomSvgGraphicsElement>()),
                ("SVGGeometryElement", ctx.class_constructor::<DomSvgGeometryElement>()),
                ("SVGTextContentElement", ctx.class_constructor::<DomSvgTextContentElement>()),
                ("SVGTextPositioningElement", ctx.class_constructor::<DomSvgTextPositioningElement>()),
                ("SVGGradientElement", ctx.class_constructor::<DomSvgGradientElement>()),
                ("SVGSVGElement", ctx.class_constructor::<DomSvgSvgElement>()),
                $( ($interface, ctx.class_constructor::<$ty>()), )+
            ]
        }
    };
}

svg_element_classes! {
    DomSvgGElement, "SVGGElement", DomSvgGraphicsElement => "g";
    DomSvgDefsElement, "SVGDefsElement", DomSvgGraphicsElement => "defs";
    DomSvgSymbolElement, "SVGSymbolElement", DomSvgGraphicsElement => "symbol";
    DomSvgUseElement, "SVGUseElement", DomSvgGraphicsElement => "use";
    DomSvgSwitchElement, "SVGSwitchElement", DomSvgGraphicsElement => "switch";
    DomSvgImageElement, "SVGImageElement", DomSvgGraphicsElement => "image";
    DomSvgForeignObjectElement, "SVGForeignObjectElement", DomSvgGraphicsElement => "foreignObject";
    DomSvgAElement, "SVGAElement", DomSvgGraphicsElement => "a";
    DomSvgPathElement, "SVGPathElement", DomSvgGeometryElement => "path";
    DomSvgRectElement, "SVGRectElement", DomSvgGeometryElement => "rect";
    DomSvgCircleElement, "SVGCircleElement", DomSvgGeometryElement => "circle";
    DomSvgEllipseElement, "SVGEllipseElement", DomSvgGeometryElement => "ellipse";
    DomSvgLineElement, "SVGLineElement", DomSvgGeometryElement => "line";
    DomSvgPolylineElement, "SVGPolylineElement", DomSvgGeometryElement => "polyline";
    DomSvgPolygonElement, "SVGPolygonElement", DomSvgGeometryElement => "polygon";
    DomSvgTextElement, "SVGTextElement", DomSvgTextPositioningElement => "text";
    DomSvgTSpanElement, "SVGTSpanElement", DomSvgTextPositioningElement => "tspan";
    DomSvgTextPathElement, "SVGTextPathElement", DomSvgTextContentElement => "textPath";
    DomSvgDescElement, "SVGDescElement", DomSvgElement => "desc";
    DomSvgMetadataElement, "SVGMetadataElement", DomSvgElement => "metadata";
    DomSvgTitleElement, "SVGTitleElement", DomSvgElement => "title";
    DomSvgStyleElement, "SVGStyleElement", DomSvgElement => "style";
    DomSvgMarkerElement, "SVGMarkerElement", DomSvgElement => "marker";
    DomSvgLinearGradientElement, "SVGLinearGradientElement", DomSvgGradientElement => "linearGradient";
    DomSvgRadialGradientElement, "SVGRadialGradientElement", DomSvgGradientElement => "radialGradient";
    DomSvgStopElement, "SVGStopElement", DomSvgElement => "stop";
    DomSvgPatternElement, "SVGPatternElement", DomSvgElement => "pattern";
    DomSvgScriptElement, "SVGScriptElement", DomSvgElement => "script";
    DomSvgViewElement, "SVGViewElement", DomSvgElement => "view";
}
