//! Shared Typed OM numeric primitives and property iteration helpers.
//!
//! Numeric CSS values share one bounded expression parser. Rendering supplies
//! a context resolver; Typed OM keeps the expression tree for reification.

use alloc::{boxed::Box, string::String, vec, vec::Vec};

use super::{UnparsedComponent, css_list_items, parse_unparsed_value, supports_declaration};

/// A CSS unit that can be represented by one `CSSUnitValue`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum NumericUnit {
    Number,
    Percent,
    Cap,
    Em,
    Ex,
    Ch,
    Ic,
    Rem,
    Lh,
    Rlh,
    Rcap,
    Rch,
    Rex,
    Ric,
    Vw,
    Vh,
    Vi,
    Vb,
    Vmin,
    Vmax,
    Svw,
    Svh,
    Svi,
    Svb,
    Svmin,
    Svmax,
    Lvw,
    Lvh,
    Lvi,
    Lvb,
    Lvmin,
    Lvmax,
    Dvw,
    Dvh,
    Dvi,
    Dvb,
    Dvmin,
    Dvmax,
    Cqw,
    Cqh,
    Cqi,
    Cqb,
    Cqmin,
    Cqmax,
    Cm,
    Mm,
    Q,
    In,
    Pt,
    Pc,
    Px,
    Deg,
    Grad,
    Rad,
    Turn,
    S,
    Ms,
    Hz,
    KHz,
    Dpi,
    Dpcm,
    Dppx,
    Fr,
}

/// The single numeric payload parsed from a CSS primitive.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NumericValue {
    pub value: f64,
    pub unit: NumericUnit,
}

/// A bounded, language-neutral CSS numeric expression.
///
/// `Calc` preserves an explicit `calc()` boundary because Typed OM reification
/// turns it into a `CSSMathSum`, even when its operand is a single value.
#[derive(Clone, Debug, PartialEq)]
pub enum NumericExpression {
    Value(NumericValue),
    Calc(Box<Self>),
    Sum(Vec<Self>),
    Product(Vec<Self>),
    Min(Vec<Self>),
    Max(Vec<Self>),
    Clamp(Box<Self>, Box<Self>, Box<Self>),
    Negate(Box<Self>),
    Invert(Box<Self>),
    Sign(Box<Self>),
}

/// Limits applied before an expression is returned to CSS or Typed OM.
pub const MAX_NUMERIC_EXPRESSION_BYTES: usize = 1024;
pub const MAX_NUMERIC_EXPRESSION_DEPTH: usize = 16;
pub const MAX_NUMERIC_EXPRESSION_NODES: usize = 256;
pub const MAX_NUMERIC_EXPRESSION_ARGS: usize = 32;

/// Context operations used to evaluate the shared expression tree.
///
/// The parser owns syntax and operator order; a caller owns unit resolution
/// and dimensional policy.
pub trait NumericExpressionContext {
    type Value;

    fn unit(&mut self, value: NumericValue) -> Option<Self::Value>;
    fn add(&mut self, left: Self::Value, right: Self::Value) -> Option<Self::Value>;
    fn multiply(&mut self, left: Self::Value, right: Self::Value) -> Option<Self::Value>;
    fn minimum(&mut self, left: Self::Value, right: Self::Value) -> Option<Self::Value>;
    fn maximum(&mut self, left: Self::Value, right: Self::Value) -> Option<Self::Value>;
    fn clamp(
        &mut self,
        lower: Self::Value,
        value: Self::Value,
        upper: Self::Value,
    ) -> Option<Self::Value>;
    fn negate(&mut self, value: Self::Value) -> Option<Self::Value>;
    fn invert(&mut self, value: Self::Value) -> Option<Self::Value>;
    fn sign(&mut self, value: Self::Value) -> Option<Self::Value>;
}

impl NumericExpression {
    /// Evaluate with caller-supplied unit conversion and type rules.
    pub fn evaluate<C: NumericExpressionContext>(&self, context: &mut C) -> Option<C::Value> {
        match self {
            Self::Value(value) => context.unit(*value),
            Self::Calc(value) => value.evaluate(context),
            Self::Sum(values) => {
                let mut values = values.iter();
                let mut result = values.next()?.evaluate(context)?;
                for value in values {
                    let next = value.evaluate(context)?;
                    result = context.add(result, next)?;
                }
                Some(result)
            }
            Self::Product(values) => {
                let mut values = values.iter();
                let mut result = values.next()?.evaluate(context)?;
                for value in values {
                    let next = value.evaluate(context)?;
                    result = context.multiply(result, next)?;
                }
                Some(result)
            }
            Self::Min(values) => {
                let mut values = values.iter();
                let mut result = values.next()?.evaluate(context)?;
                for value in values {
                    let next = value.evaluate(context)?;
                    result = context.minimum(result, next)?;
                }
                Some(result)
            }
            Self::Max(values) => {
                let mut values = values.iter();
                let mut result = values.next()?.evaluate(context)?;
                for value in values {
                    let next = value.evaluate(context)?;
                    result = context.maximum(result, next)?;
                }
                Some(result)
            }
            Self::Clamp(lower, value, upper) => {
                let lower = lower.evaluate(context)?;
                let value = value.evaluate(context)?;
                let upper = upper.evaluate(context)?;
                context.clamp(lower, value, upper)
            }
            Self::Negate(value) => {
                let value = value.evaluate(context)?;
                context.negate(value)
            }
            Self::Invert(value) => {
                let value = value.evaluate(context)?;
                context.invert(value)
            }
            Self::Sign(value) => {
                let value = value.evaluate(context)?;
                context.sign(value)
            }
        }
    }

    pub fn contains_sign(&self) -> bool {
        match self {
            Self::Sign(_) => true,
            Self::Calc(value) | Self::Negate(value) | Self::Invert(value) => value.contains_sign(),
            Self::Sum(values) | Self::Product(values) | Self::Min(values) | Self::Max(values) => {
                values.iter().any(Self::contains_sign)
            }
            Self::Clamp(lower, value, upper) => {
                lower.contains_sign() || value.contains_sign() || upper.contains_sign()
            }
            Self::Value(_) => false,
        }
    }

