//! Prints `size_of` of the types that dominate per-node and per-rule memory.
//! Run: cargo test -p lumen-html --target aarch64-apple-darwin --lib -- --nocapture print_type_sizes
extern crate std;

use crate::{css, layout, paint, Name, NodeId, NodeKind};
use core::mem::{align_of, size_of};

macro_rules! sizes {
    ($($ty:ty),+ $(,)?) => {
        $(std::println!("{:<58} size {:>5}  align {}", stringify!($ty), size_of::<$ty>(), align_of::<$ty>());)+
    };
}

#[test]
fn print_type_sizes() {
    std::println!("--- style (per distinct computed style)");
    sizes!(css::Style, css::StyleExtras, css::CustomProperties, css::FontUnitBases, css::GeneratedStyle);
    sizes!(paint::FontSpec, paint::Rgba, paint::BoxShadow, css::Outline, css::GridPlacement, css::LineHeight);
    sizes!(css::Transform, css::GridTrack, css::IntrinsicSizing, css::BorderRadiusCorner, css::SourceColor);
    std::println!("--- style Option/Arc handles");
    sizes!(
        Option<alloc::sync::Arc<css::Style>>,
        Option<alloc::sync::Arc<css::StyleExtras>>,
        Option<alloc::sync::Arc<str>>,
        Option<alloc::sync::Arc<[css::Transform]>>
    );
    std::println!("--- rules (per CSS rule)");
    sizes!(css::Rule, css::Declaration, css::Selector, css::StylesheetIdentity, css::StylesheetSource);
    sizes!(css::registered_properties::RegisteredCustomProperty, Option<[usize; 8]>);
    std::println!("--- dom (per node)");
    sizes!(NodeId, NodeKind, Name, crate::Node, crate::Dirty);
    sizes!(alloc::vec::Vec<(Name, alloc::string::String)>, (Name, alloc::string::String));
    std::println!("--- display list / geometry");
    sizes!(paint::Command, paint::DisplayList, paint::Glyph, paint::BackgroundFill, paint::BoxBorder, paint::MaskedBackground);
    sizes!(layout::LayoutGeometry, layout::HitRegion, layout::HitTransform, layout::HitClip, layout::UsedMargins);
    sizes!(layout::ScrollRegion, layout::RenderedTextBox, layout::ControlTextRun, layout::ScrollBounds, layout::ScrollPort);
}
