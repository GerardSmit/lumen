//! SVG `<mask>` references from the `mask` property on SVG graphics.
use super::*;

/// The `<mask>` element a lone `mask-image: url(#id)` layer names.
pub(super) fn svg_mask_node(document: &Document, style: &Style) -> Result<Option<NodeId>, LayoutError> {
    let Some([crate::paint::BackgroundImage::Url(url)]) = style.mask_images.as_deref() else {
        return Ok(None);
    };
    let Some((_, id)) = url.rsplit_once('#') else {
        return Ok(None);
    };
    let Some(node) = svg_element_with_id(document, id)? else {
        return Ok(None);
    };
    Ok(matches!(document.kind(node),
        Ok(NodeKind::Element { namespace: Namespace::Svg, name, .. }) if crate::svg::local_name(name) == "mask")
    .then_some(node))
}

pub(super) fn svg_mask_supported(document: &Document, style: &Style) -> Result<bool, LayoutError> {
    if !style.has_mask() {
        return Ok(true);
    }
    let Some([crate::paint::BackgroundImage::Url(url)]) = style.mask_images.as_deref() else {
        return Ok(true);
    };
    Ok(!url.contains('#') || svg_mask_node(document, style)?.is_some())
}

fn unit_fraction(value: &str) -> Option<f32> {
    let value = value.trim();
    let number = match value.strip_suffix('%') {
        Some(percent) => percent.trim().parse::<f32>().ok()? / 100.0,
        None => value.parse::<f32>().ok()?,
    };
    number.is_finite().then_some(number)
}

fn empty_mask() -> Arc<crate::paint::LayerMask> {
    Arc::new(crate::paint::LayerMask { layers: Arc::from([]), offset: [0.0; 2] })
}