    /// Count nodes and depth without allocating. This is also used for values
    /// assembled through Typed OM constructors, which do not pass through the
    /// expression parser's limits.
    pub fn within_limits(&self) -> bool {
        fn measure(value: &NumericExpression, depth: usize, count: &mut usize) -> bool {
            if depth > MAX_NUMERIC_EXPRESSION_DEPTH {
                return false;
            }
            *count += 1;
            if *count > MAX_NUMERIC_EXPRESSION_NODES {
                return false;
            }
            let children: &[NumericExpression] = match value {
                NumericExpression::Value(_) => return true,
                NumericExpression::Calc(value)
                | NumericExpression::Negate(value)
                | NumericExpression::Invert(value)
                | NumericExpression::Sign(value) => {
                    return measure(value, depth + 1, count);
                }
                NumericExpression::Sum(values)
                | NumericExpression::Product(values)
                | NumericExpression::Min(values)
                | NumericExpression::Max(values) => values,
                NumericExpression::Clamp(lower, value, upper) => {
                    return measure(lower, depth + 1, count)
                        && measure(value, depth + 1, count)
                        && measure(upper, depth + 1, count);
                }
            };
            children.len() <= MAX_NUMERIC_EXPRESSION_ARGS
                && children
                    .iter()
                    .all(|child| measure(child, depth + 1, count))
        }

        let mut count = 0;
        measure(self, 1, &mut count)
    }

    /// Serialize a Typed OM math tree using CSS Values syntax. Growth is
    /// checked at each append so deeply nested author-created values remain
    /// bounded by the same limit as parsed values.
    pub fn serialize(&self) -> Option<String> {
        use lumen_common::limits::size::{append_string, string_with_capacity};

        fn append(
            out: &mut String,
            expression: &NumericExpression,
            nested: bool,
            paren_less: bool,
        ) -> Option<()> {
            let append_text = |out: &mut String, text: &str| {
                append_string(out, text, MAX_NUMERIC_EXPRESSION_BYTES).ok()
            };
            match expression {
                NumericExpression::Value(value) => {
                    let text = serialize_numeric_value(value.value, value.unit);
                    append_text(out, &text)
                }
                NumericExpression::Calc(value) => {
                    if paren_less {
                        append(out, value, nested, true)
                    } else {
                        append_text(out, if nested { "(" } else { "calc(" })?;
                        append(out, value, false, true)?;
                        append_text(out, ")")
                    }
                }
                NumericExpression::Min(values) | NumericExpression::Max(values) => {
                    append_text(
                        out,
                        if matches!(expression, NumericExpression::Min(_)) {
                            "min("
                        } else {
                            "max("
                        },
                    )?;
                    for (index, value) in values.iter().enumerate() {
                        if index != 0 {
                            append_text(out, ", ")?;
                        }
                        append(out, value, true, true)?;
                    }
                    append_text(out, ")")
                }
                NumericExpression::Clamp(lower, value, upper) => {
                    append_text(out, "clamp(")?;
                    append(out, lower, true, true)?;
                    append_text(out, ", ")?;
                    append(out, value, true, true)?;
                    append_text(out, ", ")?;
                    append(out, upper, true, true)?;
                    append_text(out, ")")
                }
                NumericExpression::Sum(values) => {
                    if !paren_less {
                        append_text(out, if nested { "(" } else { "calc(" })?;
                    }
                    let Some((first, rest)) = values.split_first() else {
                        return None;
                    };
                    append(out, first, true, false)?;
                    for value in rest {
                        if let NumericExpression::Negate(inner) = value {
                            append_text(out, " - ")?;
                            append(out, inner, true, false)?;
                        } else {
                            append_text(out, " + ")?;
                            append(out, value, true, false)?;
                        }
                    }
                    if !paren_less {
                        append_text(out, ")")?;
                    }
                    Some(())
                }
                NumericExpression::Product(values) => {
                    if !paren_less {
                        append_text(out, if nested { "(" } else { "calc(" })?;
                    }
                    let Some((first, rest)) = values.split_first() else {
                        return None;
                    };
                    append(out, first, true, false)?;
                    for value in rest {
                        if let NumericExpression::Invert(inner) = value {
                            append_text(out, " / ")?;
                            append(out, inner, true, false)?;
                        } else {
                            append_text(out, " * ")?;
                            append(out, value, true, false)?;
                        }
                    }
                    if !paren_less {
                        append_text(out, ")")?;
                    }
                    Some(())
                }
                NumericExpression::Negate(value) => {
                    if !paren_less {
                        append_text(out, if nested { "(" } else { "calc(" })?;
                    }
                    append_text(out, "-")?;
                    append(out, value, true, false)?;
                    if !paren_less {
                        append_text(out, ")")?;
                    }
                    Some(())
                }
                NumericExpression::Invert(value) => {
                    if !paren_less {
                        append_text(out, if nested { "(" } else { "calc(" })?;
                    }
                    append_text(out, "1 / ")?;
                    append(out, value, true, false)?;
                    if !paren_less {
                        append_text(out, ")")?;
                    }
                    Some(())
                }
                NumericExpression::Sign(value) => {
                    append_text(out, "sign(")?;
                    append(out, value, false, false)?;
                    append_text(out, ")")
                }
            }
        }

        if !self.within_limits() {
            return None;
        }
        let mut output = string_with_capacity(32, MAX_NUMERIC_EXPRESSION_BYTES).ok()?;
        append(&mut output, self, false, false)?;
        Some(output)
    }

    pub fn numeric_type(&self) -> Option<NumericType> {
        match self {
            Self::Value(value) => Some(NumericType::from_unit(value.unit)),
            Self::Calc(value) | Self::Negate(value) => value.numeric_type(),
            Self::Invert(value) => Some(value.numeric_type()?.invert()),
            Self::Sign(_) => Some(NumericType::default()),
            Self::Sum(values) | Self::Min(values) | Self::Max(values) => {
                let mut values = values.iter();
                let mut result = values.next()?.numeric_type()?;
                for value in values {
                    result = result.add(value.numeric_type()?)?;
                }
                Some(result)
            }
            Self::Product(values) => {
                let mut values = values.iter();
                let mut result = values.next()?.numeric_type()?;
                for value in values {
                    result = result.multiply(value.numeric_type()?)?;
                }
                Some(result)
            }
            Self::Clamp(lower, value, upper) => Some(
                lower
                    .numeric_type()?
                    .add(value.numeric_type()?)?
                    .add(upper.numeric_type()?)?,
            ),
        }
    }

