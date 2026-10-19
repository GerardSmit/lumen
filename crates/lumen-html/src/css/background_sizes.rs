use super::*;

/// Only percentage-dependent nonlinear sizes need a persistent expression.
/// Affine lengths retain the existing Copy paint carrier and sampling path.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BackgroundSizeMath {
    pub(super) kind: BackgroundSizeKind,
    pub(super) width: Option<DecorationLength>,
    pub(super) height: Option<DecorationLength>,
}

pub(super) fn contains_math(raw: &str) -> bool {
    top_level_split(raw, b',', MAX_BACKGROUND_GEOMETRY_VALUES).is_some_and(|parts| {
        parts.iter().any(|part| {
            components(part).is_some_and(|tokens| tokens.iter().any(|token| math_function(token)))
        })
    })
}

pub(super) fn uses_font(raw: &str) -> bool {
    top_level_split(raw, b',', MAX_BACKGROUND_GEOMETRY_VALUES).is_some_and(|parts| {
        parts.iter().any(|part| {
            components(part).is_some_and(|tokens| {
                tokens.iter().any(|token| {
                    typed_numeric::parse_numeric_expression(token)
                        .is_some_and(|value| value.contains_unit(font_relative_unit))
                })
            })
        })
    })
}

fn computed_sizes(
    raw: &str,
    context: LengthContext,
    query: ContainerUnitContext,
) -> Option<(
    Option<Arc<[BackgroundSize]>>,
    Option<Arc<[BackgroundSizeMath]>>,
)> {
    let mut sizes = Vec::new();
    let mut math = Vec::new();
    let mut nonlinear = false;
    for part in top_level_split(raw, b',', MAX_BACKGROUND_GEOMETRY_VALUES)? {
        let tokens = components(part)?;
        let kind = match tokens.as_slice() {
            [value] if value.eq_ignore_ascii_case("cover") => BackgroundSizeKind::Cover,
            [value] if value.eq_ignore_ascii_case("contain") => BackgroundSizeKind::Contain,
            [_] | [_, _] => BackgroundSizeKind::Explicit,
            _ => return None,
        };
        let axis = |token: Option<&&str>| -> Option<Option<DecorationLength>> {
            let Some(token) = token else {
                return Some(None);
            };
            if token.eq_ignore_ascii_case("auto") {
                return Some(None);
            }
            if negative_length_primitive(token) {
                return None;
            }
            Some(Some(decoration_length_with_context(token, context, query)?))
        };
        let (width, height) = if kind == BackgroundSizeKind::Explicit {
            (axis(tokens.first())?, axis(tokens.get(1))?)
        } else {
            (None, None)
        };
        nonlinear |= [&width, &height]
            .into_iter()
            .flatten()
            .any(|value| matches!(value, DecorationLength::Expression(_)));
        let affine = |value: &Option<DecorationLength>| match value {
            Some(DecorationLength::Length(value)) => Some(value.into_used()),
            // The persistent AST supplies this axis before any paint command.
            Some(_) => Some(LengthPercentage::default()),
            None => None,
        };
        sizes.try_reserve(1).ok()?;
        math.try_reserve(1).ok()?;
        sizes.push(BackgroundSize {
            kind,
            width: affine(&width),
            height: affine(&height),
        });
        math.push(BackgroundSizeMath {
            kind,
            width,
            height,
        });
    }
    let auto = sizes.iter().all(|value| *value == BackgroundSize::AUTO);
    Some((
        (!auto).then(|| Arc::from(sizes)),
        nonlinear.then(|| Arc::from(math)),
    ))
}

impl Style {
    pub(super) fn compute_background_sizes(
        &mut self,
        raw: &str,
        context: LengthContext,
        query: ContainerUnitContext,
    ) -> bool {
        let Some((sizes, math)) = computed_sizes(raw, context, query) else {
            return false;
        };
        self.background_size = sizes;
        self.background_size_math = math;
        true
    }

    /// Resolve only nonlinear axes against the actual layer positioning area.
    /// This uses a retained bounded AST, with no parsing or allocation per paint.
    pub(crate) fn background_size_for_area(
        &self,
        index: usize,
        width: f32,
        height: f32,
        fallback: BackgroundSize,
    ) -> BackgroundSize {
        let Some(values) = self
            .background_size_math
            .as_deref()
            .filter(|values| !values.is_empty())
        else {
            return fallback;
        };
        let value = &values[index % values.len()];
        let axis = |value: &Option<DecorationLength>, basis| {
            value.as_ref().map(|value| LengthPercentage {
                pixels: value.used(basis).unwrap_or(0.0).max(0.0),
                fraction: 0.0,
            })
        };
        BackgroundSize {
            kind: value.kind,
            width: axis(&value.width, width),
            height: axis(&value.height, height),
        }
    }

