//! Chromium html.css defaults for push buttons and drop-down boxes. Colors
//! follow the used color scheme in the cascade's foreground pass.

use super::{
    BorderStyle, FontFamily, FontLigatures, FontSpec, FontStyle, FontVariantCaps, Rgba, Style, WhiteSpace,
};

/// The properties the helpers below overwrite, kept so `appearance: base`
/// (which opts out of the native control look) can restore the inherited values.
pub(super) struct Saved {
    font_size: f32,
    font: FontSpec,
    line_height: super::LineHeight,
    padding_sides: [f32; 4],
    border_width: f32,
    border_style: BorderStyle,
    border_solid: bool,
    border_color: Rgba,
    background: Rgba,
    color: Rgba,
    white_space: WhiteSpace,
}

impl Saved {
    pub(super) fn capture(style: &Style) -> Self {
        Self {
            font_size: style.font_size,
            font: style.font.clone(),
            line_height: style.line_height.clone(),
            padding_sides: style.padding_sides,
            border_width: style.border_width,
            border_style: style.border_style,
            border_solid: style.border_solid,
            border_color: style.border_color,
            background: style.background,
            color: style.color,
            white_space: style.white_space,
        }
    }

    pub(super) fn restore(self, style: &mut Style) {
        style.font_size = self.font_size;
        style.font = self.font;
        style.line_height = self.line_height;
        style.padding_sides = self.padding_sides;
        style.border_width = self.border_width;
        style.border_style = self.border_style;
        style.border_solid = self.border_solid;
        style.border_color = self.border_color;
        style.background = self.background;
        style.color = self.color;
        style.white_space = self.white_space;
    }
}

/// `font: -webkit-small-control` as Chromium resolves it on form controls.
pub(super) fn small_control_font(style: &mut Style) {
    style.font_size = 13.333_333;
    style.font = FontSpec {
        ligatures: FontLigatures::NORMAL,
        feature_settings: None,
        alternates: None,
        disable_optional_ligatures: false,
        family_scope: None,
        families: Some(alloc::vec![FontFamily::from("Arial")].into()),
        weight: 400,
        weight_fraction: 0,
        style: FontStyle::Normal,
        unresolved_style: None,
        unresolved_stretch: None,
        stretch: 100.0,
        size_adjust: None,
        caps: FontVariantCaps::Normal,
        synthesize_small_caps: true,
        spacing_trim: style.font.spacing_trim,
    };
    style.line_height = super::LineHeight::Normal;
}

/// `button, input[type=button|submit|reset] { padding: 1px 6px; border: 2px outset ButtonBorder }`.
pub(super) fn push_button_box(style: &mut Style) {
    style.padding_sides = [1.0, 6.0, 1.0, 6.0];
    style.border_width = 2.0;
    style.border_style = BorderStyle::Outset;
    style.border_solid = true;
    style.border_color = Rgba { r: 118, g: 118, b: 118, a: 255 };
    style.background = Rgba { r: 239, g: 239, b: 239, a: 255 };
    style.color = Rgba { r: 0, g: 0, b: 0, a: 255 };
}

/// `select { border: 1px solid #767676; background-color: Field; color: FieldText; white-space: pre }`.
pub(super) fn select_box(style: &mut Style) {
    style.border_width = 1.0;
    style.border_style = BorderStyle::Solid;
    style.border_solid = true;
    style.border_color = Rgba { r: 118, g: 118, b: 118, a: 255 };
    style.background = Rgba { r: 255, g: 255, b: 255, a: 255 };
    style.white_space = WhiteSpace::Pre;
}

#[cfg(test)]
mod tests {
    use super::super::{compute_node, parse, BorderStyle, Style, StyleIndex};