    /// Simplify sums whose primitive units have a context-independent common
    /// canonical unit. Relative lengths and unresolved percentages are kept.
    pub fn simplify_absolute_units(&mut self) {
        match self {
            Self::Value(_) => return,
            Self::Calc(value) | Self::Negate(value) | Self::Invert(value) | Self::Sign(value) => {
                value.simplify_absolute_units();
                return;
            }
            Self::Clamp(lower, value, upper) => {
                lower.simplify_absolute_units();
                value.simplify_absolute_units();
                upper.simplify_absolute_units();
                return;
            }
            Self::Product(values) | Self::Min(values) | Self::Max(values) => {
                for value in values.iter_mut() {
                    value.simplify_absolute_units();
                }
                return;
            }
            Self::Sum(values) => {
                for value in values.iter_mut() {
                    value.simplify_absolute_units();
                }
                if values.len() < 2 {
                    return;
                }
            }
        }

        let Self::Sum(values) = self else {
            return;
        };
        let mut canonical = None;
        let mut sum = 0.0;
        for expression in values.iter() {
            let (number, unit, sign) = match expression {
                Self::Value(value) => (value.value, value.unit, 1.0),
                Self::Negate(value) => match value.as_ref() {
                    Self::Value(value) => (value.value, value.unit, -1.0),
                    _ => return,
                },
                _ => return,
            };
            let Some((target, factor)) = unit.canonical_unit_and_factor() else {
                return;
            };
            if canonical.is_some_and(|previous| previous != target) {
                return;
            }
            canonical = Some(target);
            sum += number * factor * sign;
            if !sum.is_finite() {
                return;
            }
        }
        let Some(unit) = canonical else {
            return;
        };
        values.clear();
        values.push(Self::Value(NumericValue { value: sum, unit }));
    }
}

/// Builder interface for the shared math grammar. A renderer can fold into a
/// small scalar accumulator; Typed OM uses the same callbacks to build a tree.
pub trait NumericExpressionBuilder {
    type Expr;
    type Accumulator;

    fn value(&mut self, value: NumericValue) -> Option<Self::Expr>;
    fn begin_sum(&mut self, first: Self::Expr) -> Option<Self::Accumulator>;
    fn push_sum(
        &mut self,
        values: &mut Self::Accumulator,
        next: Self::Expr,
        subtract: bool,
    ) -> Option<()>;
    fn finish_sum(&mut self, values: Self::Accumulator, operated: bool) -> Option<Self::Expr>;
    fn begin_product(&mut self, first: Self::Expr) -> Option<Self::Accumulator>;
    fn push_product(
        &mut self,
        values: &mut Self::Accumulator,
        next: Self::Expr,
        divide: bool,
    ) -> Option<()>;
    fn finish_product(&mut self, values: Self::Accumulator, operated: bool) -> Option<Self::Expr>;
    fn begin_min(&mut self, first: Self::Expr) -> Option<Self::Accumulator>;
    fn push_min(&mut self, values: &mut Self::Accumulator, next: Self::Expr) -> Option<()>;
    fn finish_min(&mut self, values: Self::Accumulator) -> Option<Self::Expr>;
    fn begin_max(&mut self, first: Self::Expr) -> Option<Self::Accumulator>;
    fn push_max(&mut self, values: &mut Self::Accumulator, next: Self::Expr) -> Option<()>;
    fn finish_max(&mut self, values: Self::Accumulator) -> Option<Self::Expr>;
    fn calc(&mut self, value: Self::Expr) -> Option<Self::Expr>;
    fn clamp(
        &mut self,
        lower: Self::Expr,
        value: Self::Expr,
        upper: Self::Expr,
    ) -> Option<Self::Expr>;
    fn negate(&mut self, value: Self::Expr) -> Option<Self::Expr>;
    fn invert(&mut self, value: Self::Expr) -> Option<Self::Expr>;
    /// A scalar result may still consume a dimensional argument to sign().
    fn begin_sign_input(&mut self) {}
    fn end_sign_input(&mut self) {}
    fn sign(&mut self, value: Self::Expr) -> Option<Self::Expr>;
}

/// Parse a CSS numeric primitive or math function without resolving units.
///
/// The input, nesting, argument count, and total expression-node count are
/// bounded so CSS supplied by a page cannot create unbounded parser work.
pub fn parse_numeric_expression(input: &str) -> Option<NumericExpression> {
    parse_numeric_expression_with(input, &mut NumericExpressionTreeBuilder)
}

/// Run the shared bounded numeric grammar through a caller-provided builder.
/// This keeps rendering allocation-free while allowing Typed OM to retain the
/// parsed operators and units.
pub fn parse_numeric_expression_with<B: NumericExpressionBuilder>(
    input: &str,
    builder: &mut B,
) -> Option<B::Expr> {
    if input.len() > MAX_NUMERIC_EXPRESSION_BYTES
        || input.trim_start_matches(css_whitespace).starts_with('(')
    {
        return None;
    }
    let mut parser = NumericExpressionParser {
        input,
        position: 0,
        nodes: 0,
        builder,
    };
    // The public input is a numeric token or a math function. A bare sum or
    // product is only valid inside one of those functions (for example, in
    // `calc(1px + 2px)`). Parsing a root sum here accepted non-CSS values such
    // as `1px + 2px` and `2 * 3s`.
    let expression = parser.atom(0)?;
    parser.space();
    (parser.position == input.len()).then_some(expression)
}

struct NumericExpressionParser<'a, 'b, B> {
    input: &'a str,
    position: usize,
    nodes: usize,
    builder: &'b mut B,
}

impl<B: NumericExpressionBuilder> NumericExpressionParser<'_, '_, B> {
    fn space(&mut self) {
        while self
            .input
            .as_bytes()
            .get(self.position)
            .is_some_and(|byte| matches!(byte, b'\t' | b'\n' | b'\x0c' | b'\r' | b' '))
        {
            self.position += 1;
        }
    }

    fn count_node(&mut self) -> Option<()> {
        self.nodes = self.nodes.checked_add(1)?;
        (self.nodes <= MAX_NUMERIC_EXPRESSION_NODES).then_some(())
    }

    fn sum(&mut self, depth: usize) -> Option<B::Expr> {
        let first = self.product(depth)?;
        let mut values = self.builder.begin_sum(first)?;
        let mut operated = false;
        loop {
            self.space();
            let position = self.position;
            let Some(&operator @ (b'+' | b'-')) = self.input.as_bytes().get(position) else {
                break;
            };
            if position == 0
                || !is_css_whitespace(self.input.as_bytes()[position - 1])
                || !self
                    .input
                    .as_bytes()
                    .get(position + 1)
                    .is_some_and(|byte| is_css_whitespace(*byte))
            {
                return None;
            }
            self.position += 1;
            let right = self.product(depth)?;
            self.count_node()?;
            if operator == b'-' {
                self.count_node()?;
            }
            self.builder
                .push_sum(&mut values, right, operator == b'-')?;
            operated = true;
        }
        if operated {
            self.count_node()?;
        }
        self.builder.finish_sum(values, operated)
    }

    fn product(&mut self, depth: usize) -> Option<B::Expr> {
        let first = self.atom(depth)?;
        let mut values = self.builder.begin_product(first)?;
        let mut operated = false;
        loop {
            self.space();
            let Some(&operator @ (b'*' | b'/')) = self.input.as_bytes().get(self.position) else {
                break;
            };
            self.position += 1;
            let right = self.atom(depth)?;
            self.count_node()?;
            if operator == b'/' {
                self.count_node()?;
            }
            self.builder
                .push_product(&mut values, right, operator == b'/')?;
            operated = true;
        }
        if operated {
            self.count_node()?;
        }
        self.builder.finish_product(values, operated)
    }

