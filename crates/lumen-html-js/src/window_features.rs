#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct WindowFeatures {
    pub left: u32,
    pub top: u32,
    pub width: u32,
    pub height: u32,
}

fn is_separator(character: char) -> bool {
    character.is_ascii_whitespace() || character == ',' || character == '='
}

fn parse_feature_integer(value: &str) -> i64 {
    let value = value.trim_start_matches(|c: char| c.is_ascii_whitespace());
    let (negative, digits) = match value.as_bytes().first() {
        Some(b'-') => (true, &value[1..]),
        Some(b'+') => (false, &value[1..]),
        _ => (false, value),
    };
    let end = digits.find(|c: char| !c.is_ascii_digit()).unwrap_or(digits.len());
    if end == 0 {
        return 0;
    }
    let magnitude = digits[..end].parse::<i64>().unwrap_or(i64::from(u32::MAX)).min(i64::from(u32::MAX));
    if negative { -magnitude } else { magnitude }
}

/// A size below the minimum (but not zero, which leaves it unset) becomes the minimum.
fn feature_size(number: i64) -> u32 {
    if number < 0 { 1 } else { number as u32 }
}

impl WindowFeatures {
    /// Tokenizes a `window.open` features string and keeps the position and
    /// size features; zero stands for an unset or invalid value.
    pub(crate) fn parse(features: &str) -> Self {
        let mut result = Self::default();
        let mut rest = features;
        loop {
            rest = rest.trim_start_matches(is_separator);
            if rest.is_empty() {
                break;
            }
            let name_end = rest.find(is_separator).unwrap_or(rest.len());
            let name = rest[..name_end].to_ascii_lowercase();
            rest = &rest[name_end..];
            let mut value = "";
            let after_space = rest.trim_start_matches(|c: char| c.is_ascii_whitespace());
            if let Some(after_equals) = after_space.strip_prefix('=') {
                let value_start = after_equals.trim_start_matches(|c: char| c.is_ascii_whitespace() || c == '=');
                let value_end = value_start.find(is_separator).unwrap_or(value_start.len());
                value = &value_start[..value_end];
                rest = &value_start[value_end..];
            }
            let number = parse_feature_integer(value);
            match name.as_str() {
                "left" | "screenx" => result.left = number.clamp(0, i64::from(i32::MAX)) as u32,
                "top" | "screeny" => result.top = number.clamp(0, i64::from(i32::MAX)) as u32,
                "width" | "innerwidth" => result.width = feature_size(number),
                "height" | "innerheight" => result.height = feature_size(number),
                _ => {}
            }
        }
        result
    }

    pub(crate) fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::WindowFeatures;

    #[test]
    fn tokenizes_separators_and_integers() {
        let features = WindowFeatures::parse("\nHEIGHT= 402,,width=\n401.5, left==7 top=-3");
        assert_eq!((features.width, features.height, features.left, features.top), (401, 402, 7, 0));
        let negative = WindowFeatures::parse("width=-404.5,height=-404e1");
        assert_eq!((negative.width, negative.height), (1, 1));
    }
}
