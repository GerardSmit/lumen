//! `image-resolution: [ from-image || <resolution> ] && snap?`. Components keep the
//! order they were written in.
use super::registered_properties::accepts_value;
use super::*;
use alloc::string::String;
use alloc::vec::Vec;

pub(super) fn canonicalize(raw: &str) -> Option<String> {
    let mut from_image = false;
    let mut snap = false;
    let mut resolution = false;
    let mut output: Vec<String> = Vec::new();
    for token in components(raw)? {
        if decoded_css_keyword(token, "from-image") {
            if core::mem::replace(&mut from_image, true) {
                return None;
            }
            output.push(String::from("from-image"));
        } else if decoded_css_keyword(token, "snap") {
            if core::mem::replace(&mut snap, true) {
                return None;
            }
            output.push(String::from("snap"));
        } else {
            if resolution || !accepts_value("<resolution>", token) {
                return None;
            }
            resolution = true;
            output.push(typed_numeric::parse_numeric_expression(token)?.serialize_specified()?);
        }
    }
    (from_image || resolution).then(|| output.join(" "))
}