    pub(crate) fn background_sizes_computed_value(&self) -> Option<String> {
        let Some(values) = self.background_size_math.as_deref() else {
            return computed_values::background_sizes_css_value(self.background_size.as_deref());
        };
        let mut output = String::new();
        for (index, value) in values.iter().enumerate() {
            if index != 0 {
                output.push_str(", ");
            }
            match value.kind {
                BackgroundSizeKind::Cover => output.push_str("cover"),
                BackgroundSizeKind::Contain => output.push_str("contain"),
                BackgroundSizeKind::Explicit => {
                    output.push_str(
                        &value
                            .width
                            .as_ref()
                            .map_or_else(|| "auto".into(), DecorationLength::computed),
                    );
                    output.push(' ');
                    output.push_str(
                        &value
                            .height
                            .as_ref()
                            .map_or_else(|| "auto".into(), DecorationLength::computed),
                    );
                }
            }
            if output.len() > MAX_VARIABLE_BYTES {
                return None;
            }
        }
        Some(output)
    }

    pub(crate) fn interpolate_background_size_math(
        &self,
        other: &Self,
        progress: f64,
    ) -> Option<String> {
        let a_len = self.background_size_math.as_ref().map_or_else(
            || {
                self.background_size
                    .as_ref()
                    .map_or(1, |values| values.len())
            },
            |values| values.len(),
        );
        let b_len = other.background_size_math.as_ref().map_or_else(
            || {
                other
                    .background_size
                    .as_ref()
                    .map_or(1, |values| values.len())
            },
            |values| values.len(),
        );
        let mut gcd = a_len;
        let mut remainder = b_len;
        while remainder != 0 {
            let next = gcd % remainder;
            gcd = remainder;
            remainder = next;
        }
        let count = a_len.checked_div(gcd)?.checked_mul(b_len)?;
        if count > MAX_BACKGROUND_GEOMETRY_VALUES {
            return None;
        }
        let value = |style: &Self, index: usize| -> BackgroundSizeMath {
            if let Some(values) = style.background_size_math.as_deref() {
                return values[index % values.len()].clone();
            }
            let value = style
                .background_size
                .as_deref()
                .map_or(BackgroundSize::AUTO, |values| values[index % values.len()]);
            BackgroundSizeMath {
                kind: value.kind,
                width: value.width.map(|value|DecorationLength::Length(TransformLength::from_used(value))),
                height: value.height.map(|value|DecorationLength::Length(TransformLength::from_used(value))),
            }
        };
        let axis = |a: &Option<DecorationLength>, b: &Option<DecorationLength>| match (a, b) {
            (Some(a), Some(b)) => a.interpolate_css_value(b, progress),
            (None, None) => Some("auto".into()),
            _ => None,
        };
        let mut output = String::new();
        for index in 0..count {
            let (a, b) = (value(self, index), value(other, index));
            if a.kind != b.kind {
                return None;
            }
            if index != 0 {
                output.push_str(", ");
            }
            match a.kind {
                BackgroundSizeKind::Cover => output.push_str("cover"),
                BackgroundSizeKind::Contain => output.push_str("contain"),
                BackgroundSizeKind::Explicit => {
                    output.push_str(&axis(&a.width, &b.width)?);
                    output.push(' ');
                    output.push_str(&axis(&a.height, &b.height)?);
                }
            }
            if output.len() > MAX_VARIABLE_BYTES {
                return None;
            }
        }
        Some(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn style(raw: &str) -> Style {
        let element = NodeKind::Element {
            namespace: Namespace::Html,
            name: "div".into(),
            attributes: alloc::vec![("style".into(), raw.into())],
        };
        compute(&element, None, &StyleIndex::new(Vec::new())).unwrap()
    }
    #[test]
    fn specification_background_size_math_uses_actual_area_and_computed_context() {
        for source in [
            "background-size:calc(100% - 10px) 20px",
            "background:0/calc(100% - 10px) 20px",
        ] {
            let computed = style(source);
            for width in [5.0f32, 50.0, 200.0] {
                let size = computed.background_size_for_area(
                    0,
                    width,
                    100.0,
                    computed.background_size.as_ref().unwrap()[0],
                );
                assert_eq!(
                    size.width.unwrap().resolve(width).max(0.0),
                    (width - 10.0).max(0.0)
                );
                assert_eq!(size.height.unwrap().resolve(100.0), 20.0);
            }
        }
        assert!(!supports_declaration("background-size", "-1px 20px"));
        assert!(!supports_declaration("background-size", "-1em 20px"));
        assert!(supports_declaration("background-size", "calc(-10px) 20px"));
        let computed = style("background-size:min(50%, 80px) clamp(10px, 25%, 50px)");
        assert!(computed.background_size_math.is_some());
        for (area, expected) in [
            (40.0, (20.0, 10.0)),
            (100.0, (50.0, 25.0)),
            (400.0, (80.0, 50.0)),
        ] {
            let size = computed.background_size_for_area(0, area, area, BackgroundSize::AUTO);
            assert_eq!(
                (size.width.unwrap().pixels, size.height.unwrap().pixels),
                expected
            );
        }
        let serialized = computed.background_sizes_computed_value().unwrap();
        let roundtrip = style(&alloc::format!("background-size:{serialized}"));
        assert_eq!(
            computed.background_size_for_area(0, 400.0, 400.0, BackgroundSize::AUTO),
            roundtrip.background_size_for_area(0, 400.0, 400.0, BackgroundSize::AUTO)
        );
        let mut pending = style("font-size:10cqw;background-size:min(50%, 2em) calc(25% + 10cqw)");
        assert!(pending.property_query_context_pending("background-size"));
        assert!(!pending.resolve_query_context(ContainerUnitContext::default()));
        let query = ContainerUnitContext {
            width: ContainerUnitBasis::Size(300.0),
            height: ContainerUnitBasis::Size(200.0),
            ..ContainerUnitContext::default()
        };
        assert!(pending.resolve_query_context(query));
        assert_eq!(pending.font_size, 30.0);
        let size = pending.background_size_for_area(
            0,
            200.0,
            100.0,
            pending.background_size.as_ref().unwrap()[0],
        );
        assert_eq!(
            (size.width.unwrap().pixels, size.height.unwrap().pixels),
            (60.0, 55.0)
        );
        let resolved = pending.resolve_percentages_with_query(
            999.0,
            Some(888.0),
            ContainerUnitContext::default(),
        );
        assert_eq!(
            size,
            resolved.background_size_for_area(
                0,
                200.0,
                100.0,
                resolved.background_size.as_ref().unwrap()[0]
            ),
            "background percentages belong to the positioning area, not the containing block"
        );
        let inherited = NodeKind::Element {
            namespace: Namespace::Html,
            name: "div".into(),
            attributes: alloc::vec![(
                "style".into(),
                "font-size:100px;background-size:inherit".into()
            )],
        };
        let inherited = compute(&inherited, Some(&pending), &StyleIndex::new(Vec::new())).unwrap();
        assert_eq!(
            size,
            inherited.background_size_for_area(
                0,
                200.0,
                100.0,
                inherited.background_size.as_ref().unwrap()[0]
            )
        );
    }
    #[test]
    fn specification_background_size_math_transition_samples_nonlinear_endpoints() {
        let a = style("background-size:min(50%, 80px) 20px, 10px auto");
        let b = style("background-size:max(25%, 60px) 40px, 30px auto");
        let sample = a.interpolate_background_size_math(&b, 0.5).unwrap();
        let middle = style(&alloc::format!("background-size:{sample}"));
        for (area, width) in [(40.0, 40.0), (100.0, 55.0), (400.0, 90.0)] {
            let size = middle.background_size_for_area(
                0,
                area,
                100.0,
                middle.background_size.as_ref().unwrap()[0],
            );
            assert_eq!(
                (size.width.unwrap().pixels, size.height.unwrap().pixels),
                (width, 30.0)
            );
        }
        let compound = crate::animation::transition_values::interpolate_compound_transition(
            "background-size",
            &a,
            &b,
            0.5,
            |a, _, _| a,
        )
        .unwrap();
        assert_eq!(compound, sample);
        assert!(a
            .interpolate_background_size_math(&style("background-size:cover"), 0.5)
            .is_none());
        assert!(
            a.checked_private_payload_bytes().unwrap()
                > Style::initial().checked_private_payload_bytes().unwrap()
        );
    }
}