    fn atom(&mut self, depth: usize) -> Option<B::Expr> {
        if depth > MAX_NUMERIC_EXPRESSION_DEPTH {
            return None;
        }
        self.space();
        let rest = self.input.get(self.position..)?;
        for (name, kind) in [
            ("calc(", 0u8),
            ("min(", 1),
            ("max(", 2),
            ("clamp(", 3),
            ("sign(", 4),
        ] {
            if rest
                .get(..name.len())
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(name))
            {
                self.position += name.len();
                if kind == 1 || kind == 2 {
                    let first = self.sum(depth + 1)?;
                    let mut values = if kind == 1 {
                        self.builder.begin_min(first)?
                    } else {
                        self.builder.begin_max(first)?
                    };
                    let mut count = 1usize;
                    loop {
                        self.space();
                        match self.input.as_bytes().get(self.position) {
                            Some(b',') => self.position += 1,
                            Some(b')') => {
                                self.position += 1;
                                break;
                            }
                            _ => return None,
                        }
                        if count == MAX_NUMERIC_EXPRESSION_ARGS {
                            return None;
                        }
                        let next = self.sum(depth + 1)?;
                        if kind == 1 {
                            self.builder.push_min(&mut values, next)?;
                        } else {
                            self.builder.push_max(&mut values, next)?;
                        }
                        count += 1;
                    }
                    self.count_node()?;
                    if kind == 1 {
                        return self.builder.finish_min(values);
                    } else {
                        return self.builder.finish_max(values);
                    }
                }
                if kind == 3 {
                    let lower = self.sum(depth + 1)?;
                    self.space();
                    if self.input.as_bytes().get(self.position) != Some(&b',') {
                        return None;
                    }
                    self.position += 1;
                    let value = self.sum(depth + 1)?;
                    self.space();
                    if self.input.as_bytes().get(self.position) != Some(&b',') {
                        return None;
                    }
                    self.position += 1;
                    let upper = self.sum(depth + 1)?;
                    self.space();
                    if self.input.as_bytes().get(self.position) != Some(&b')') {
                        return None;
                    }
                    self.position += 1;
                    self.count_node()?;
                    return self.builder.clamp(lower, value, upper);
                }
                if kind == 4 {
                    self.builder.begin_sign_input();
                }
                let value = self.sum(depth + 1);
                if kind == 4 {
                    self.builder.end_sign_input();
                }
                let value = value?;
                self.space();
                if self.input.as_bytes().get(self.position) != Some(&b')') {
                    return None;
                }
                self.position += 1;
                self.count_node()?;
                return if kind == 0 {
                    self.builder.calc(value)
                } else {
                    self.builder.sign(value)
                };
            }
        }
        if self.input.as_bytes().get(self.position) == Some(&b'(') {
            self.position += 1;
            let expression = self.sum(depth + 1)?;
            self.space();
            if self.input.as_bytes().get(self.position) != Some(&b')') {
                return None;
            }
            self.position += 1;
            return Some(expression);
        }
        let (value, consumed) = parse_numeric_prefix(rest)?;
        self.position = self.position.checked_add(consumed)?;
        self.count_node()?;
        self.builder.value(value)
    }
}

struct NumericExpressionTreeBuilder;

impl NumericExpressionBuilder for NumericExpressionTreeBuilder {
    type Expr = NumericExpression;
    type Accumulator = Vec<NumericExpression>;

    fn value(&mut self, value: NumericValue) -> Option<Self::Expr> {
        Some(NumericExpression::Value(value))
    }

    fn begin_sum(&mut self, first: Self::Expr) -> Option<Self::Accumulator> {
        let mut values = Vec::new();
        values.try_reserve(4).ok()?;
        values.push(first);
        Some(values)
    }

    fn push_sum(
        &mut self,
        values: &mut Self::Accumulator,
        next: Self::Expr,
        subtract: bool,
    ) -> Option<()> {
        if values.len() >= MAX_NUMERIC_EXPRESSION_ARGS {
            return None;
        }
        values.try_reserve(1).ok()?;
        values.push(if subtract {
            NumericExpression::Negate(Box::new(next))
        } else {
            next
        });
        Some(())
    }

    fn finish_sum(&mut self, mut values: Self::Accumulator, operated: bool) -> Option<Self::Expr> {
        if operated {
            Some(NumericExpression::Sum(values))
        } else {
            values.pop()
        }
    }

    fn begin_product(&mut self, first: Self::Expr) -> Option<Self::Accumulator> {
        self.begin_sum(first)
    }

    fn push_product(
        &mut self,
        values: &mut Self::Accumulator,
        next: Self::Expr,
        divide: bool,
    ) -> Option<()> {
        if values.len() >= MAX_NUMERIC_EXPRESSION_ARGS {
            return None;
        }
        values.try_reserve(1).ok()?;
        values.push(if divide {
            NumericExpression::Invert(Box::new(next))
        } else {
            next
        });
        Some(())
    }

    fn finish_product(
        &mut self,
        mut values: Self::Accumulator,
        operated: bool,
    ) -> Option<Self::Expr> {
        if operated {
            Some(NumericExpression::Product(values))
        } else {
            values.pop()
        }
    }

    fn begin_min(&mut self, first: Self::Expr) -> Option<Self::Accumulator> {
        self.begin_sum(first)
    }

    fn push_min(&mut self, values: &mut Self::Accumulator, next: Self::Expr) -> Option<()> {
        if values.len() >= MAX_NUMERIC_EXPRESSION_ARGS {
            return None;
        }
        values.try_reserve(1).ok()?;
        values.push(next);
        Some(())
    }

    fn finish_min(&mut self, values: Self::Accumulator) -> Option<Self::Expr> {
        Some(NumericExpression::Min(values))
    }

    fn begin_max(&mut self, first: Self::Expr) -> Option<Self::Accumulator> {
        self.begin_sum(first)
    }

    fn push_max(&mut self, values: &mut Self::Accumulator, next: Self::Expr) -> Option<()> {
        self.push_min(values, next)
    }

    fn finish_max(&mut self, values: Self::Accumulator) -> Option<Self::Expr> {
        Some(NumericExpression::Max(values))
    }

    fn calc(&mut self, value: Self::Expr) -> Option<Self::Expr> {
        Some(NumericExpression::Calc(Box::new(value)))
    }

    fn clamp(
        &mut self,
        lower: Self::Expr,
        value: Self::Expr,
        upper: Self::Expr,
    ) -> Option<Self::Expr> {
        Some(NumericExpression::Clamp(
            Box::new(lower),
            Box::new(value),
            Box::new(upper),
        ))
    }