    fn computed(markup: &str, css: &str, id: &str) -> Style {
        let document = crate::html::parse(markup, 64).unwrap();
        let index = StyleIndex::new(parse(css).unwrap());
        let node = crate::selector::get_element_by_id(&document, document.root(), id).unwrap().unwrap();
        let mut chain = alloc::vec![node];
        while let Some(parent) = document.parent(*chain.last().unwrap()).unwrap() {
            if !matches!(document.kind(parent), Ok(crate::NodeKind::Element { .. })) { break; }
            chain.push(parent);
        }
        let mut parent: Option<Style> = None;
        for node in chain.into_iter().rev() {
            parent = Some(compute_node(&document, node, parent.as_ref(), &index).unwrap());
        }
        parent.unwrap()
    }

    const PAGE: &str = "<!doctype html><button id=b>x</button><select id=s></select><input id=u type=submit><input id=t><textarea id=a></textarea>";
    const DARK_TEXT: &str = "body{color:#fff;font:20px serif}";

    #[test]
    fn button_ignores_inherited_text_styles() {
        let button = computed(PAGE, DARK_TEXT, "b");
        assert_eq!((button.color.r, button.background.r), (0, 239));
        assert_eq!(button.font_size, 13.333_333);
        assert_eq!(button.font.families.as_deref().unwrap()[0].as_ref(), "Arial");
        assert_eq!(button.padding_sides, [1.0, 6.0, 1.0, 6.0]);
        assert_eq!((button.border_width, button.border_style), (2.0, BorderStyle::Outset));
        assert_eq!(button.border_color.r, 118);
    }

    #[test]
    fn select_uses_field_surface_and_gray_border() {
        let select = computed(PAGE, DARK_TEXT, "s");
        assert_eq!((select.color.r, select.background.r), (0, 255));
        assert_eq!(select.font_size, 13.333_333);
        assert_eq!(select.padding_sides, [0.0; 4]);
        assert_eq!((select.border_width, select.border_style), (1.0, BorderStyle::Solid));
        assert_eq!(select.border_color.r, 118);
    }

    #[test]
    fn push_button_inputs_match_button() {
        let submit = computed(PAGE, DARK_TEXT, "u");
        assert_eq!((submit.color.r, submit.background.r), (0, 239));
        assert_eq!((submit.padding_sides, submit.border_style, submit.border_width),
            ([1.0, 6.0, 1.0, 6.0], BorderStyle::Outset, 2.0));
    }

    #[test]
    fn field_borders_use_the_ua_gray() {
        assert_eq!(computed(PAGE, DARK_TEXT, "t").border_color.r, 118);
        assert_eq!(computed(PAGE, DARK_TEXT, "a").border_color.r, 118);
    }

    #[test]
    fn author_border_keeps_ua_foreground_surface_and_font() {
        let button = computed(PAGE, "body{color:#fff}button{border:1px solid red}", "b");
        assert_eq!((button.color.r, button.background.r), (0, 239));
        assert_eq!((button.border_width, button.border_style, button.border_color.r), (1.0, BorderStyle::Solid, 255));
        assert_eq!(button.font_size, 13.333_333);
    }

    #[test]
    fn author_color_and_background_override_the_ua_sheet() {
        let button = computed(PAGE, "button{color:red;background:blue}", "b");
        assert_eq!((button.color.r, button.background.b), (255, 255));
    }

    #[test]
    fn base_appearance_opts_out_of_the_native_control_look() {
        let page = "<!doctype html><button id=b class=base>x</button><select id=s class=base></select>";
        let css = "body{color:rgb(0,128,0);font:20px serif}.base{appearance:base}";
        for id in ["b", "s"] {
            let style = computed(page, css, id);
            assert_eq!(style.color.g, 128, "{id}");
            assert_eq!(style.background.a, 0, "{id}");
        }
        assert_eq!(computed(page, css, "b").font_size, 20.0);
    }

    #[test]
    fn color_input_and_buttons_share_button_border_and_face() {
        let page = "<!doctype html><input id=c type=color><button id=b>x</button>";
        let color = computed(page, "", "c");
        let button = computed(page, "", "b");
        assert_eq!((color.border_color, color.background), (button.border_color, button.background));
        assert_eq!(color.border_color.r, 118);
    }
}