impl Layout<'_> {
    /// Attaches the owner's referenced `<mask>` to its layer clip, creating a
    /// shape-less clip carrier when the owner has no `clip-path`.
    pub(super) fn with_svg_layer_mask(
        &mut self,
        clip: Option<Arc<crate::paint::SvgLayerClip>>,
        node: NodeId,
        style: &Style,
        transform: Affine,
        user_box: crate::svg::ViewBox,
        depth: usize,
        use_chain: &mut Vec<NodeId>,
    ) -> Result<Option<Arc<crate::paint::SvgLayerClip>>, LayoutError> {
        if !style.has_mask() {
            return Ok(clip);
        }
        let Some(mask) = self.svg_layer_mask(node, style, transform, user_box, depth, use_chain)? else {
            return Ok(clip);
        };
        Ok(Some(Arc::new(match clip {
            Some(owner) => crate::paint::SvgLayerClip { mask: Some(mask), ..(*owner).clone() },
            None => crate::paint::SvgLayerClip {
                transform,
                clip: Arc::new(crate::paint::SvgClip {
                    units: crate::paint::SvgGradientUnits::UserSpaceOnUse,
                    transform: Affine::IDENTITY,
                    shapes: Arc::from([]),
                }),
                mask: Some(mask),
            },
        })))
    }

    fn svg_layer_mask(
        &mut self,
        node: NodeId,
        style: &Style,
        transform: Affine,
        user_box: crate::svg::ViewBox,
        depth: usize,
        use_chain: &mut Vec<NodeId>,
    ) -> Result<Option<Arc<crate::paint::LayerMask>>, LayoutError> {
        let Some(mask_node) = svg_mask_node(self.document, style)? else {
            return Ok(None);
        };
        if use_chain.contains(&mask_node) || use_chain.len() >= 32 || depth > 512 {
            return Ok(Some(empty_mask()));
        }
        let NodeKind::Element { attributes, .. } = self.document.kind(mask_node).map_err(|_| LayoutError::InvalidTree)? else {
            return Ok(None);
        };
        let attribute = |name: &str| crate::svg::attribute(attributes, name);
        let region_in_box = attribute("maskUnits") != Some("userSpaceOnUse");
        let content_in_box = attribute("maskContentUnits") == Some("objectBoundingBox");
        let bbox = if region_in_box || content_in_box {
            let boxes = match self.svg_transform_geometry(node, style, user_box, depth, use_chain) {
                Ok(boxes) => boxes.map(|(fill, _)| fill),
                Err(LayoutError::Text) => None,
                Err(error) => return Err(error),
            };
            let root = matches!(self.document.kind(node),
                Ok(NodeKind::Element { namespace: Namespace::Svg, name, .. }) if crate::svg::local_name(name) == "svg");
            boxes.or_else(|| {
                root.then_some(Rect { x: user_box.min_x, y: user_box.min_y, width: user_box.width, height: user_box.height })
            })
        } else {
            None
        };
        let usable_box = bbox.filter(|rect| rect.width > 0.0 && rect.height > 0.0);
        if (region_in_box || content_in_box) && usable_box.is_none() {
            return Ok(Some(empty_mask()));
        }
        let region = match usable_box.filter(|_| region_in_box) {
            Some(rect) => {
                let fraction = |name: &str, default: f32| attribute(name).and_then(unit_fraction).unwrap_or(default);
                Rect {
                    x: rect.x + fraction("x", -0.1) * rect.width,
                    y: rect.y + fraction("y", -0.1) * rect.height,
                    width: fraction("width", 1.2) * rect.width,
                    height: fraction("height", 1.2) * rect.height,
                }
            }
            None => {
                let length = |name: &str, basis: f32, default: f32| {
                    attribute(name).and_then(|value| crate::svg::coordinate_value(value, basis)).unwrap_or(default * basis)
                };
                Rect {
                    x: length("x", user_box.width, -0.1),
                    y: length("y", user_box.height, -0.1),
                    width: length("width", user_box.width, 1.2),
                    height: length("height", user_box.height, 1.2),
                }
            }
        };
        if ![region.x, region.y, region.width, region.height].iter().all(|value| value.is_finite())
            || region.width <= 0.0
            || region.height <= 0.0
        {
            return Ok(Some(empty_mask()));
        }
        let content = match usable_box.filter(|_| content_in_box) {
            Some(rect) => transform.then(Affine { a: rect.width, d: rect.height, e: rect.x, f: rect.y, ..Affine::IDENTITY }),
            None => transform,
        };
        let mask_style = self.computed_style_from_tree(mask_node)?;
        let luminance = css::extra_props::keyword(&mask_style, "mask-type") != Some("alpha");

        let saved_commands = core::mem::take(&mut self.commands);
        let saved_bytes = core::mem::replace(&mut self.command_bytes, 0);
        let saved_geometry = self.geometry.take();
        let saved_source = self.svg_filter_source_transform;
        use_chain.push(mask_node);
        let painted = self.paint_svg_mask_content(mask_node, &mask_style, region, transform, content, user_box, depth, use_chain);
        use_chain.pop();
        self.svg_filter_source_transform = saved_source;
        self.geometry = saved_geometry;
        let list = core::mem::replace(&mut self.commands, saved_commands);
        self.command_bytes = saved_bytes;
        painted?;

        let layers: Vec<crate::paint::MaskLayer> = if list.is_empty() {
            Vec::new()
        } else {
            alloc::vec![crate::paint::MaskLayer { list: Arc::new(DisplayList(list)), luminance }]
        };
        Ok(Some(Arc::new(crate::paint::LayerMask { layers: Arc::from(layers), offset: [0.0; 2] })))
    }

    fn paint_svg_mask_content(
        &mut self,
        mask_node: NodeId,
        mask_style: &Style,
        region: Rect,
        transform: Affine,
        content: Affine,
        user_box: crate::svg::ViewBox,
        depth: usize,
        use_chain: &mut Vec<NodeId>,
    ) -> Result<(), LayoutError> {
        let data = alloc::format!("M{} {}h{}v{}h{}z", region.x, region.y, region.width, region.height, -region.width);
        let clip = Arc::new(crate::paint::SvgClip {
            units: crate::paint::SvgGradientUnits::UserSpaceOnUse,
            transform: Affine::IDENTITY,
            shapes: Arc::from([crate::paint::SvgClipShape {
                data: Arc::from(data),
                transform: Affine::IDENTITY,
                fill_rule: crate::paint::SvgFillRule::NonZero,
            }]),
        });
        let bounds = transform.bounds(region);
        self.push_command(Command::PushLayer {
            svg_clip: Some(Arc::new(crate::paint::SvgLayerClip { transform, clip, mask: None })),
            filters: None,
            corners: None,
            rect: bounds,
            radius: 0.0,
            opacity: 1.0,
            clip: false,
        })?;
        let result = self.paint_svg_children(mask_node, mask_style, content, bounds, 1.0, user_box, depth + 1, use_chain, &[]);
        self.push_command(Command::PopLayer)?;
        result
    }
}