    fn negate(&mut self, value: Self::Expr) -> Option<Self::Expr> {
        Some(NumericExpression::Negate(Box::new(value)))
    }

    fn invert(&mut self, value: Self::Expr) -> Option<Self::Expr> {
        Some(NumericExpression::Invert(Box::new(value)))
    }

    fn sign(&mut self, value: Self::Expr) -> Option<Self::Expr> {
        Some(NumericExpression::Sign(Box::new(value)))
    }
}

fn is_css_whitespace(byte: u8) -> bool {
    matches!(byte, b'\t' | b'\n' | b'\x0c' | b'\r' | b' ')
}

/// CSS Typed OM base dimension used by `CSSNumericValue.type()`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NumericDimension {
    Number,
    Percent,
    Length,
    Angle,
    Time,
    Frequency,
    Resolution,
    Flex,
}

/// Dimensional exponents used by CSS Numeric Type checking.
///
/// Percentages remain explicit unless compatible with another dimension, in
/// which case `percent_hint` records the type against which they resolve.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NumericType {
    pub length: i8,
    pub angle: i8,
    pub time: i8,
    pub frequency: i8,
    pub resolution: i8,
    pub flex: i8,
    pub percent: i8,
    pub percent_hint: Option<NumericDimension>,
}

impl NumericType {
    pub const fn from_unit(unit: NumericUnit) -> Self {
        let mut value = Self {
            length: 0,
            angle: 0,
            time: 0,
            frequency: 0,
            resolution: 0,
            flex: 0,
            percent: 0,
            percent_hint: None,
        };
        match unit.dimension() {
            NumericDimension::Number => {}
            NumericDimension::Percent => value.percent = 1,
            NumericDimension::Length => value.length = 1,
            NumericDimension::Angle => value.angle = 1,
            NumericDimension::Time => value.time = 1,
            NumericDimension::Frequency => value.frequency = 1,
            NumericDimension::Resolution => value.resolution = 1,
            NumericDimension::Flex => value.flex = 1,
        }
        value
    }

    pub fn add(self, other: Self) -> Option<Self> {
        if self.percent_hint.is_some()
            && other.percent_hint.is_some()
            && self.percent_hint != other.percent_hint
        {
            return None;
        }
        let hint = self.percent_hint.or(other.percent_hint);
        let mut left = self.apply_percent_hint(hint)?;
        let mut right = other.apply_percent_hint(hint)?;
        if left.exponents_equal(right) {
            left.percent_hint = hint;
            return Some(left);
        }
        let has_percent_mix = (left.percent != 0 && right.has_non_percent())
            || (right.percent != 0 && left.has_non_percent());
        if !has_percent_mix {
            return None;
        }
        for candidate in [
            NumericDimension::Length,
            NumericDimension::Angle,
            NumericDimension::Time,
            NumericDimension::Frequency,
            NumericDimension::Resolution,
            NumericDimension::Flex,
        ] {
            let mut candidate_left = self.apply_percent_hint(Some(candidate))?;
            let candidate_right = other.apply_percent_hint(Some(candidate))?;
            if candidate_left.exponents_equal(candidate_right) {
                candidate_left.percent_hint = Some(candidate);
                return Some(candidate_left);
            }
        }
        None
    }

    pub fn multiply(self, other: Self) -> Option<Self> {
        if self.percent_hint.is_some()
            && other.percent_hint.is_some()
            && self.percent_hint != other.percent_hint
        {
            return None;
        }
        let hint = self.percent_hint.or(other.percent_hint);
        let left = self.apply_percent_hint(hint)?;
        let right = other.apply_percent_hint(hint)?;
        Some(Self {
            length: left.length.checked_add(right.length)?,
            angle: left.angle.checked_add(right.angle)?,
            time: left.time.checked_add(right.time)?,
            frequency: left.frequency.checked_add(right.frequency)?,
            resolution: left.resolution.checked_add(right.resolution)?,
            flex: left.flex.checked_add(right.flex)?,
            percent: left.percent.checked_add(right.percent)?,
            percent_hint: hint,
        })
    }

    pub fn invert(self) -> Self {
        Self {
            length: -self.length,
            angle: -self.angle,
            time: -self.time,
            frequency: -self.frequency,
            resolution: -self.resolution,
            flex: -self.flex,
            percent: -self.percent,
            percent_hint: self.percent_hint,
        }
    }

    fn apply_percent_hint(self, hint: Option<NumericDimension>) -> Option<Self> {
        if self.percent_hint.is_some() && self.percent_hint != hint {
            return None;
        }
        let Some(hint) = hint else {
            return Some(self);
        };
        let mut value = self;
        value.percent_hint = Some(hint);
        match hint {
            NumericDimension::Number | NumericDimension::Percent => {}
            NumericDimension::Length => {
                value.length = value.length.checked_add(value.percent)?;
                value.percent = 0;
            }
            NumericDimension::Angle => {
                value.angle = value.angle.checked_add(value.percent)?;
                value.percent = 0;
            }
            NumericDimension::Time => {
                value.time = value.time.checked_add(value.percent)?;
                value.percent = 0;
            }
            NumericDimension::Frequency => {
                value.frequency = value.frequency.checked_add(value.percent)?;
                value.percent = 0;
            }
            NumericDimension::Resolution => {
                value.resolution = value.resolution.checked_add(value.percent)?;
                value.percent = 0;
            }
            NumericDimension::Flex => {
                value.flex = value.flex.checked_add(value.percent)?;
                value.percent = 0;
            }
        }
        Some(value)
    }

    fn has_non_percent(self) -> bool {
        self.length != 0
            || self.angle != 0
            || self.time != 0
            || self.frequency != 0
            || self.resolution != 0
            || self.flex != 0
    }

    fn exponents_equal(self, other: Self) -> bool {
        self.length == other.length
            && self.angle == other.angle
            && self.time == other.time
            && self.frequency == other.frequency
            && self.resolution == other.resolution
            && self.flex == other.flex
            && self.percent == other.percent
    }
}

