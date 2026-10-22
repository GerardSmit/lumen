//! CSS Counter Styles Level 3: predefined styles and the formatting algorithm.
use super::*;
use alloc::borrow::Cow;

const MAX_FALLBACK_DEPTH: usize = 16;
const MAX_REPRESENTATION_CHARS: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OmitOne {
    Never,
    BeforeTen,
    Always,
}

/// The "longhand East Asian" counter styles spell the digits of a value of
/// at most four digits with unit markers (CSS Counter Styles 3 §6.3).
#[derive(Debug, Eq, PartialEq)]
pub struct Cjk {
    digits: [&'static str; 10],
    markers: [&'static str; 3],
    omit_one: OmitOne,
    zero_fill: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum System {
    Cyclic,
    Fixed(i64),
    Symbolic,
    Alphabetic,
    Numeric,
    Additive,
    Cjk(&'static Cjk),
    Ethiopic,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Definition {
    pub system: System,
    pub symbols: Vec<Cow<'static, str>>,
    pub additive: Vec<(i64, Cow<'static, str>)>,
    pub negative: (Cow<'static, str>, Cow<'static, str>),
    pub prefix: Cow<'static, str>,
    pub suffix: Cow<'static, str>,
    pub range: Option<Vec<(i64, i64)>>,
    pub pad: (usize, Cow<'static, str>),
    pub fallback: Cow<'static, str>,
}

impl Definition {
    pub fn new(system: System) -> Self {
        Self {
            system,
            symbols: Vec::new(),
            additive: Vec::new(),
            negative: (Cow::Borrowed("-"), Cow::Borrowed("")),
            prefix: Cow::Borrowed(""),
            suffix: Cow::Borrowed(". "),
            range: None,
            pad: (0, Cow::Borrowed("")),
            fallback: Cow::Borrowed("decimal"),
        }
    }

    fn symbols(system: System, symbols: &'static [&'static str]) -> Self {
        let mut definition = Self::new(system);
        definition.symbols = symbols.iter().map(|symbol| Cow::Borrowed(*symbol)).collect();
        definition
    }

    fn chars(system: System, symbols: &'static str) -> Self {
        let mut definition = Self::new(system);
        definition.symbols = symbols.chars().map(|symbol| Cow::Owned(String::from(symbol))).collect();
        definition
    }

    fn with_suffix(mut self, suffix: &'static str) -> Self {
        self.suffix = Cow::Borrowed(suffix);
        self
    }

    fn with_fallback(mut self, fallback: &'static str) -> Self {
        self.fallback = Cow::Borrowed(fallback);
        self
    }

    fn with_range(mut self, low: i64, high: i64) -> Self {
        self.range = Some(alloc::vec![(low, high)]);
        self
    }

    fn additive(table: &'static [(i64, &'static str)], low: i64, high: i64) -> Self {
        let mut definition = Self::new(System::Additive).with_range(low, high);
        definition.additive = table.iter().map(|(weight, symbol)| (*weight, Cow::Borrowed(*symbol))).collect();
        definition
    }

    fn accepts(&self, value: i64) -> bool {
        match &self.range {
            Some(ranges) => ranges.iter().any(|(low, high)| (*low..=*high).contains(&value)),
            None => match self.system {
                System::Symbolic | System::Alphabetic | System::Ethiopic => value >= 1,
                System::Additive => value >= 0,
                _ => true,
            },
        }
    }

    fn uses_negative_sign(&self) -> bool {
        !matches!(self.system, System::Cyclic | System::Fixed(_))
    }
}

const ROMAN_UPPER: &[(i64, &str)] = &[
    (1000, "M"), (900, "CM"), (500, "D"), (400, "CD"), (100, "C"), (90, "XC"),
    (50, "L"), (40, "XL"), (10, "X"), (9, "IX"), (5, "V"), (4, "IV"), (1, "I"),
];
const ROMAN_LOWER: &[(i64, &str)] = &[
    (1000, "m"), (900, "cm"), (500, "d"), (400, "cd"), (100, "c"), (90, "xc"),
    (50, "l"), (40, "xl"), (10, "x"), (9, "ix"), (5, "v"), (4, "iv"), (1, "i"),
];
const ARMENIAN_UPPER: &[(i64, &str)] = &[
    (9000, "Ք"), (8000, "Փ"), (7000, "Ւ"), (6000, "Ց"), (5000, "Ր"), (4000, "Տ"), (3000, "Վ"),
    (2000, "Ս"), (1000, "Ռ"), (900, "Ջ"), (800, "Պ"), (700, "Չ"), (600, "Ո"), (500, "Շ"),
    (400, "Ն"), (300, "Յ"), (200, "Մ"), (100, "Ճ"), (90, "Ղ"), (80, "Ձ"), (70, "Հ"), (60, "Կ"),
    (50, "Ծ"), (40, "Խ"), (30, "Լ"), (20, "Ի"), (10, "Ժ"), (9, "Թ"), (8, "Ը"), (7, "Է"),
    (6, "Զ"), (5, "Ե"), (4, "Դ"), (3, "Գ"), (2, "Բ"), (1, "Ա"),
];
const ARMENIAN_LOWER: &[(i64, &str)] = &[
    (9000, "ք"), (8000, "փ"), (7000, "ւ"), (6000, "ց"), (5000, "ր"), (4000, "տ"), (3000, "վ"),
    (2000, "ս"), (1000, "ռ"), (900, "ջ"), (800, "պ"), (700, "չ"), (600, "ո"), (500, "շ"),
    (400, "ն"), (300, "յ"), (200, "մ"), (100, "ճ"), (90, "ղ"), (80, "ձ"), (70, "հ"), (60, "կ"),
    (50, "ծ"), (40, "խ"), (30, "լ"), (20, "ի"), (10, "ժ"), (9, "թ"), (8, "ը"), (7, "է"),
    (6, "զ"), (5, "ե"), (4, "դ"), (3, "գ"), (2, "բ"), (1, "ա"),
];
const GEORGIAN: &[(i64, &str)] = &[
    (10000, "ჵ"), (9000, "ჰ"), (8000, "ჯ"), (7000, "ჴ"), (6000, "ხ"), (5000, "ჭ"), (4000, "წ"),
    (3000, "ძ"), (2000, "ც"), (1000, "ჩ"), (900, "შ"), (800, "ყ"), (700, "ღ"), (600, "ქ"),
    (500, "ფ"), (400, "ჳ"), (300, "ტ"), (200, "ს"), (100, "რ"), (90, "ჟ"), (80, "პ"), (70, "ო"),
    (60, "ჲ"), (50, "ნ"), (40, "მ"), (30, "ლ"), (20, "კ"), (10, "ი"), (9, "თ"), (8, "ჱ"),
    (7, "ზ"), (6, "ვ"), (5, "ე"), (4, "დ"), (3, "გ"), (2, "ბ"), (1, "ა"),
];
const HEBREW: &[(i64, &str)] = &[
    (10000, "י׳"), (9000, "ט׳"), (8000, "ח׳"), (7000, "ז׳"), (6000, "ו׳"), (5000, "ה׳"),
    (4000, "ד׳"), (3000, "ג׳"), (2000, "ב׳"), (1000, "א׳"), (400, "ת"), (300, "ש"), (200, "ר"),
    (100, "ק"), (90, "צ"), (80, "פ"), (70, "ע"), (60, "ס"), (50, "נ"), (40, "מ"), (30, "ל"),
    (20, "כ"), (19, "יט"), (18, "יח"), (17, "יז"), (16, "טז"), (15, "טו"), (10, "י"), (9, "ט"),
    (8, "ח"), (7, "ז"), (6, "ו"), (5, "ה"), (4, "ד"), (3, "ג"), (2, "ב"), (1, "א"),
];

static JAPANESE_INFORMAL: Cjk = Cjk {
    digits: ["〇", "一", "二", "三", "四", "五", "六", "七", "八", "九"],
    markers: ["十", "百", "千"],
    omit_one: OmitOne::Always,
    zero_fill: false,
};
static JAPANESE_FORMAL: Cjk = Cjk {
    digits: ["零", "壱", "弐", "参", "四", "伍", "六", "七", "八", "九"],
    markers: ["拾", "百", "阡"],
    omit_one: OmitOne::Never,
    zero_fill: false,
};
static CHINESE_INFORMAL: Cjk = Cjk {
    digits: ["零", "一", "二", "三", "四", "五", "六", "七", "八", "九"],
    markers: ["十", "百", "千"],
    omit_one: OmitOne::BeforeTen,
    zero_fill: true,
};
static TRAD_CHINESE_FORMAL: Cjk = Cjk {
    digits: ["零", "壹", "貳", "參", "肆", "伍", "陸", "柒", "捌", "玖"],
    markers: ["拾", "佰", "仟"],
    omit_one: OmitOne::Never,
    zero_fill: true,
};
static SIMP_CHINESE_FORMAL: Cjk = Cjk {
    digits: ["零", "壹", "贰", "叁", "肆", "伍", "陆", "柒", "捌", "玖"],
    markers: ["拾", "佰", "仟"],
    omit_one: OmitOne::Never,
    zero_fill: true,
};
static KOREAN_HANGUL_FORMAL: Cjk = Cjk {
    digits: ["영", "일", "이", "삼", "사", "오", "육", "칠", "팔", "구"],
    markers: ["십", "백", "천"],
    omit_one: OmitOne::Never,
    zero_fill: false,
};
static KOREAN_HANJA_FORMAL: Cjk = Cjk {
    digits: ["零", "壹", "貳", "參", "四", "五", "六", "七", "八", "九"],
    markers: ["拾", "百", "仟"],
    omit_one: OmitOne::Never,
    zero_fill: false,
};
static KOREAN_HANJA_INFORMAL: Cjk = Cjk {
    digits: ["零", "一", "二", "三", "四", "五", "六", "七", "八", "九"],
    markers: ["十", "百", "千"],
    omit_one: OmitOne::Always,
    zero_fill: false,
};

fn numeric(zero: char) -> Definition {
    let mut definition = Definition::new(System::Numeric);
    definition.symbols = (0..10u32)
        .filter_map(|offset| char::from_u32(zero as u32 + offset))
        .map(|digit| Cow::Owned(String::from(digit)))
        .collect();
    definition
}

fn cjk(spec: &'static Cjk, negative: &'static str, suffix: &'static str, fallback: &'static str) -> Definition {
    let mut definition = Definition::new(System::Cjk(spec)).with_range(-9999, 9999);
    definition.negative = (Cow::Borrowed(negative), Cow::Borrowed(""));
    definition.suffix = Cow::Borrowed(suffix);
    definition.fallback = Cow::Borrowed(fallback);
    definition
}

/// The predefined counter style with this (ASCII case-insensitive) name.
pub fn predefined(name: &str) -> Option<Definition> {
    let lower = name.to_ascii_lowercase();
    Some(match lower.as_str() {
        "decimal" => numeric('0'),
        "decimal-leading-zero" => {
            let mut definition = numeric('0');
            definition.pad = (2, Cow::Borrowed("0"));
            definition
        }
        "arabic-indic" => numeric('\u{660}'),
        "persian" => numeric('\u{6f0}'),
        "bengali" => numeric('\u{9e6}'),
        "devanagari" => numeric('\u{966}'),
        "gujarati" => numeric('\u{ae6}'),
        "gurmukhi" => numeric('\u{a66}'),
        "kannada" => numeric('\u{ce6}'),
        "khmer" | "cambodian" => numeric('\u{17e0}'),
        "lao" => numeric('\u{ed0}'),
        "malayalam" => numeric('\u{d66}'),
        "mongolian" => numeric('\u{1810}'),
        "myanmar" => numeric('\u{1040}'),
        "oriya" => numeric('\u{b66}'),
        "tamil" => numeric('\u{be6}'),
        "telugu" => numeric('\u{c66}'),
        "thai" => numeric('\u{e50}'),
        "tibetan" => numeric('\u{f20}'),
        "cjk-decimal" => Definition::chars(System::Numeric, "〇一二三四五六七八九").with_suffix("、"),
        "disc" => Definition::symbols(System::Cyclic, &["\u{2022}"]).with_suffix(" "),
        "circle" => Definition::symbols(System::Cyclic, &["\u{25e6}"]).with_suffix(" "),
        "square" => Definition::symbols(System::Cyclic, &["\u{25aa}"]).with_suffix(" "),
        "disclosure-open" => Definition::symbols(System::Cyclic, &["\u{25be}"]).with_suffix(" "),
        "disclosure-closed" => Definition::symbols(System::Cyclic, &["\u{25b8}"]).with_suffix(" "),
        "lower-alpha" | "lower-latin" => Definition::chars(System::Alphabetic, "abcdefghijklmnopqrstuvwxyz"),
        "upper-alpha" | "upper-latin" => Definition::chars(System::Alphabetic, "ABCDEFGHIJKLMNOPQRSTUVWXYZ"),
        "lower-greek" => Definition::chars(System::Alphabetic, "αβγδεζηθικλμνξοπρστυφχψω"),
        "hiragana" => Definition::chars(System::Alphabetic, "あいうえおかきくけこさしすせそたちつてとなにぬねのはひふへほまみむめもやゆよらりるれろわゐゑをん").with_suffix("、"),
        "hiragana-iroha" => Definition::chars(System::Alphabetic, "いろはにほへとちりぬるをわかよたれそつねならむうゐのおくやまけふこえてあさきゆめみしゑひもせす").with_suffix("、"),
        "katakana" => Definition::chars(System::Alphabetic, "アイウエオカキクケコサシスセソタチツテトナニヌネノハヒフヘホマミムメモヤユヨラリルレロワヰヱヲン").with_suffix("、"),
        "katakana-iroha" => Definition::chars(System::Alphabetic, "イロハニホヘトチリヌルヲワカヨタレソツネナラムウヰノオクヤマケフコエテアサキユメミシヱヒモセス").with_suffix("、"),
        "cjk-earthly-branch" => Definition::chars(System::Fixed(1), "子丑寅卯辰巳午未申酉戌亥").with_suffix("、").with_fallback("cjk-decimal"),
        "cjk-heavenly-stem" => Definition::chars(System::Fixed(1), "甲乙丙丁戊己庚辛壬癸").with_suffix("、").with_fallback("cjk-decimal"),
        "upper-roman" => Definition::additive(ROMAN_UPPER, 1, 3999),
        "lower-roman" => Definition::additive(ROMAN_LOWER, 1, 3999),
        "armenian" | "upper-armenian" => Definition::additive(ARMENIAN_UPPER, 1, 9999),
        "lower-armenian" => Definition::additive(ARMENIAN_LOWER, 1, 9999),
        "georgian" => Definition::additive(GEORGIAN, 1, 19999),
        "hebrew" => Definition::additive(HEBREW, 1, 10999),
        "ethiopic-numeric" => {
            let mut definition = Definition::new(System::Ethiopic);
            definition.suffix = Cow::Borrowed("/ ");
            definition
        }
        "japanese-informal" => cjk(&JAPANESE_INFORMAL, "マイナス", "、", "cjk-decimal"),
        "japanese-formal" => cjk(&JAPANESE_FORMAL, "マイナス", "、", "cjk-decimal"),
        "trad-chinese-informal" => cjk(&CHINESE_INFORMAL, "負", "、", "cjk-decimal"),
        "trad-chinese-formal" => cjk(&TRAD_CHINESE_FORMAL, "負", "、", "cjk-decimal"),
        "simp-chinese-informal" => cjk(&CHINESE_INFORMAL, "负", "、", "cjk-decimal"),
        "simp-chinese-formal" => cjk(&SIMP_CHINESE_FORMAL, "负", "、", "cjk-decimal"),
        "korean-hangul-formal" => cjk(&KOREAN_HANGUL_FORMAL, "마이너스 ", ", ", "decimal"),
        "korean-hanja-formal" => cjk(&KOREAN_HANJA_FORMAL, "마이너스 ", ", ", "decimal"),
        "korean-hanja-informal" => cjk(&KOREAN_HANJA_INFORMAL, "마이너스 ", ", ", "decimal"),
        _ => return None,
    })
}

/// Parses the arguments of `symbols()` into an anonymous counter style.
pub fn parse_symbols(arguments: &str) -> Option<Definition> {
    let parts = components_bounded(arguments, MAX_GENERATED_CONTENT_BYTES)?;
    let (system, rest) = match parts.first() {
        Some(first) if first.eq_ignore_ascii_case("cyclic") => (System::Cyclic, &parts[1..]),
        Some(first) if first.eq_ignore_ascii_case("numeric") => (System::Numeric, &parts[1..]),
        Some(first) if first.eq_ignore_ascii_case("alphabetic") => (System::Alphabetic, &parts[1..]),
        Some(first) if first.eq_ignore_ascii_case("symbolic") => (System::Symbolic, &parts[1..]),
        Some(first) if first.eq_ignore_ascii_case("fixed") => (System::Fixed(1), &parts[1..]),
        _ => (System::Symbolic, &parts[..]),
    };
    let mut definition = Definition::new(system);
    for part in rest {
        let symbol = match css_string(part) {
            Some(value) => value,
            None => String::from(&*parse_counter_ident(part)?),
        };
        definition.symbols.push(Cow::Owned(symbol));
    }
    let minimum = match system {
        System::Numeric | System::Alphabetic => 2,
        _ => 1,
    };
    (definition.symbols.len() >= minimum).then_some(definition)
}

fn lookup(name: &str, resolve: &dyn Fn(&str) -> Option<Definition>) -> Option<Definition> {
    let name = name.trim();
    if name.len() > 8 && name.get(..8).is_some_and(|head| head.eq_ignore_ascii_case("symbols(")) {
        return parse_symbols(name.get(8..name.len() - 1)?);
    }
    resolve(name).or_else(|| predefined(name))
}

/// The prefix and suffix that surround a marker of this counter style.
pub fn affixes(style: &str, resolve: &dyn Fn(&str) -> Option<Definition>) -> (String, String) {
    match lookup(style, resolve) {
        Some(definition) => (definition.prefix.into_owned(), definition.suffix.into_owned()),
        None => (String::new(), String::from(". ")),
    }
}

/// Whether the name denotes a counter style that exists.
pub fn exists(style: &str, resolve: &dyn Fn(&str) -> Option<Definition>) -> bool {
    lookup(style, resolve).is_some()
}

/// The text of `value` in the named counter style, following fallbacks.
pub fn format(value: i64, style: &str, resolve: &dyn Fn(&str) -> Option<Definition>) -> String {
    let mut current = lookup(style, resolve);
    for _ in 0..MAX_FALLBACK_DEPTH {
        let Some(definition) = current else { break };
        if let Some(text) = represent(value, &definition) {
            return text;
        }
        current = lookup(&definition.fallback, resolve);
    }
    alloc::format!("{value}")
}

fn represent(value: i64, definition: &Definition) -> Option<String> {
    if !definition.accepts(value) {
        return None;
    }
    let negative = value < 0 && definition.uses_negative_sign();
    let magnitude = value.unsigned_abs();
    let mut text = match definition.system {
        System::Cyclic => {
            let count = definition.symbols.len() as i64;
            if count == 0 {
                return None;
            }
            definition.symbols[(value - 1).rem_euclid(count) as usize].clone().into_owned()
        }
        System::Fixed(first) => {
            let offset = value.checked_sub(first)?;
            usize::try_from(offset).ok().and_then(|index| definition.symbols.get(index))?.clone().into_owned()
        }
        System::Symbolic => {
            let count = definition.symbols.len() as u64;
            if count == 0 || magnitude == 0 {
                return None;
            }
            let repetitions = (magnitude - 1) / count + 1;
            let symbol = &definition.symbols[((magnitude - 1) % count) as usize];
            if repetitions as usize * symbol.chars().count().max(1) > MAX_REPRESENTATION_CHARS {
                return None;
            }
            symbol.repeat(repetitions as usize)
        }
        System::Alphabetic => alphabetic(magnitude, &definition.symbols)?,
        System::Numeric => numeric_digits(magnitude, &definition.symbols)?,
        System::Additive => additive(magnitude, &definition.additive)?,
        System::Cjk(spec) => cjk_text(magnitude, spec)?,
        System::Ethiopic => ethiopic(magnitude)?,
    };
    let sign = if negative { (definition.negative.0.as_ref(), definition.negative.1.as_ref()) } else { ("", "") };
    let length = lumen_common::ucd::graphemes(sign.0).count()
        + lumen_common::ucd::graphemes(&text).count()
        + lumen_common::ucd::graphemes(sign.1).count();
    if definition.pad.0 > length && !definition.pad.1.is_empty() {
        let missing = definition.pad.0 - length;
        if missing > MAX_REPRESENTATION_CHARS {
            return None;
        }
        text.insert_str(0, &definition.pad.1.repeat(missing));
    }
    if negative {
        text = alloc::format!("{}{}{}", sign.0, text, sign.1);
    }
    Some(text)
}

fn alphabetic(mut value: u64, symbols: &[Cow<'static, str>]) -> Option<String> {
    let count = symbols.len() as u64;
    if count < 2 || value == 0 {
        return None;
    }
    let mut reversed: Vec<&str> = Vec::new();
    while value != 0 {
        value -= 1;
        reversed.push(&symbols[(value % count) as usize]);
        value /= count;
    }
    Some(reversed.iter().rev().copied().collect())
}

fn numeric_digits(mut value: u64, symbols: &[Cow<'static, str>]) -> Option<String> {
    let count = symbols.len() as u64;
    if count < 2 {
        return None;
    }
    if value == 0 {
        return Some(symbols[0].clone().into_owned());
    }
    let mut reversed: Vec<&str> = Vec::new();
    while value != 0 {
        reversed.push(&symbols[(value % count) as usize]);
        value /= count;
    }
    Some(reversed.iter().rev().copied().collect())
}

fn additive(mut value: u64, table: &[(i64, Cow<'static, str>)]) -> Option<String> {
    if value == 0 {
        return table.iter().find(|(weight, _)| *weight == 0).map(|(_, symbol)| symbol.clone().into_owned());
    }
    let mut text = String::new();
    for (weight, symbol) in table {
        let weight = u64::try_from(*weight).ok().filter(|weight| *weight != 0)?;
        if value >= weight {
            let repetitions = value / weight;
            if repetitions as usize * symbol.chars().count().max(1) + text.chars().count() > MAX_REPRESENTATION_CHARS {
                return None;
            }
            for _ in 0..repetitions {
                text.push_str(symbol);
            }
            value %= weight;
        }
        if value == 0 {
            break;
        }
    }
    (value == 0).then_some(text)
}

fn cjk_text(value: u64, spec: &Cjk) -> Option<String> {
    if value == 0 {
        return Some(String::from(spec.digits[0]));
    }
    if value > 9999 {
        return None;
    }
    let places = [(value / 1000, 3usize), (value / 100 % 10, 2), (value / 10 % 10, 1), (value % 10, 0)];
    let mut text = String::new();
    let mut pending_zero = false;
    for (digit, place) in places {
        if digit == 0 {
            pending_zero = !text.is_empty();
            continue;
        }
        if pending_zero && spec.zero_fill {
            text.push_str(spec.digits[0]);
        }
        pending_zero = false;
        let omit = digit == 1
            && place > 0
            && match spec.omit_one {
                OmitOne::Never => false,
                OmitOne::Always => true,
                OmitOne::BeforeTen => place == 1 && (10..=19).contains(&value),
            };
        if !omit {
            text.push_str(spec.digits[digit as usize]);
        }
        if place > 0 {
            text.push_str(spec.markers[place - 1]);
        }
    }
    Some(text)
}

fn ethiopic(value: u64) -> Option<String> {
    let ones = |digit: u64| char::from_u32(0x1368 + digit as u32);
    let tens = |digit: u64| char::from_u32(0x1371 + digit as u32);
    let mut pairs: Vec<u64> = Vec::new();
    let mut rest = value;
    while rest != 0 {
        pairs.push(rest % 100);
        rest /= 100;
    }
    let top = pairs.len().checked_sub(1)?;
    let mut text = String::new();
    for position in (0..=top).rev() {
        let pair = pairs[position];
        let marker = if position == 0 {
            None
        } else if position % 2 == 1 {
            (pair != 0).then_some('\u{137b}')
        } else {
            (pair != 0 || pairs.get(position + 1).is_some_and(|higher| *higher != 0)).then_some('\u{137c}')
        };
        let leading_one = pair == 1 && position == top && position > 0;
        if pair != 0 && !leading_one {
            if pair / 10 != 0 {
                text.push(tens(pair / 10)?);
            }
            if pair % 10 != 0 {
                text.push(ones(pair % 10)?);
            }
        }
        if let Some(marker) = marker {
            text.push(marker);
        }
    }
    Some(text)
}

/// One `@counter-style` rule: its case-sensitive name and resolved definition.
#[derive(Clone, Debug)]
pub struct CounterStyleRule {
    pub name: Arc<str>,
    pub definition: Definition,
}

const RESERVED_NAMES: [&str; 6] = ["decimal", "disc", "square", "circle", "disclosure-open", "disclosure-closed"];

pub(super) fn rule_name(prelude: &str) -> Option<Arc<str>> {
    let tail = nesting::at_rule_tail(prelude.trim(), "@counter-style")?;
    let name = parse_counter_ident(tail.trim())?;
    (!RESERVED_NAMES.iter().any(|reserved| name.eq_ignore_ascii_case(reserved))).then_some(name)
}

fn symbol(part: &str) -> Option<String> {
    match css_string(part) {
        Some(value) => Some(value),
        None => parse_counter_ident(part).map(|name| String::from(&*name)),
    }
}

enum SystemSpec {
    Plain(System),
    Extends(Arc<str>),
}

fn parse_system(value: &str) -> Option<SystemSpec> {
    let parts = components_bounded(value, MAX_GENERATED_CONTENT_BYTES)?;
    let keyword = parts.first()?.to_ascii_lowercase();
    let plain = |system: System| (parts.len() == 1).then_some(SystemSpec::Plain(system));
    match keyword.as_str() {
        "cyclic" => plain(System::Cyclic),
        "numeric" => plain(System::Numeric),
        "alphabetic" => plain(System::Alphabetic),
        "symbolic" => plain(System::Symbolic),
        "additive" => plain(System::Additive),
        "fixed" => match parts.len() {
            1 => Some(SystemSpec::Plain(System::Fixed(1))),
            2 => Some(SystemSpec::Plain(System::Fixed(parts[1].parse().ok()?))),
            _ => None,
        },
        "extends" if parts.len() == 2 => parse_counter_ident(parts[1]).map(SystemSpec::Extends),
        _ => None,
    }
}

fn parse_symbol_list(value: &str) -> Option<Vec<Cow<'static, str>>> {
    let parts = components_bounded(value, MAX_GENERATED_CONTENT_BYTES)?;
    parts.into_iter().map(|part| symbol(part).map(Cow::Owned)).collect()
}

fn parse_additive(value: &str) -> Option<Vec<(i64, Cow<'static, str>)>> {
    let entries = top_level_split(value, b',', 256)?;
    let mut table: Vec<(i64, Cow<'static, str>)> = Vec::new();
    for entry in entries {
        let parts = components_bounded(entry, MAX_GENERATED_CONTENT_BYTES)?;
        if parts.len() != 2 {
            return None;
        }
        let (weight, text) = match parts[0].parse::<i64>() {
            Ok(weight) => (weight, parts[1]),
            Err(_) => (parts[1].parse::<i64>().ok()?, parts[0]),
        };
        if weight < 0 || table.last().is_some_and(|(previous, _)| *previous <= weight) {
            return None;
        }
        table.push((weight, Cow::Owned(symbol(text)?)));
    }
    (!table.is_empty()).then_some(table)
}

fn parse_range(value: &str) -> Option<Option<Vec<(i64, i64)>>> {
    if value.eq_ignore_ascii_case("auto") {
        return Some(None);
    }
    let bound = |part: &str, infinity: i64| {
        if part.eq_ignore_ascii_case("infinite") {
            Some(infinity)
        } else {
            part.parse::<i64>().ok()
        }
    };
    let mut ranges = Vec::new();
    for entry in top_level_split(value, b',', 64)? {
        let parts = components_bounded(entry, MAX_GENERATED_CONTENT_BYTES)?;
        if parts.len() != 2 {
            return None;
        }
        let (low, high) = (bound(parts[0], i64::MIN)?, bound(parts[1], i64::MAX)?);
        if low > high {
            return None;
        }
        ranges.push((low, high));
    }
    Some(Some(ranges))
}

fn parse_pad(value: &str) -> Option<(usize, Cow<'static, str>)> {
    let parts = components_bounded(value, MAX_GENERATED_CONTENT_BYTES)?;
    if parts.len() != 2 {
        return None;
    }
    let (count, text) = match parts[0].parse::<i64>() {
        Ok(count) => (count, parts[1]),
        Err(_) => (parts[1].parse::<i64>().ok()?, parts[0]),
    };
    if count < 0 {
        return None;
    }
    Some((count.min(255) as usize, Cow::Owned(symbol(text)?)))
}

fn parse_negative(value: &str) -> Option<(Cow<'static, str>, Cow<'static, str>)> {
    let parts = components_bounded(value, MAX_GENERATED_CONTENT_BYTES)?;
    match parts.as_slice() {
        [before] => Some((Cow::Owned(symbol(before)?), Cow::Borrowed(""))),
        [before, after] => Some((Cow::Owned(symbol(before)?), Cow::Owned(symbol(after)?))),
        _ => None,
    }
}

fn build_rule(declarations: &str, earlier: &[CounterStyleRule]) -> Option<Definition> {
    let mut system = None;
    let mut symbols = None;
    let mut additive = None;
    let mut negative = None;
    let mut prefix = None;
    let mut suffix = None;
    let mut range = None;
    let mut pad = None;
    let mut fallback = None;
    for piece in top_level_split(declarations, b';', 256)? {
        let Some((name, value)) = piece.split_once(':') else { continue };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "system" => system = parse_system(value).or(system),
            "symbols" => symbols = parse_symbol_list(value).or(symbols),
            "additive-symbols" => additive = parse_additive(value).or(additive),
            "negative" => negative = parse_negative(value).or(negative),
            "prefix" => prefix = symbol(value).map(Cow::Owned).or(prefix),
            "suffix" => suffix = symbol(value).map(Cow::Owned).or(suffix),
            "range" => range = parse_range(value).or(range),
            "pad" => pad = parse_pad(value).or(pad),
            "fallback" => fallback = parse_counter_ident(value).map(|name| Cow::Owned(String::from(&*name))).or(fallback),
            _ => {}
        }
    }
    let mut definition = match system {
        Some(SystemSpec::Extends(base)) => earlier
            .iter()
            .rev()
            .find(|rule| rule.name == base)
            .map(|rule| rule.definition.clone())
            .or_else(|| predefined(&base))
            .or_else(|| predefined("decimal"))?,
        other => {
            let system = match other {
                Some(SystemSpec::Plain(system)) => system,
                _ => System::Symbolic,
            };
            let mut definition = Definition::new(system);
            match system {
                System::Additive => definition.additive = additive?,
                System::Numeric | System::Alphabetic => {
                    let symbols = symbols?;
                    if symbols.len() < 2 {
                        return None;
                    }
                    definition.symbols = symbols;
                }
                _ => definition.symbols = symbols.filter(|symbols| !symbols.is_empty())?,
            }
            definition
        }
    };
    if let Some(value) = negative {
        definition.negative = value;
    }
    if let Some(value) = prefix {
        definition.prefix = value;
    }
    if let Some(value) = suffix {
        definition.suffix = value;
    }
    if let Some(value) = range {
        definition.range = value;
    }
    if let Some(value) = pad {
        definition.pad = value;
    }
    if let Some(value) = fallback {
        definition.fallback = value;
    }
    Some(definition)
}

fn collect_nodes(input: &str, nodes: &[nesting::SourceRule], out: &mut Vec<CounterStyleRule>) {
    for node in nodes {
        if node.kind == nesting::SourceRuleKind::CounterStyle && out.len() < 256 {
            if let Some(name) = rule_name(&input[node.prelude.clone()]) {
                if let Some(definition) = build_rule(&node.declaration_source(input), out) {
                    out.push(CounterStyleRule { name, definition });
                }
            }
        }
        collect_nodes(input, &node.children, out);
    }
}

/// The valid `@counter-style` rules of a stylesheet, in source order.
pub fn collect_rules(input: &str) -> Vec<CounterStyleRule> {
    let mut rules = Vec::new();
    if !input.as_bytes().windows(14).any(|window| window.eq_ignore_ascii_case(b"@counter-style")) {
        return rules;
    }
    if let Ok(nodes) = nesting::parse_source_rules(input, &[], false) {
        collect_nodes(input, &nodes, &mut rules);
    }
    rules
}