impl NumericUnit {
    /// Parse the case-sensitive CSS Typed OM unit string.
    pub fn parse(unit: &str) -> Option<Self> {
        Some(match unit {
            "number" => Self::Number,
            "percent" => Self::Percent,
            "cap" => Self::Cap,
            "em" => Self::Em,
            "ex" => Self::Ex,
            "ch" => Self::Ch,
            "ic" => Self::Ic,
            "rem" => Self::Rem,
            "lh" => Self::Lh,
            "rlh" => Self::Rlh,
            "rcap" => Self::Rcap,
            "rch" => Self::Rch,
            "rex" => Self::Rex,
            "ric" => Self::Ric,
            "vw" => Self::Vw,
            "vh" => Self::Vh,
            "vi" => Self::Vi,
            "vb" => Self::Vb,
            "vmin" => Self::Vmin,
            "vmax" => Self::Vmax,
            "svw" => Self::Svw,
            "svh" => Self::Svh,
            "svi" => Self::Svi,
            "svb" => Self::Svb,
            "svmin" => Self::Svmin,
            "svmax" => Self::Svmax,
            "lvw" => Self::Lvw,
            "lvh" => Self::Lvh,
            "lvi" => Self::Lvi,
            "lvb" => Self::Lvb,
            "lvmin" => Self::Lvmin,
            "lvmax" => Self::Lvmax,
            "dvw" => Self::Dvw,
            "dvh" => Self::Dvh,
            "dvi" => Self::Dvi,
            "dvb" => Self::Dvb,
            "dvmin" => Self::Dvmin,
            "dvmax" => Self::Dvmax,
            "cqw" => Self::Cqw,
            "cqh" => Self::Cqh,
            "cqi" => Self::Cqi,
            "cqb" => Self::Cqb,
            "cqmin" => Self::Cqmin,
            "cqmax" => Self::Cqmax,
            "cm" => Self::Cm,
            "mm" => Self::Mm,
            "Q" => Self::Q,
            "in" => Self::In,
            "pt" => Self::Pt,
            "pc" => Self::Pc,
            "px" => Self::Px,
            "deg" => Self::Deg,
            "grad" => Self::Grad,
            "rad" => Self::Rad,
            "turn" => Self::Turn,
            "s" => Self::S,
            "ms" => Self::Ms,
            "Hz" => Self::Hz,
            "kHz" => Self::KHz,
            "dpi" => Self::Dpi,
            "dpcm" => Self::Dpcm,
            "dppx" => Self::Dppx,
            "fr" => Self::Fr,
            _ => return None,
        })
    }

    /// Parse a CSS dimension unit, whose token spelling is ASCII
    /// case-insensitive, and return the canonical Typed OM unit.
    pub fn parse_css(unit: &str) -> Option<Self> {
        let lower = unit.to_ascii_lowercase();
        match lower.as_str() {
            "q" => Some(Self::Q),
            "hz" => Some(Self::Hz),
            "khz" => Some(Self::KHz),
            _ => Self::parse(&lower),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Number => "number",
            Self::Percent => "percent",
            Self::Cap => "cap",
            Self::Em => "em",
            Self::Ex => "ex",
            Self::Ch => "ch",
            Self::Ic => "ic",
            Self::Rem => "rem",
            Self::Lh => "lh",
            Self::Rlh => "rlh",
            Self::Rcap => "rcap",
            Self::Rch => "rch",
            Self::Rex => "rex",
            Self::Ric => "ric",
            Self::Vw => "vw",
            Self::Vh => "vh",
            Self::Vi => "vi",
            Self::Vb => "vb",
            Self::Vmin => "vmin",
            Self::Vmax => "vmax",
            Self::Svw => "svw",
            Self::Svh => "svh",
            Self::Svi => "svi",
            Self::Svb => "svb",
            Self::Svmin => "svmin",
            Self::Svmax => "svmax",
            Self::Lvw => "lvw",
            Self::Lvh => "lvh",
            Self::Lvi => "lvi",
            Self::Lvb => "lvb",
            Self::Lvmin => "lvmin",
            Self::Lvmax => "lvmax",
            Self::Dvw => "dvw",
            Self::Dvh => "dvh",
            Self::Dvi => "dvi",
            Self::Dvb => "dvb",
            Self::Dvmin => "dvmin",
            Self::Dvmax => "dvmax",
            Self::Cqw => "cqw",
            Self::Cqh => "cqh",
            Self::Cqi => "cqi",
            Self::Cqb => "cqb",
            Self::Cqmin => "cqmin",
            Self::Cqmax => "cqmax",
            Self::Cm => "cm",
            Self::Mm => "mm",
            Self::Q => "q",
            Self::In => "in",
            Self::Pt => "pt",
            Self::Pc => "pc",
            Self::Px => "px",
            Self::Deg => "deg",
            Self::Grad => "grad",
            Self::Rad => "rad",
            Self::Turn => "turn",
            Self::S => "s",
            Self::Ms => "ms",
            Self::Hz => "hz",
            Self::KHz => "khz",
            Self::Dpi => "dpi",
            Self::Dpcm => "dpcm",
            Self::Dppx => "dppx",
            Self::Fr => "fr",
        }
    }

    pub const fn dimension(self) -> NumericDimension {
        match self {
            Self::Number => NumericDimension::Number,
            Self::Percent => NumericDimension::Percent,
            Self::Em
            | Self::Ex
            | Self::Ch
            | Self::Ic
            | Self::Rem
            | Self::Lh
            | Self::Rlh
            | Self::Cap
            | Self::Rcap
            | Self::Rch
            | Self::Rex
            | Self::Ric
            | Self::Vw
            | Self::Vh
            | Self::Vi
            | Self::Vb
            | Self::Vmin
            | Self::Vmax
            | Self::Svw
            | Self::Svh
            | Self::Svi
            | Self::Svb
            | Self::Svmin
            | Self::Svmax
            | Self::Lvw
            | Self::Lvh
            | Self::Lvi
            | Self::Lvb
            | Self::Lvmin
            | Self::Lvmax
            | Self::Dvw
            | Self::Dvh
            | Self::Dvi
            | Self::Dvb
            | Self::Dvmin
            | Self::Dvmax
            | Self::Cqw
            | Self::Cqh
            | Self::Cqi
            | Self::Cqb
            | Self::Cqmin
            | Self::Cqmax
            | Self::Cm
            | Self::Mm
            | Self::Q
            | Self::In
            | Self::Pt
            | Self::Pc
            | Self::Px => NumericDimension::Length,
            Self::Deg | Self::Grad | Self::Rad | Self::Turn => NumericDimension::Angle,
            Self::S | Self::Ms => NumericDimension::Time,
            Self::Hz | Self::KHz => NumericDimension::Frequency,
            Self::Dpi | Self::Dpcm | Self::Dppx => NumericDimension::Resolution,
            Self::Fr => NumericDimension::Flex,
        }
    }

    /// Return a context-independent canonical unit and scale factor, if one
    /// exists for this unit.
    pub const fn canonical_unit_and_factor(self) -> Option<(Self, f64)> {
        match self {
            Self::Number => Some((Self::Number, 1.0)),
            Self::Percent => Some((Self::Percent, 1.0)),
            Self::Px => Some((Self::Px, 1.0)),
            Self::In => Some((Self::Px, 96.0)),
            Self::Cm => Some((Self::Px, 96.0 / 2.54)),
            Self::Mm => Some((Self::Px, 96.0 / 25.4)),
            Self::Q => Some((Self::Px, 96.0 / 101.6)),
            Self::Pt => Some((Self::Px, 96.0 / 72.0)),
            Self::Pc => Some((Self::Px, 16.0)),
            Self::Deg => Some((Self::Deg, 1.0)),
            Self::Grad => Some((Self::Deg, 0.9)),
            Self::Rad => Some((Self::Deg, 180.0 / core::f64::consts::PI)),
            Self::Turn => Some((Self::Deg, 360.0)),
            Self::S => Some((Self::S, 1.0)),
            Self::Ms => Some((Self::S, 0.001)),
            Self::Hz => Some((Self::Hz, 1.0)),
            Self::KHz => Some((Self::Hz, 1000.0)),
            Self::Dpi => Some((Self::Dppx, 1.0 / 96.0)),
            Self::Dpcm => Some((Self::Dppx, 2.54 / 96.0)),
            Self::Dppx => Some((Self::Dppx, 1.0)),
            _ => None,
        }
    }
}

/// Parse exactly one CSS number, percentage, or dimension token.
///
/// The CSS property grammar is validated separately by `supports_declaration`.
/// This helper only performs numeric-token recognition and unit classification;
/// it does not pretend to evaluate a math expression.
pub fn parse_numeric_value(input: &str) -> Option<NumericValue> {
    const MAX_NUMERIC_TOKEN_BYTES: usize = 1024;
    let input = input.trim_matches(css_whitespace);
    if input.is_empty() || input.len() > MAX_NUMERIC_TOKEN_BYTES {
        return None;
    }
    let (value, consumed) = parse_numeric_prefix(input)?;
    (consumed == input.len()).then_some(value)
}

fn parse_numeric_prefix(input: &str) -> Option<(NumericValue, usize)> {
    let bytes = input.as_bytes();
    let mut position = 0usize;
    if matches!(bytes.get(position), Some(b'+' | b'-')) {
        position += 1;
    }

    let whole_start = position;
    while bytes.get(position).is_some_and(u8::is_ascii_digit) {
        position += 1;
    }
    let whole_digits = position != whole_start;
    let mut digits = whole_digits;
    if bytes.get(position) == Some(&b'.') {
        position += 1;
        let fraction_start = position;
        while bytes.get(position).is_some_and(u8::is_ascii_digit) {
            position += 1;
        }
        if fraction_start == position {
            return None;
        }
        digits |= fraction_start != position;
    }
    if !digits {
        return None;
    }
    if matches!(bytes.get(position), Some(b'e' | b'E')) {
        let exponent_start = position;
        let mut exponent = position + 1;
        if matches!(bytes.get(exponent), Some(b'+' | b'-')) {
            exponent += 1;
        }
        let digit_start = exponent;
        while bytes.get(exponent).is_some_and(u8::is_ascii_digit) {
            exponent += 1;
        }
        if exponent == digit_start {
            // `e` starts a unit unless it forms a complete exponent.
            position = exponent_start;
        } else {
            position = exponent;
        }
    }

    let value = input[..position].parse::<f64>().ok()?;
    if !value.is_finite() {
        return None;
    }
    let suffix_start = position;
    while bytes.get(position).is_some_and(u8::is_ascii_alphabetic) {
        position += 1;
    }
    if bytes.get(position) == Some(&b'%') {
        position += 1;
    }
    let suffix = &input[suffix_start..position];
    let unit = if suffix == "%" {
        NumericUnit::Percent
    } else if suffix.is_empty() {
        NumericUnit::Number
    } else if suffix.bytes().all(|byte| byte.is_ascii_alphabetic()) {
        NumericUnit::parse_css(suffix)?
    } else {
        return None;
    };
    Some((NumericValue { value, unit }, position))
}

/// Reify a parsed token for a property. CSS's unitless zero is represented as
/// `0px` only when the property's registered grammar accepts dimensions but
/// does not accept ordinary numbers (for example `width`, but not `line-height`).
pub fn parse_property_numeric_value(property: &str, input: &str) -> Option<NumericValue> {
    let mut numeric = parse_numeric_value(input)?;
    if numeric.unit == NumericUnit::Number
        && numeric.value == 0.0
        && supports_declaration(property, "1px")
        && !supports_declaration(property, "1")
    {
        numeric.unit = NumericUnit::Px;
    }
    Some(numeric)
}

/// Divide a validated value into Typed OM iterations for the registered
/// list-valued properties supported by the renderer. Values containing `var()`
/// stay whole because unresolved component values cannot be safely subdivided.
pub fn property_value_iterations(property: &str, input: &str) -> Option<Vec<String>> {
    const MAX_TYPED_PROPERTY_BYTES: usize = 8 * 1024;
    if input.len() > MAX_TYPED_PROPERTY_BYTES {
        return None;
    }
    let components = parse_unparsed_value(input).ok()?;
    if components
        .iter()
        .any(|component| matches!(component, UnparsedComponent::Variable { .. }))
        || !is_list_valued_property(property)
    {
        return Some(vec![input.trim_matches(css_whitespace).into()]);
    }
    let values = css_list_items(input);
    (!values.is_empty()).then_some(values)
}

/// Return whether a property has independently reified comma-separated values.
/// This list is the CSS Typed OM iteration behavior for the currently supported
/// declarations; shorthand declarations remain a single value.
pub fn is_list_valued_property(property: &str) -> bool {
    const LIST_PROPERTIES: &[&str] = &[
        "animation",
        "animation-composition",
        "animation-delay",
        "animation-direction",
        "animation-duration",
        "animation-fill-mode",
        "animation-iteration-count",
        "animation-name",
        "animation-play-state",
        "animation-timing-function",
        "background",
        "background-attachment",
        "background-clip",
        "background-image",
        "background-origin",
        "background-position",
        "background-position-x",
        "background-position-y",
        "background-repeat",
        "background-size",
        "box-shadow",
        "cursor",
        "font-family",
        "mask",
        "mask-clip",
        "mask-composite",
        "mask-image",
        "mask-mode",
        "mask-origin",
        "mask-position",
        "mask-repeat",
        "mask-size",
        "text-shadow",
        "transition",
        "transition-behavior",
        "transition-delay",
        "transition-duration",
        "transition-property",
        "transition-timing-function",
    ];
    let property = property.to_ascii_lowercase();
    LIST_PROPERTIES.contains(&property.as_str())
}

/// Serialize a number and unit using CSS numeric-value formatting.
pub fn serialize_numeric_value(value: f64, unit: NumericUnit) -> String {
    let number = if value == 0.0 {
        String::from("0")
    } else {
        alloc::format!("{value}")
    };
    match unit {
        NumericUnit::Number => number,
        NumericUnit::Percent => alloc::format!("{number}%"),
        unit => alloc::format!("{number}{}", unit.as_str()),
    }
}

fn css_whitespace(character: char) -> bool {
    matches!(character, '\t' | '\n' | '\u{c}' | '\r' | ' ')
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn numeric_primitives_preserve_units_and_reject_extra_tokens() {
        assert_eq!(
            parse_numeric_value("  -2.5px  "),
            Some(NumericValue {
                value: -2.5,
                unit: NumericUnit::Px,
            })
        );
        assert_eq!(
            parse_numeric_value("25%").unwrap().unit,
            NumericUnit::Percent
        );
        assert_eq!(parse_numeric_value("2Q").unwrap().unit, NumericUnit::Q);
        assert_eq!(parse_numeric_value("3KHZ").unwrap().unit, NumericUnit::KHz);
        assert_eq!(NumericUnit::parse("Q"), Some(NumericUnit::Q));
        assert_eq!(NumericUnit::parse("q"), None);
        assert_eq!(parse_numeric_value("1e2ms").unwrap().value, 100.0);
        assert_eq!(parse_numeric_value("1e2ms").unwrap().unit, NumericUnit::Ms);
        assert_eq!(parse_numeric_value(".5turn").unwrap().value, 0.5);
        assert_eq!(parse_numeric_value("1."), None);
        assert_eq!(parse_numeric_value("1.px"), None);
        assert_eq!(serialize_numeric_value(30.0, NumericUnit::Q), "30q");
        assert_eq!(serialize_numeric_value(2.0, NumericUnit::Hz), "2hz");
        assert_eq!(serialize_numeric_value(3.0, NumericUnit::KHz), "3khz");
        for invalid in ["", "1 2", "1px junk", "1xyz", "NaN", "1e+", "calc(1px)"] {
            assert_eq!(parse_numeric_value(invalid), None, "accepted {invalid:?}");
        }
    }

    #[test]
    fn property_reification_distinguishes_zero_length_from_number() {
        assert_eq!(
            parse_property_numeric_value("width", "0").unwrap().unit,
            NumericUnit::Px
        );
        assert_eq!(
            parse_property_numeric_value("line-height", "0")
                .unwrap()
                .unit,
            NumericUnit::Number
        );
        assert_eq!(
            parse_property_numeric_value("opacity", "0").unwrap().unit,
            NumericUnit::Number
        );
    }

    #[test]
    fn property_iterations_split_only_registered_lists_and_keep_variables_whole() {
        assert_eq!(
            property_value_iterations("transition-duration", "1s, 2s, 3s").unwrap(),
            vec![String::from("1s"), String::from("2s"), String::from("3s")]
        );
        assert_eq!(
            property_value_iterations("margin", "1px 2px").unwrap(),
            vec![String::from("1px 2px")]
        );
        assert_eq!(
            property_value_iterations("transition-duration", "1s, var(--later, 2s)").unwrap(),
            vec![String::from("1s, var(--later, 2s)")]
        );
        assert_eq!(serialize_numeric_value(0.0, NumericUnit::Percent), "0%");
    }

    #[test]
    fn expression_parser_retains_typed_math_shape_and_bounds_work() {
        let expression =
            parse_numeric_expression("calc(9em - 8px + 1vh + (2 * min(10px, 20%)))").unwrap();
        assert!(matches!(
            expression,
            NumericExpression::Calc(ref value)
                if matches!(value.as_ref(), NumericExpression::Sum(terms)
                    if matches!(terms.as_slice(), [
                        NumericExpression::Value(_),
                        NumericExpression::Negate(_),
                        NumericExpression::Value(_),
                        NumericExpression::Product(_)
                    ]))
        ));
        assert!(expression.numeric_type().is_some());

        let invalid_type = parse_numeric_expression("calc(1px + 1s)").unwrap();
        assert_eq!(invalid_type.numeric_type(), None);
        assert!(
            parse_numeric_expression("calc(1px + 1s)")
                .unwrap()
                .contains_sign()
                == false
        );

        let percent_length = parse_numeric_expression("calc(1px + 2%)").unwrap();
        assert_eq!(
            percent_length.numeric_type(),
            Some(NumericType {
                length: 1,
                percent_hint: Some(NumericDimension::Length),
                ..NumericType::default()
            })
        );

        let mut absolute = parse_numeric_expression("calc(1px + 1in)").unwrap();
        absolute.simplify_absolute_units();
        assert!(matches!(
            absolute,
            NumericExpression::Calc(ref value)
                if matches!(value.as_ref(), NumericExpression::Sum(terms)
                    if matches!(terms.as_slice(), [NumericExpression::Value(NumericValue {
                        value: 97.0,
                        unit: NumericUnit::Px,
                    })]))
        ));

        let too_deep = alloc::format!("{}1px{}", "calc(".repeat(18), ")".repeat(18));
        assert!(parse_numeric_expression(&too_deep).is_none());
        let too_many = alloc::format!(
            "min({})",
            (0..=MAX_NUMERIC_EXPRESSION_ARGS)
                .map(|_| "1px")
                .collect::<Vec<_>>()
                .join(",")
        );
        assert!(parse_numeric_expression(&too_many).is_none());
        assert!(
            parse_numeric_expression("calc(sign(1px))")
                .unwrap()
                .contains_sign()
        );

        assert!(parse_numeric_expression("1px + 2px").is_none());
        assert!(parse_numeric_expression("2 * 3s").is_none());
        assert!(matches!(
            parse_numeric_expression("calc(2 * 3s)"),
            Some(NumericExpression::Calc(value))
                if matches!(value.as_ref(), NumericExpression::Product(_))
        ));

        let sum = parse_numeric_expression("calc(1px + 2 * 3)").unwrap();
        assert_eq!(sum.serialize().as_deref(), Some("calc(1px + (2 * 3))"));
        let min = parse_numeric_expression("min(calc(1px + 2px), 3px)").unwrap();
        assert_eq!(min.serialize().as_deref(), Some("min(1px + 2px, 3px)"));
    }

    #[test]
    fn numeric_type_algebra_tracks_dimensions_and_percent_hints() {
        let length_squared = NumericType::from_unit(NumericUnit::Px)
            .multiply(NumericType::from_unit(NumericUnit::Px))
            .unwrap();
        assert_eq!(length_squared.length, 2);
        assert!(
            length_squared
                .add(NumericType::from_unit(NumericUnit::Percent))
                .is_none()
        );

        let length_percent = NumericType::from_unit(NumericUnit::Px)
            .add(NumericType::from_unit(NumericUnit::Percent))
            .unwrap();
        assert_eq!(length_percent.length, 1);
        assert_eq!(length_percent.percent, 0);
        assert_eq!(length_percent.percent_hint, Some(NumericDimension::Length));

        let inverse_time = NumericType::from_unit(NumericUnit::S).invert();
        assert_eq!(inverse_time.time, -1);
    }
}
