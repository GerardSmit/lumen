//! Shared Typed OM numeric primitives and property iteration helpers.
//!
//! Numeric CSS values share one bounded expression parser. Rendering supplies
//! a context resolver; Typed OM keeps the expression tree for reification.

use alloc::{boxed::Box, string::{String,ToString}, vec, vec::Vec};

use super::{css_list_items, parse_unparsed_value, supports_declaration, UnparsedComponent};

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

/// CSS calculation comparisons propagate NaN and order signed zeroes.
pub fn numeric_minimum(left: f64, right: f64) -> f64 {
    if left.is_nan() || right.is_nan() { f64::NAN }
    else if left == 0.0 && right == 0.0 { if left.is_sign_negative() || right.is_sign_negative() { -0.0 } else { 0.0 } }
    else { left.min(right) }
}

pub fn numeric_maximum(left: f64, right: f64) -> f64 {
    if left.is_nan() || right.is_nan() { f64::NAN }
    else if left == 0.0 && right == 0.0 { if left.is_sign_negative() && right.is_sign_negative() { -0.0 } else { 0.0 } }
    else { left.max(right) }
}

/// A bounded, language-neutral CSS numeric expression.
///
/// `Calc` preserves an explicit `calc()` boundary because Typed OM reification
/// turns it into a `CSSMathSum`, even when its operand is a single value.
#[derive(Clone, Debug, PartialEq)]
pub enum NumericExpression {
    Value(NumericValue),
    /// Scoped numeric keyword admitted only by a caller-provided named builder.
    Identifier(&'static str),
    Calc(Box<Self>),
    Sum(Vec<Self>),
    Product(Vec<Self>),
    Min(Vec<Self>),
    Max(Vec<Self>),
    Clamp(Box<Self>, Box<Self>, Box<Self>),
    Negate(Box<Self>),
    Invert(Box<Self>),
    Sign(Box<Self>),
    Function(MathFunction, Vec<Self>),
}

/// CSS Values math functions share type checking and arithmetic with the AST.
#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub enum MathFunction { Sin, Cos, Tan, Asin, Acos, Atan, Atan2, Pow, Sqrt, Hypot, Log, Exp, Abs, Mod, Rem, Round(RoundingStrategy), Progress {clamp:bool}, SiblingIndex, SiblingCount }

#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub enum RoundingStrategy { Nearest, Up, Down, ToZero }
impl RoundingStrategy {
    fn name(self)->&'static str {match self {Self::Nearest=>"nearest",Self::Up=>"up",Self::Down=>"down",Self::ToZero=>"to-zero"}}
}
impl MathFunction {
    pub fn name(self)->&'static str {match self {Self::Sin=>"sin",Self::Cos=>"cos",Self::Tan=>"tan",Self::Asin=>"asin",Self::Acos=>"acos",Self::Atan=>"atan",Self::Atan2=>"atan2",Self::Pow=>"pow",Self::Sqrt=>"sqrt",Self::Hypot=>"hypot",Self::Log=>"log",Self::Exp=>"exp",Self::Abs=>"abs",Self::Mod=>"mod",Self::Rem=>"rem",Self::Round(_)=>"round",Self::Progress{..}=>"progress",Self::SiblingIndex=>"sibling-index",Self::SiblingCount=>"sibling-count"}}
    fn kind(self,types:&[NumericType])->Option<NumericType>{
        if matches!(self,Self::SiblingIndex|Self::SiblingCount){return types.is_empty().then_some(NumericType::default());}
        let first=*types.first()?;let number=NumericType::default();let angle=NumericType::from_unit(NumericUnit::Deg);
        match self {
            Self::Progress{..} if types.len()==3=>{
                let consistent=first.add(types[1])?.add(types[2])?;
                // A progress argument is a number, percentage or dimension;
                // squared dimensions are calculations but not this production.
                let plain=NumericType{percent_hint:None,..consistent};
                if ![NumericUnit::Number,NumericUnit::Percent,NumericUnit::Px,NumericUnit::Deg,NumericUnit::S,NumericUnit::Hz,NumericUnit::Dppx,NumericUnit::Fr].into_iter().any(|unit|plain==NumericType::from_unit(unit)){return None;}
                Some(NumericType{percent_hint:consistent.percent_hint,..number})
            },
            Self::Sin|Self::Cos|Self::Tan if types.len()==1&&(first==number||first==angle)=>Some(number),
            Self::Asin|Self::Acos|Self::Atan if types.len()==1&&first==number=>Some(angle),
            Self::Atan2 if types.len()==2 && first==types[1]=>Some(angle),
            Self::Pow if types.len()==2&&types.iter().all(|kind|*kind==number)=>Some(number),
            Self::Sqrt|Self::Exp if types.len()==1&&first==number=>Some(number),
            Self::Log if (1..=2).contains(&types.len())&&types.iter().all(|kind|*kind==number)=>Some(number),
            Self::Abs if types.len()==1=>Some(first),
            Self::Mod|Self::Rem|Self::Round(_) if types.len()==2=>first.add(types[1]),
            Self::Round(_) if types.len()==1&&first==number=>Some(number),
            Self::Hypot=>{let mut result=first;for next in &types[1..]{result=result.add(*next)?;}Some(result)},
            _=>None,
        }
    }
    /// Numeric arguments use canonical units: angle inputs are degrees.
    pub fn evaluate(self,values:&[f64],types:&[NumericType])->Option<f64>{
        self.kind(types)?;if values.len()!=types.len(){return None;}
        if matches!(self,Self::SiblingIndex|Self::SiblingCount){return None;}
        if values.iter().any(|value|value.is_nan()){return Some(f64::NAN);}
        let first=*values.first()?;let radians=if types[0]==NumericType::from_unit(NumericUnit::Deg){first*core::f64::consts::PI/180.0}else{first};
        let degrees=180.0/core::f64::consts::PI;
        let value=match self {
            Self::SiblingIndex|Self::SiblingCount=>return None,
            Self::Progress{clamp}=>{
                let start=values[1];let end=values[2];
                let result=if start==end {
                    if clamp||first==start{0.0}else if first<start{f64::NEG_INFINITY}else{f64::INFINITY}
                }else{(first-start)/(end-start)};
                if clamp{result.clamp(0.0,1.0)}else{result}
            },
            Self::Sin=>libm::sin(radians),Self::Cos=>libm::cos(radians),Self::Tan=>libm::tan(radians),
            Self::Asin=>libm::asin(first)*degrees,Self::Acos=>libm::acos(first)*degrees,Self::Atan=>libm::atan(first)*degrees,
            Self::Atan2=>libm::atan2(first,values[1])*degrees,Self::Pow=>if first.abs()==1.0&&values[1].is_infinite(){f64::NAN}else{libm::pow(first,values[1])},Self::Sqrt=>libm::sqrt(first),
            Self::Hypot=>values.iter().fold(0.0,|sum,value|libm::hypot(sum,*value)),
            Self::Mod=>{
                let step=values[1];
                if step==0.0||first.is_infinite()||step.is_infinite()&&first.is_sign_negative()!=step.is_sign_negative(){f64::NAN}
                else if step.is_infinite(){first}
                else {let remainder=libm::fmod(first,step);if remainder==0.0{libm::copysign(0.0,step)}else if remainder.is_sign_negative()!=step.is_sign_negative(){remainder+step}else{remainder}}
            },
            Self::Rem=>{let step=values[1];if step==0.0||first.is_infinite(){f64::NAN}else if step.is_infinite(){first}else{libm::fmod(first,step)}},
            Self::Round(strategy)=>{
                let step=values.get(1).copied().unwrap_or(1.0).abs();
                if step==0.0||first.is_infinite()&&step.is_infinite(){return Some(f64::NAN);}
                if first.is_infinite(){return Some(first);}
                if step.is_infinite(){return Some(match strategy {
                    RoundingStrategy::Nearest|RoundingStrategy::ToZero=>libm::copysign(0.0,first),
                    RoundingStrategy::Up=>if first>0.0{f64::INFINITY}else{libm::copysign(0.0,first)},
                    RoundingStrategy::Down=>if first<0.0{f64::NEG_INFINITY}else{libm::copysign(0.0,first)},
                });}
                let ratio=first/step;
                // Exact multiples retain their value and signed zero.
                if libm::fmod(first,step)==0.0||!ratio.is_finite(){first}else{
                    let multiple=match strategy {RoundingStrategy::Nearest=>libm::floor(ratio+0.5),RoundingStrategy::Up=>libm::ceil(ratio),RoundingStrategy::Down=>libm::floor(ratio),RoundingStrategy::ToZero=>libm::trunc(ratio)};
                    if multiple==0.0{libm::copysign(0.0,first)}else{multiple*step}
                }
            },
            Self::Log=>libm::log(first)/values.get(1).map_or(1.0,|base|libm::log(*base)),Self::Exp=>libm::exp(first),Self::Abs=>first.abs(),
        };Some(value)
    }
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
    fn identifier(&mut self,_name:&str)->Option<Self::Value> {None}
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
    fn function(&mut self,_function:MathFunction,_values:Vec<Self::Value>,_types:Vec<NumericType>)->Option<Self::Value>{None}
}

impl NumericExpression {
    /// Heap storage retained by this bounded expression, excluding its root.
    /// Overflow conservatively defeats callers' bounded admission checks.
    pub fn retained_bytes(&self) -> usize {
        self.checked_retained_bytes().unwrap_or(usize::MAX)
    }
    pub fn checked_retained_bytes(&self) -> Option<usize> {
        let boxed=|value:&Self| core::mem::size_of::<Self>().checked_add(value.checked_retained_bytes()?);
        match self {
            Self::Value(_)|Self::Identifier(_)=>Some(0),
            Self::Calc(value)|Self::Negate(value)|Self::Invert(value)|Self::Sign(value)=>boxed(value),
            Self::Clamp(lower,value,upper)=>boxed(lower)?.checked_add(boxed(value)?)?.checked_add(boxed(upper)?),
            Self::Sum(values)|Self::Product(values)|Self::Min(values)|Self::Max(values)|Self::Function(_,values)=>{
                let mut bytes=values.capacity().checked_mul(core::mem::size_of::<Self>())?;
                for value in values {bytes=bytes.checked_add(value.checked_retained_bytes()?)?;}
                Some(bytes)
            }
        }
    }
    /// Resolve leaves in place while preserving this canonical bounded tree.
    /// The caller supplies computed unit policy; no additional expression parse
    /// or temporary tree is needed for context-dependent image values.
    /// Resolve a percentage-as-number property context without altering
    /// already dimensionless subtrees (such as progress()'s dimensional inputs).
    /// A mixed sum is typed only after its context-dependent percentages resolve.
    pub fn percentages_as_numbers(&mut self)->Option<()>{
        if self.numeric_type()==Some(NumericType::default()){return Some(());}
        match self {
            Self::Value(value)=>{if value.unit==NumericUnit::Percent{value.value/=100.0;value.unit=NumericUnit::Number;}},
            Self::Identifier(_)=>{},
            Self::Calc(value)|Self::Negate(value)|Self::Invert(value)|Self::Sign(value)=>value.percentages_as_numbers()?,
            Self::Clamp(a,b,c)=>{a.percentages_as_numbers()?;b.percentages_as_numbers()?;c.percentages_as_numbers()?;},
            Self::Function(MathFunction::Progress{..},_)=>{},
            Self::Sum(values)|Self::Product(values)|Self::Min(values)|Self::Max(values)|Self::Function(_,values)=>{for value in values{value.percentages_as_numbers()?;}},
        }
        Some(())
    }
    pub fn map_numeric_values(&mut self, mut map: impl FnMut(NumericValue) -> Option<NumericValue>) -> Option<()> {
        fn visit(expression: &mut NumericExpression, map: &mut impl FnMut(NumericValue) -> Option<NumericValue>) -> Option<()> {
            match expression {
                NumericExpression::Value(value) => *value = map(*value)?,
                NumericExpression::Identifier(_)=>{},
                NumericExpression::Calc(value) | NumericExpression::Negate(value) |
                NumericExpression::Invert(value) | NumericExpression::Sign(value) => visit(value, map)?,
                NumericExpression::Sum(values) | NumericExpression::Product(values) |
                NumericExpression::Min(values) | NumericExpression::Max(values) | NumericExpression::Function(_,values) => {
                    for value in values { visit(value, map)?; }
                }
                NumericExpression::Clamp(lower, value, upper) => {visit(lower,map)?;visit(value,map)?;visit(upper,map)?;}
            }
            Some(())
        }
        visit(self, &mut map)
    }

    /// Project a computed percentage/dimension mix after value combination.
    /// CSS Values' mix computation differs from authored calculation-tree
    /// simplification: a zero dimension becomes a percentage, and a zero
    /// percentage becomes a dimension. Do not use this on authored math trees.
    pub fn computed_percentage_dimension_mix(&self)->Option<NumericValue> {
        let Self::Sum(values)=self else{return self.single_numeric_value();};
        let mut percentage=None;let mut dimension=None;
        for expression in values {
            let value=expression.single_numeric_value()?;
            if !value.value.is_finite(){return None;}
            if value.unit==NumericUnit::Percent {
                percentage=Some(percentage.unwrap_or(0.0)+value.value);
            }else if value.unit.dimension()!=NumericDimension::Number {
                let (unit,factor)=value.unit.canonical_unit_and_factor()?;
                let (previous,old_unit)=dimension.unwrap_or((0.0,unit));
                if old_unit!=unit{return None;}
                dimension=Some((previous+value.value*factor,unit));
            }else{return None;}
        }
        let percentage=percentage?;let (dimension,unit)=dimension?;
        if !percentage.is_finite()||!dimension.is_finite(){return None;}
        if percentage==0.0{Some(NumericValue{value:dimension,unit})}
        else if dimension==0.0{Some(NumericValue{value:percentage,unit:NumericUnit::Percent})}
        else{None}
    }

    /// Inspect units without reparsing or exposing a second expression grammar.
    pub fn contains_unit(&self, predicate:fn(NumericUnit)->bool) -> bool {
        match self {
            Self::Value(value) => predicate(value.unit),
            Self::Identifier(_)=>false,
            Self::Calc(value) | Self::Negate(value) | Self::Invert(value) | Self::Sign(value) => value.contains_unit(predicate),
            Self::Sum(values) | Self::Product(values) | Self::Min(values) | Self::Max(values) | Self::Function(_,values) => values.iter().any(|value| value.contains_unit(predicate)),
            Self::Clamp(lower, value, upper) => lower.contains_unit(predicate) || value.contains_unit(predicate) || upper.contains_unit(predicate),
        }
    }
    /// Evaluate with caller-supplied unit conversion and type rules.
    pub fn evaluate<C: NumericExpressionContext>(&self, context: &mut C) -> Option<C::Value> {
        match self {
            Self::Value(value) => context.unit(*value),
            Self::Identifier(name)=>context.identifier(name),
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
            Self::Function(function,arguments)=>{
                let mut values=Vec::new();let mut types=Vec::new();
                values.try_reserve_exact(arguments.len()).ok()?;types.try_reserve_exact(arguments.len()).ok()?;
                for argument in arguments {types.push(argument.numeric_type()?);values.push(argument.evaluate(context)?);}
                context.function(*function,values,types)
            }
            Self::Sign(value) => {
                let value = value.evaluate(context)?;
                context.sign(value)
            }
        }
    }

    /// Numeric tree-counting functions require an actual owner. Typed OM
    /// cannot reify them as a context-free CSSNumericValue.
    pub fn contains_tree_functions(&self)->bool {
        match self {
            Self::Function(MathFunction::SiblingIndex|MathFunction::SiblingCount,_)=>true,
            Self::Calc(value)|Self::Negate(value)|Self::Invert(value)|Self::Sign(value)=>value.contains_tree_functions(),
            Self::Sum(values)|Self::Product(values)|Self::Min(values)|Self::Max(values)|Self::Function(_,values)=>values.iter().any(Self::contains_tree_functions),
            Self::Clamp(lower,value,upper)=>lower.contains_tree_functions()||value.contains_tree_functions()||upper.contains_tree_functions(),
            _=>false,
        }
    }

    pub fn contains_sign(&self) -> bool {
        match self {
            Self::Sign(_) => true,
            Self::Calc(value) | Self::Negate(value) | Self::Invert(value) => value.contains_sign(),
            Self::Sum(values) | Self::Product(values) | Self::Min(values) | Self::Max(values) | Self::Function(_,values) => {
                values.iter().any(Self::contains_sign)
            }
            Self::Clamp(lower, value, upper) => {
                lower.contains_sign() || value.contains_sign() || upper.contains_sign()
            }
            Self::Value(_)|Self::Identifier(_) => false,
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
                NumericExpression::Identifier(name)=>return CHANNEL_KEYWORDS.contains(name),
                NumericExpression::Calc(value)
                | NumericExpression::Negate(value)
                | NumericExpression::Invert(value)
                | NumericExpression::Sign(value) => {
                    return measure(value, depth + 1, count);
                }
                NumericExpression::Sum(values)
                | NumericExpression::Product(values)
                | NumericExpression::Min(values)
                | NumericExpression::Max(values) | NumericExpression::Function(_,values) => values,
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
                NumericExpression::Identifier(name)=>append_text(out,name),
                NumericExpression::Value(value) => {
                    if !value.value.is_finite() {
                        let constant=if value.value.is_nan(){"NaN"}else if value.value.is_sign_negative(){"-infinity"}else{"infinity"};
                        let bare=if value.unit==NumericUnit::Number{constant.into()}else{alloc::format!("{constant} * 1{}",value.unit.as_str())};
                        return append_text(out,&if paren_less{bare}else{alloc::format!("calc({bare})")});
                    }
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
                        } else if let NumericExpression::Value(number)=value {
                            if number.value<0.0 {
                                append_text(out," - ")?;
                                append_text(out,&serialize_numeric_value(-number.value,number.unit))?;
                            }else{
                                append_text(out," + ")?;
                                append(out,value,true,false)?;
                            }                        } else {
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
                NumericExpression::Function(function,values)=>{
                    append_text(out,function.name())?;append_text(out,"(")?;
                    if let MathFunction::Round(strategy)=function {append_text(out,strategy.name())?;append_text(out,", ")?;}
                    if let MathFunction::Progress{clamp:false}=function{append_text(out,"no-clamp ")?;}
                    for (at,value) in values.iter().enumerate(){if at!=0{append_text(out,", ")?;}append(out,value,false,false)?;}
                    append_text(out,")")
                }
                NumericExpression::Sign(value) => {
                    append_text(out, "sign(")?;
                    append(out, value, true, true)?;
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

    /// Specified CSS math keeps its calculation boundary after source-only
    /// simplification. Consume the existing bounded AST without another parse
    /// or a temporary clone; computed serializers still emit numeric leaves.
    pub fn serialize_specified(mut self)->Option<String> {
        let calculation=!matches!(&self,Self::Value(_)|Self::Identifier(_));
        self.simplify_absolute_units();
        if calculation&&matches!(&self,Self::Value(_)){self=Self::Calc(Box::new(self));}
        self.serialize()
    }

    pub fn numeric_type(&self) -> Option<NumericType> {
        match self {
            Self::Value(value) => Some(NumericType::from_unit(value.unit)),
            Self::Identifier(_)=>Some(NumericType::default()),
            Self::Calc(value) | Self::Negate(value) => value.numeric_type(),
            Self::Invert(value) => Some(value.numeric_type()?.invert()),
            Self::Function(function,values)=>{
                let mut types=Vec::new();types.try_reserve_exact(values.len()).ok()?;
                for value in values {types.push(value.numeric_type()?);}
                function.kind(&types)
            }
            Self::Sign(value) => {
                value.numeric_type()?;
                Some(NumericType::default())
            },
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

    /// A simplified math expression whose remaining operator wrappers carry
    /// one primitive value. Computed CSS serializes that primitive directly;
    /// specified Typed OM retains its explicit calc() boundary.
    pub fn single_numeric_value(&self) -> Option<NumericValue> {
        match self {
            Self::Value(value)=>Some(*value),
            Self::Calc(value)=>value.single_numeric_value(),
            Self::Sum(values)|Self::Product(values) if values.len()==1=>values[0].single_numeric_value(),
            _=>None,
        }
    }

    /// Simplify the supported calculation operators using information already
    /// present in the tree. Contextual units are resolved by the caller first;
    /// unresolved percentage bases never become comparison or sign values.
    pub fn simplify_absolute_units(&mut self) {
        fn canonical(expression:&NumericExpression)->Option<NumericValue> {
            let value=expression.single_numeric_value()?;
            let(unit,factor)=value.unit.canonical_unit_and_factor()?;
            let value=value.value*factor;
            Some(NumericValue{value,unit})
        }
        fn comparable(expression:&NumericExpression)->Option<NumericValue> {
            canonical(expression).filter(|value|value.unit!=NumericUnit::Percent)
        }
        match self {
            Self::Value(_)|Self::Identifier(_)=>{},
            Self::Calc(value)=>value.simplify_absolute_units(),
            Self::Negate(value)=>{
                value.simplify_absolute_units();
                if let Some(mut value)=value.single_numeric_value(){value.value=-value.value;*self=Self::Value(value);}
            },
            Self::Invert(value)=>{
                value.simplify_absolute_units();
                if let Some(value)=value.single_numeric_value().filter(|value|value.unit==NumericUnit::Number){
                    let reciprocal=1.0/value.value;
                    if reciprocal.is_finite(){*self=Self::Value(NumericValue{value:reciprocal,unit:NumericUnit::Number});}
                }
            },
            Self::Function(function,values)=>{
                for value in values.iter_mut(){value.simplify_absolute_units();}
                let all_percent=matches!(function,MathFunction::Progress{..})&&values.iter().all(|value|value.numeric_type()==Some(NumericType::from_unit(NumericUnit::Percent)));
                let Some(arguments):Option<Vec<_>>=values.iter().map(|value|if all_percent{
                    if let Some(value)=canonical(value){return Some(value);}
                    let mut expression=value.clone();
                    expression.map_numeric_values(|mut value|{if value.unit==NumericUnit::Percent{value.unit=NumericUnit::Number;}Some(value)})?;
                    expression.simplify_absolute_units();let mut value=expression.single_numeric_value()?;
                    value.unit=NumericUnit::Percent;Some(value)
                }else{comparable(value)}).collect() else{return;};
                let numbers:Vec<_>=arguments.iter().map(|value|value.value).collect();
                let types:Vec<_>=arguments.iter().map(|value|NumericType::from_unit(value.unit)).collect();
                let Some(kind)=function.kind(&types) else{return;};
                let unit=[NumericUnit::Number,NumericUnit::Px,NumericUnit::Percent,NumericUnit::Deg,NumericUnit::S,NumericUnit::Hz,NumericUnit::Dppx,NumericUnit::Fr].into_iter().find(|unit|NumericType::from_unit(*unit)==kind);
                if let(Some(unit),Some(value))=(unit,function.evaluate(&numbers,&types)){
                    let value=Self::Value(NumericValue{value,unit});
                    *self=if matches!(function,MathFunction::Progress{..}){Self::Calc(Box::new(value))}else{value};
                }
            },
            Self::Sign(value)=>{
                value.simplify_absolute_units();
                if let Some(value)=comparable(value){
                    // sign() preserves signed zero; f64::signum() maps zero to
                    // +/-1 and therefore cannot implement the CSS operation.
                    let sign=if value.value.is_nan(){f64::NAN}else if value.value==0.0{value.value}else if value.value>0.0{1.0}else{-1.0};
                    *self=Self::Value(NumericValue{value:sign,unit:NumericUnit::Number});
                }
            },
            Self::Clamp(lower,value,upper)=>{
                lower.simplify_absolute_units();value.simplify_absolute_units();upper.simplify_absolute_units();
                if let (Some(lower),Some(value),Some(upper))=(comparable(lower),comparable(value),comparable(upper)) {
                    if lower.unit==value.unit&&value.unit==upper.unit {
                        *self=Self::Value(NumericValue{value:numeric_maximum(lower.value,numeric_minimum(value.value,upper.value)),unit:value.unit});
                    }
                }
            },
            Self::Min(_) | Self::Max(_)=>{
                let minimum=matches!(self,Self::Min(_));
                let(Self::Min(values)|Self::Max(values))=self else{unreachable!()};
                for value in values.iter_mut(){value.simplify_absolute_units();}
                let mut index=0;
                while index<values.len(){
                    if let Some(mut combined)=comparable(&values[index]){
                        let mut other=index+1;
                        while other<values.len(){
                            if let Some(value)=comparable(&values[other]).filter(|value|value.unit==combined.unit){
                                combined.value=if minimum{numeric_minimum(combined.value,value.value)}else{numeric_maximum(combined.value,value.value)};
                                values.remove(other);
                            }else{other+=1;}
                        }
                        values[index]=Self::Value(combined);
                    }
                    index+=1;
                }
                if values.len()==1{*self=values.pop().unwrap();}
            },
            Self::Product(values)=>{
                // Cancel exact source and unit-conversion factors before rounding
                // either quotient. Never use an approximate equality or epsilon.
                fn product_factors(expression:&NumericExpression,inverse:bool,numerators:&mut Vec<f64>,denominators:&mut Vec<f64>,powers:&mut [i32;7])->Option<()> {
                    match expression {
                        NumericExpression::Calc(value)=>product_factors(value,inverse,numerators,denominators,powers),
                        NumericExpression::Invert(value)=>product_factors(value,!inverse,numerators,denominators,powers),
                        NumericExpression::Product(values)=>{for value in values{product_factors(value,inverse,numerators,denominators,powers)?;}Some(())},
                        NumericExpression::Value(value)=>{
                            let(unit,numerator,denominator)=value.unit.canonical_unit_and_ratio()?;
                            if !value.value.is_finite()||value.value==0.0{return None;}
                            let units=[NumericUnit::Percent,NumericUnit::Px,NumericUnit::Deg,NumericUnit::S,NumericUnit::Hz,NumericUnit::Dppx,NumericUnit::Fr];
                            if unit!=NumericUnit::Number {powers[units.iter().position(|candidate|*candidate==unit)?]+=if inverse{-1}else{1};}
                            let(top,bottom)=if inverse{(denominators,numerators)}else{(numerators,denominators)};
                            if value.value!=1.0{top.push(value.value);}if numerator!=1.0{top.push(numerator);}if denominator!=1.0{bottom.push(denominator);}Some(())
                        },
                        _=>None,
                    }
                }
                // Ordinary px/scalar products keep the existing allocation-free
                // simplification path. Count only factors this rare path stores.
                fn factor_budget(expression:&NumericExpression)->Option<(usize,bool)> {
                    match expression {
                        NumericExpression::Calc(value)|NumericExpression::Invert(value)=>factor_budget(value),
                        NumericExpression::Product(values)=>{let(mut count,mut needed)=(0usize,false);for value in values{let(next,ratio)=factor_budget(value)?;count=count.checked_add(next)?;needed|=ratio;}Some((count,needed))},
                        NumericExpression::Value(value)=>{let(_,numerator,denominator)=value.unit.canonical_unit_and_ratio()?;Some((usize::from(value.value!=1.0)+usize::from(numerator!=1.0)+usize::from(denominator!=1.0),denominator!=1.0))},
                        _=>None,
                    }
                }
                let budget=values.iter().try_fold((0usize,false),|(count,needed),value|{let(next,ratio)=factor_budget(value)?;Some((count.checked_add(next)?,needed||ratio))});
                if let Some((count,true))=budget.filter(|(count,_)|*count<=MAX_NUMERIC_EXPRESSION_NODES*3) {
                let mut numerators=Vec::new();let mut denominators=Vec::new();let mut powers=[0;7];
                if numerators.try_reserve_exact(count).is_ok()&&denominators.try_reserve_exact(count).is_ok()&&values.iter().all(|value|product_factors(value,false,&mut numerators,&mut denominators,&mut powers).is_some()) {
                    for numerator in &mut numerators {if let Some(denominator)=denominators.iter_mut().find(|denominator|**denominator==*numerator){*numerator=1.0;*denominator=1.0;}}
                    let product=numerators.iter().product::<f64>()/denominators.iter().product::<f64>();
                    let units=[NumericUnit::Percent,NumericUnit::Px,NumericUnit::Deg,NumericUnit::S,NumericUnit::Hz,NumericUnit::Dppx,NumericUnit::Fr];
                    let mut unit=NumericUnit::Number;let mut simple=true;
                    for(index,power)in powers.iter().enumerate(){if *power!=0{if *power!=1||unit!=NumericUnit::Number{simple=false;break;}unit=units[index];}}
                    if simple&&product.is_finite(){*self=Self::Value(NumericValue{value:product,unit});return;}
                }
                }
                for value in values.iter_mut(){value.simplify_absolute_units();}
                values.sort_by_key(|value|value.single_numeric_value().is_none());
                let units=[NumericUnit::Percent,NumericUnit::Px,NumericUnit::Deg,NumericUnit::S,NumericUnit::Hz,NumericUnit::Dppx];
                let mut powers=[0i32;6];let mut product=1.0;
                for expression in values.iter(){
                    let (value,inverse)=match expression{Self::Invert(value)=>(canonical(value),true),_=>(canonical(expression),false)};
                    let Some(value)=value else{return;};
                    if value.unit!=NumericUnit::Number{
                        let Some(index)=units.iter().position(|unit|*unit==value.unit)else{return;};
                        powers[index]+=if inverse{-1}else{1};
                    }
                    product*=if inverse{1.0/value.value}else{value.value};
                }
                let mut unit=NumericUnit::Number;
                for(index,power)in powers.iter().enumerate(){
                    if *power!=0{
                        if *power!=1||unit!=NumericUnit::Number{return;}
                        unit=units[index];
                    }
                }
                *self=Self::Value(NumericValue{value:product,unit});
            },
            Self::Sum(values)=>{
                for value in values.iter_mut(){value.simplify_absolute_units();}
                // Combine the same source unit before conversion. Distributing
                // a non-exact scale across a subtraction loses identities such
                // as (100dpi - 4dpi) / 96dpi = 1 without any CSS rounding policy.
                let mut index=0;
                while index<values.len(){
                    if let Some(mut combined)=values[index].single_numeric_value(){
                        let mut other=index+1;
                        while other<values.len(){
                            if let Some(value)=values[other].single_numeric_value().filter(|value|value.unit==combined.unit){
                                let sum=combined.value+value.value;
                                combined.value=sum;values.remove(other);continue;
                            }
                            other+=1;
                        }
                        values[index]=Self::Value(combined);
                    }
                    index+=1;
                }
                // Fold identical units independently: retaining a zero % term
                // is required even when every other term is an absolute length.
                let mut index=0;
                while index<values.len(){
                    if let Some(value)=values[index].single_numeric_value(){
                        let (unit,factor)=value.unit.canonical_unit_and_factor().unwrap_or((value.unit,1.0));
                        let mut combined=value.value*factor;
                        let mut other=index+1;
                        while other<values.len(){
                            if let Some(value)=values[other].single_numeric_value(){
                                let (candidate,factor)=value.unit.canonical_unit_and_factor().unwrap_or((value.unit,1.0));
                                if candidate==unit{combined+=value.value*factor;values.remove(other);continue;}
                            }
                            other+=1;
                        }
                        values[index]=Self::Value(NumericValue{value:combined,unit});
                    }
                    index+=1;
                }
                values.sort_by_key(|value|match value.single_numeric_value(){
                    Some(value)=>match value.unit{NumericUnit::Number=>(0,""),NumericUnit::Percent=>(1,""),unit=>(2,unit.as_str())},
                    None=>(3,""),
                });
                if values.len()==1{*self=values.pop().unwrap();}
            },
        }
    }

}

/// Builder interface for the shared math grammar. A renderer can fold into a
/// small scalar accumulator; Typed OM uses the same callbacks to build a tree.
pub trait NumericExpressionBuilder {
    type Expr;
    type Accumulator;

    fn value(&mut self, value: NumericValue) -> Option<Self::Expr>;
    /// Context-defined numeric identifiers, such as relative color channels.
    /// Ordinary numeric properties retain their strict no-identifier grammar.
    fn identifier(&mut self, _name:&str)->Option<Self::Expr> {None}
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
    fn function(&mut self,_function:MathFunction,_values:Vec<Self::Expr>)->Option<Self::Expr>{None}
}

/// Operation-local named numeric values; no AST or source text is retained.
/// The same expression grammar, unit checking and resource bounds apply.
pub struct NamedNumericBuilder<'a,B> {
    pub inner:&'a mut B,
    pub values:&'a [(&'a str,NumericValue)],
}
impl<B:NumericExpressionBuilder> NumericExpressionBuilder for NamedNumericBuilder<'_,B> {
    type Expr=B::Expr;
    type Accumulator=B::Accumulator;
    fn identifier(&mut self,name:&str)->Option<Self::Expr> {
        let value=self.values.iter().find(|(key,_)|name.eq_ignore_ascii_case(key))?.1;
        self.inner.value(value)
    }
    fn value(&mut self, value: NumericValue) -> Option<Self::Expr> {self.inner.value(value)}
    fn begin_sum(&mut self, first: Self::Expr) -> Option<Self::Accumulator> {self.inner.begin_sum(first)}
    fn push_sum(
        &mut self,
        values: &mut Self::Accumulator,
        next: Self::Expr,
        subtract: bool,
    ) -> Option<()> {self.inner.push_sum(values, next, subtract)}
    fn finish_sum(&mut self, values: Self::Accumulator, operated: bool) -> Option<Self::Expr> {self.inner.finish_sum(values, operated)}
    fn begin_product(&mut self, first: Self::Expr) -> Option<Self::Accumulator> {self.inner.begin_product(first)}
    fn push_product(
        &mut self,
        values: &mut Self::Accumulator,
        next: Self::Expr,
        divide: bool,
    ) -> Option<()> {self.inner.push_product(values, next, divide)}
    fn finish_product(&mut self, values: Self::Accumulator, operated: bool) -> Option<Self::Expr> {self.inner.finish_product(values, operated)}
    fn begin_min(&mut self, first: Self::Expr) -> Option<Self::Accumulator> {self.inner.begin_min(first)}
    fn push_min(&mut self, values: &mut Self::Accumulator, next: Self::Expr) -> Option<()> {self.inner.push_min(values, next)}
    fn finish_min(&mut self, values: Self::Accumulator) -> Option<Self::Expr> {self.inner.finish_min(values)}
    fn begin_max(&mut self, first: Self::Expr) -> Option<Self::Accumulator> {self.inner.begin_max(first)}
    fn push_max(&mut self, values: &mut Self::Accumulator, next: Self::Expr) -> Option<()> {self.inner.push_max(values, next)}
    fn finish_max(&mut self, values: Self::Accumulator) -> Option<Self::Expr> {self.inner.finish_max(values)}
    fn calc(&mut self, value: Self::Expr) -> Option<Self::Expr> {self.inner.calc(value)}
    fn clamp(
        &mut self,
        lower: Self::Expr,
        value: Self::Expr,
        upper: Self::Expr,
    ) -> Option<Self::Expr> {self.inner.clamp(lower, value, upper)}
    fn negate(&mut self, value: Self::Expr) -> Option<Self::Expr> {self.inner.negate(value)}
    fn invert(&mut self, value: Self::Expr) -> Option<Self::Expr> {self.inner.invert(value)}
    fn begin_sign_input(&mut self) {self.inner.begin_sign_input()}
    fn end_sign_input(&mut self) {self.inner.end_sign_input()}
    fn sign(&mut self, value: Self::Expr) -> Option<Self::Expr> {self.inner.sign(value)}
    fn function(&mut self,function:MathFunction,values:Vec<Self::Expr>)->Option<Self::Expr>{self.inner.function(function,values)}
}

const CHANNEL_KEYWORDS:&[&str]=&["r","g","b","h","s","l","w","a","c","x","y","z","alpha"];
struct NamedExpressionTreeBuilder<'a> {inner:NumericExpressionTreeBuilder,names:&'a[&'static str]}
impl NumericExpressionBuilder for NamedExpressionTreeBuilder<'_> {
    type Expr=NumericExpression;
    type Accumulator=Vec<NumericExpression>;
    fn identifier(&mut self,name:&str)->Option<Self::Expr> {
        let name=*self.names.iter().find(|key|name.eq_ignore_ascii_case(key))?;
        Some(NumericExpression::Identifier(name))
    }
    fn value(&mut self, value: NumericValue) -> Option<Self::Expr> {self.inner.value(value)}
    fn begin_sum(&mut self, first: Self::Expr) -> Option<Self::Accumulator> {self.inner.begin_sum(first)}
    fn push_sum(
        &mut self,
        values: &mut Self::Accumulator,
        next: Self::Expr,
        subtract: bool,
    ) -> Option<()> {self.inner.push_sum(values, next, subtract)}
    fn finish_sum(&mut self, values: Self::Accumulator, operated: bool) -> Option<Self::Expr> {self.inner.finish_sum(values, operated)}
    fn begin_product(&mut self, first: Self::Expr) -> Option<Self::Accumulator> {self.inner.begin_product(first)}
    fn push_product(
        &mut self,
        values: &mut Self::Accumulator,
        next: Self::Expr,
        divide: bool,
    ) -> Option<()> {self.inner.push_product(values, next, divide)}
    fn finish_product(&mut self, values: Self::Accumulator, operated: bool) -> Option<Self::Expr> {self.inner.finish_product(values, operated)}
    fn begin_min(&mut self, first: Self::Expr) -> Option<Self::Accumulator> {self.inner.begin_min(first)}
    fn push_min(&mut self, values: &mut Self::Accumulator, next: Self::Expr) -> Option<()> {self.inner.push_min(values, next)}
    fn finish_min(&mut self, values: Self::Accumulator) -> Option<Self::Expr> {self.inner.finish_min(values)}
    fn begin_max(&mut self, first: Self::Expr) -> Option<Self::Accumulator> {self.inner.begin_max(first)}
    fn push_max(&mut self, values: &mut Self::Accumulator, next: Self::Expr) -> Option<()> {self.inner.push_max(values, next)}
    fn finish_max(&mut self, values: Self::Accumulator) -> Option<Self::Expr> {self.inner.finish_max(values)}
    fn calc(&mut self, value: Self::Expr) -> Option<Self::Expr> {self.inner.calc(value)}
    fn clamp(
        &mut self,
        lower: Self::Expr,
        value: Self::Expr,
        upper: Self::Expr,
    ) -> Option<Self::Expr> {self.inner.clamp(lower, value, upper)}
    fn negate(&mut self, value: Self::Expr) -> Option<Self::Expr> {self.inner.negate(value)}
    fn invert(&mut self, value: Self::Expr) -> Option<Self::Expr> {self.inner.invert(value)}
    fn begin_sign_input(&mut self) {self.inner.begin_sign_input()}
    fn end_sign_input(&mut self) {self.inner.end_sign_input()}
    fn sign(&mut self, value: Self::Expr) -> Option<Self::Expr> {self.inner.sign(value)}
    fn function(&mut self,function:MathFunction,values:Vec<Self::Expr>)->Option<Self::Expr>{self.inner.function(function,values)}
}
/// Canonical symbolic channel math; ordinary expression parsing remains strict.
pub(super) fn parse_channel_expression(input:&str,names:&[&'static str])->Option<NumericExpression> {
    if names.iter().any(|name|!CHANNEL_KEYWORDS.contains(name)) {return None;}
    parse_numeric_expression_with(input,&mut NamedExpressionTreeBuilder{inner:NumericExpressionTreeBuilder,names})
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

// The dependency gate and length grammar share the parser's supported function
// dispatch. Unsupported functions are not silently admitted as computable math.
const NUMERIC_MATH_FUNCTIONS: [(&str,u8);24] = [
    ("calc(",0),("min(",1),("max(",2),("clamp(",3),("sign(",4),
    ("sin(",5),("cos(",6),("tan(",7),("asin(",8),("acos(",9),("atan(",10),("atan2(",11),("pow(",12),("sqrt(",13),("hypot(",14),("log(",15),("exp(",16),("abs(",17),("mod(",18),("rem(",19),("round(",20),("progress(",21),("sibling-index(",22),("sibling-count(",23),
];
pub(super) fn is_numeric_math_function(name:&str)->bool {
    NUMERIC_MATH_FUNCTIONS.iter().any(|(function,_)|
        name.eq_ignore_ascii_case(&function[..function.len()-1]))
}

struct NumericExpressionParser<'a, 'b, B> {
    input: &'a str,
    position: usize,
    nodes: usize,
    builder: &'b mut B,
}

impl<B: NumericExpressionBuilder> NumericExpressionParser<'_, '_, B> {
    fn space(&mut self) {
        loop {
            while self.input.as_bytes().get(self.position).is_some_and(|byte| matches!(byte, b'\t' | b'\n' | b'\x0c' | b'\r' | b' ')) { self.position += 1; }
            if !self.input[self.position..].starts_with("/*") { break; }
            let mut cursor = super::syntax::Cursor::new(self.input, self.position).expect("bounded numeric input");
            let Some(token) = cursor.next() else { break; };
            self.position = token.end;
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
        // Decode only escaped function names; ordinary numeric parsing stays borrowed.
        let escaped = rest.find('(').filter(|end| rest[..*end].contains('\\')).and_then(|end| {
            let mut consumed = 0;
            let name = super::consume_selector_identifier(&rest[..end], &mut consumed)?;
            (consumed == end).then_some((name, end + 1))
        });
        for (name, kind) in NUMERIC_MATH_FUNCTIONS {
            let escaped_length = escaped.as_ref().filter(|(decoded,_)| decoded.eq_ignore_ascii_case(&name[..name.len()-1])).map(|(_,length)| *length);
            if rest.get(..name.len()).is_some_and(|prefix| prefix.eq_ignore_ascii_case(name)) || escaped_length.is_some() {
                self.position += escaped_length.unwrap_or(name.len());
                if kind>=5 {
                    let mut function=match kind {5=>MathFunction::Sin,6=>MathFunction::Cos,7=>MathFunction::Tan,8=>MathFunction::Asin,9=>MathFunction::Acos,10=>MathFunction::Atan,11=>MathFunction::Atan2,12=>MathFunction::Pow,13=>MathFunction::Sqrt,14=>MathFunction::Hypot,15=>MathFunction::Log,16=>MathFunction::Exp,17=>MathFunction::Abs,18=>MathFunction::Mod,19=>MathFunction::Rem,20=>MathFunction::Round(RoundingStrategy::Nearest),21=>MathFunction::Progress{clamp:true},22=>MathFunction::SiblingIndex,23=>MathFunction::SiblingCount,_=>return None};
                    if kind==22||kind==23 {
                        self.space();if self.input.as_bytes().get(self.position)!=Some(&b')'){return None;}
                        self.position+=1;self.count_node()?;return self.builder.function(function,Vec::new());
                    }
                    if kind==20 {
                        self.space();let start=self.position;let mut consumed=0;
                        if let Some(name)=super::consume_selector_identifier(&self.input[start..],&mut consumed){
                            let strategy=match name.to_ascii_lowercase().as_str(){"nearest"=>Some(RoundingStrategy::Nearest),"up"=>Some(RoundingStrategy::Up),"down"=>Some(RoundingStrategy::Down),"to-zero"=>Some(RoundingStrategy::ToZero),_=>None};
                            if let Some(strategy)=strategy {
                                self.position+=consumed;self.space();
                                if self.input.as_bytes().get(self.position)!=Some(&b','){return None;}
                                self.position+=1;function=MathFunction::Round(strategy);
                            }
                        }
                    }
                    if kind==21 {
                        self.space();let start=self.position;let mut consumed=0;
                        if super::consume_selector_identifier(&self.input[start..],&mut consumed).is_some_and(|name|name.eq_ignore_ascii_case("no-clamp")) {
                            self.position+=consumed;self.space();function=MathFunction::Progress{clamp:false};
                        }
                    }
                    let mut values=Vec::new();
                    loop {
                        if values.len()>=MAX_NUMERIC_EXPRESSION_ARGS{return None;}
                        values.try_reserve(1).ok()?;values.push(self.sum(depth+1)?);self.space();
                        match self.input.as_bytes().get(self.position){Some(b',')=>self.position+=1,Some(b')')=>{self.position+=1;break;},_=>return None}
                    }
                    self.count_node()?;return self.builder.function(function,values);
                }
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
        if super::would_start_css_identifier(rest) {
            let mut consumed=0;
            let name=super::consume_selector_identifier(rest,&mut consumed)?;
            self.position=self.position.checked_add(consumed)?;
            self.count_node()?;
            if depth>0 {
                let constant=match name.to_ascii_lowercase().as_str(){"infinity"=>Some(f64::INFINITY),"-infinity"=>Some(f64::NEG_INFINITY),"nan"=>Some(f64::NAN),_=>None};
                if let Some(value)=constant{return self.builder.value(NumericValue{value,unit:NumericUnit::Number});}
            }
            return self.builder.identifier(&name);
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
    fn function(&mut self,function:MathFunction,values:Vec<Self::Expr>)->Option<Self::Expr>{Some(NumericExpression::Function(function,values))}
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
        let right = other.apply_percent_hint(hint)?;
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
            "x" => Some(Self::Dppx),
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
        match self.canonical_unit_and_ratio() {Some((unit,numerator,denominator))=>Some((unit,numerator/denominator)),None=>None}
    }

    /// Keep authored conversion factors separate until product cancellation.
    pub const fn canonical_unit_and_ratio(self) -> Option<(Self, f64, f64)> {
        match self {
            Self::Number => Some((Self::Number, 1.0, 1.0)),
            Self::Percent => Some((Self::Percent, 1.0, 1.0)),
            Self::Px => Some((Self::Px, 1.0, 1.0)),
            Self::In => Some((Self::Px, 96.0, 1.0)),
            Self::Cm => Some((Self::Px, 96.0, 2.54)),
            Self::Mm => Some((Self::Px, 96.0, 25.4)),
            Self::Q => Some((Self::Px, 96.0, 101.6)),
            Self::Pt => Some((Self::Px, 96.0, 72.0)),
            Self::Pc => Some((Self::Px, 16.0, 1.0)),
            Self::Deg => Some((Self::Deg, 1.0, 1.0)),
            Self::Grad => Some((Self::Deg, 0.9, 1.0)),
            Self::Rad => Some((Self::Deg, 180.0, core::f64::consts::PI)),
            Self::Turn => Some((Self::Deg, 360.0, 1.0)),
            Self::S => Some((Self::S, 1.0, 1.0)),
            Self::Ms => Some((Self::S, 0.001, 1.0)),
            Self::Hz => Some((Self::Hz, 1.0, 1.0)),
            Self::KHz => Some((Self::Hz, 1000.0, 1.0)),
            Self::Dpi => Some((Self::Dppx, 1.0, 96.0)),
            Self::Dpcm => Some((Self::Dppx, 2.54, 96.0)),
            Self::Dppx => Some((Self::Dppx, 1.0, 1.0)),
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
    let decoded = if bytes.get(position) == Some(&b'\\') {
        position = suffix_start;
        Some(super::consume_selector_identifier(input, &mut position)?)
    } else { None };
    let suffix = decoded.as_deref().unwrap_or(&input[suffix_start..position]);
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
    if !value.is_finite() {
        let constant=if value.is_nan(){"NaN"}else if value.is_sign_negative(){"-infinity"}else{"infinity"};
        return if unit==NumericUnit::Number{alloc::format!("calc({constant})")}else{alloc::format!("calc({constant} * 1{})",unit.as_str())};
    }
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
    #[test]
    fn specification_math_special_values_share_evaluation_and_simplification() {
        for(raw,expected)in [("round(up,1,infinity)",f64::INFINITY),("round(down,-1,infinity)",f64::NEG_INFINITY),("round(1,infinity)",0.0),("round(-1,infinity)",-0.0),("round(1e-300,1e300)",0.0),("mod(-1,3)",2.0),("mod(1,-3)",-2.0),("rem(-1,3)",-1.0),("mod(1,infinity)",1.0),("rem(-1,infinity)",-1.0)] {
            let mut expression=parse_numeric_expression(raw).unwrap();expression.simplify_absolute_units();
            let value=expression.single_numeric_value().unwrap_or_else(||panic!("{raw}: {expression:?}")).value;
            assert_eq!(value,expected,"{raw}");if expected==0.0{assert_eq!(value.is_sign_negative(),expected.is_sign_negative(),"{raw}");}
        }
        for raw in ["round(1,0)","round(infinity,infinity)","mod(-1,infinity)","mod(0,infinity * -1)","rem(infinity,1)","mod(1,0)","pow(NaN,0)","pow(1,infinity)","hypot(infinity,NaN)","min(NaN,1)","max(NaN,1)","clamp(0,NaN,1)","sign(NaN)"] {
            let mut expression=parse_numeric_expression(raw).unwrap();expression.simplify_absolute_units();
            assert!(expression.single_numeric_value().unwrap_or_else(||panic!("{raw}: {expression:?}")).value.is_nan(),"{raw}");
        }
        assert!(numeric_minimum(0.0,-0.0).is_sign_negative());
        assert!(!numeric_maximum(-0.0,0.0).is_sign_negative());
    }

    #[test]
    fn specification_specified_math_keeps_calculation_boundary() {
        for(raw,expected)in [("progress(0.5,0,1)","calc(0.5)"),("sin(0deg)","calc(0)"),("round(1.5)","calc(2)"),("calc(3 * 4)","calc(12)"),("progress(1em,0px,10px)","progress(1em, 0px, 10px)"),("2","2")] {
            assert_eq!(parse_numeric_expression(raw).unwrap().serialize_specified().as_deref(),Some(expected),"{raw}");
        }
    }

    #[test]
    fn specification_sibling_numeric_functions_keep_owner_and_syntax_authority() {
        for raw in ["sibling-index()","sibling-count(/* comment */)",r"sibling-\69 ndex()","calc(1dppx * sibling-index())"] {
            let expression=parse_numeric_expression(raw).unwrap();assert!(expression.numeric_type().is_some());assert!(expression.contains_tree_functions());
            let (resolved,dependent)=substitute_sibling_functions(raw,2,4).unwrap();assert!(dependent);
            let expression=parse_numeric_expression(&resolved).unwrap();assert!(!expression.contains_tree_functions());
        }
        let(resolved,dependent)=substitute_sibling_functions("steps(sibling-index(), jump-none)",1,3).unwrap();
        assert!(dependent);assert_eq!(resolved,"steps(calc(1), jump-none)");
        let input=r#"image-set(url("sibling-index()") calc(1dppx * sibling-\69 ndex(/*x*/)))"#;
        let (resolved,dependent)=substitute_sibling_functions(input,3,4).unwrap();assert!(dependent);
        assert_eq!(resolved,r#"image-set(url("sibling-index()") calc(1dppx * calc(3)))"#);
        let (unchanged,dependent)=substitute_sibling_functions(r#"url(sibling-index()) "sibling-count()" /*sibling-index()*/"#,2,4).unwrap();
        assert!(!dependent);assert!(matches!(unchanged,alloc::borrow::Cow::Borrowed(_)));
        for invalid in ["sibling-index(1)","sibling-count(1, 2)","sibling-index("] {assert!(parse_numeric_expression(invalid).is_none());assert!(substitute_sibling_functions(invalid,2,4).is_none());}
    }

    #[test]
    fn specification_progress_math_shared_types_clamping_and_degenerate_ranges() {
        for (raw,expected) in [
            ("progress(0.5, 0, 1)",0.5),("progress(100px, 0px, 50px)",1.0),
            ("progress(no-clamp 100px, 0px, 50px)",2.0),("progress(no-clamp -100px, 0px, 50px)",-2.0),
            ("progress(1%, (10% - 10%), 100%)",0.01),
            ("progress(abs(5%), hypot(3%, 4%), 10%)",0.0),
            ("progress(100px, 10px, 10px)",0.0),("progress(no-clamp 10px, 10px, 10px)",0.0),
            ("progress(progress(1, 0, 1), progress(0px, 0px, 1px), progress(1deg, 0deg, 1deg))",1.0),
        ] {
            let mut expression=parse_numeric_expression(raw).unwrap();assert_eq!(expression.numeric_type(),Some(NumericType::default()),"{raw}");
            expression.simplify_absolute_units();let value=expression.single_numeric_value().unwrap();
            assert_eq!(value.unit,NumericUnit::Number);assert!((value.value-expected).abs()<1e-12,"{raw}");
        }
        for (raw,expected) in [("progress(no-clamp 100px, 10px, 10px)","calc(infinity)"),("progress(no-clamp 1px, 10px, 10px)","calc(-infinity)")] {
            let mut expression=parse_numeric_expression(raw).unwrap();expression.simplify_absolute_units();
            assert_eq!(expression.serialize().as_deref(),Some(expected));
            assert!(parse_numeric_expression(expected).unwrap().single_numeric_value().unwrap().value.is_infinite());
        }
        for invalid in ["progress(1)","progress(1, 0)","progress(1, 0, 1, 2)","progress(no-clamp, 1, 2)","progress(1 no-clamp, 0, 1)","progress(1px, 0s, 1s)","progress(1px * 1px, 0px * 0px, 1px * 1px)"] {
            assert!(parse_numeric_expression(invalid).is_none_or(|expression|expression.numeric_type().is_none()),"{invalid}");
        }
        assert_eq!(computed_f32(f64::INFINITY),f32::MAX);
        assert_eq!(computed_f32(f64::NEG_INFINITY),-f32::MAX);
        assert_eq!(computed_f32(f64::NAN),0.0);
        assert!(!computed_f32(-0.0).is_sign_negative());
        for constant in ["infinity","-infinity","NaN"]{assert!(parse_numeric_expression(constant).is_none(),"calc-keywords are not bare numeric tokens");}
        assert!(parse_numeric_expression("calc(-InFiNiTy)").unwrap().single_numeric_value().unwrap().value.is_sign_negative());
    }

    #[test]
    fn specification_numeric_product_preserves_exact_conversion_cancellation() {
        for (raw,unit) in [("calc(1dpcm * 96 / 2.54)",NumericUnit::Dppx),("calc(1cm * 2.54 / 96)",NumericUnit::Px),("calc(1rad * 3.141592653589793 / 180)",NumericUnit::Deg)] {
            let mut expression=parse_numeric_expression(raw).unwrap();expression.simplify_absolute_units();
            assert_eq!(expression.single_numeric_value(),Some(NumericValue{value:1.0,unit}));
        }
        let mut expression=parse_numeric_expression("calc(2dpcm * 48 / 2.54)").unwrap();expression.simplify_absolute_units();
        assert_eq!(expression.single_numeric_value(),Some(NumericValue{value:1.0,unit:NumericUnit::Dppx}));
        let mut expression=parse_numeric_expression("calc(1dpcm * 96 / 2.540000000000001)").unwrap();expression.simplify_absolute_units();
        assert_ne!(expression.single_numeric_value().unwrap().value,1.0);
    }

    #[test]
    fn specification_computed_percentage_mix_does_not_simplify_authored_zero_units() {
        for input in ["calc(5% + 0px)","calc(0% + 1px)"] {
            let mut expression=parse_numeric_expression(input).unwrap();expression.simplify_absolute_units();
            assert_eq!(expression.serialize().as_deref(),Some(input),"authored calculation retains its units");
        }
        assert_eq!(crate::animation::combine_numeric_values(&[("0px",0.5),("10%",0.5)]).as_deref(),Some("5%"));
        assert_eq!(crate::animation::combine_numeric_values(&[("0%",0.5),("10px",0.5)]).as_deref(),Some("5px"));
        assert_eq!(crate::animation::combine_numeric_values(&[("0%",0.5),("0px",0.5)]).as_deref(),Some("0px"));
        assert_eq!(crate::animation::combine_numeric_values(&[("10px",0.5),("10%",0.5)]).as_deref(),Some("calc(5% + 5px)"));
        assert!(crate::animation::combine_numeric_values(&[("0s",0.5),("10px",0.5)]).is_none(),"invalid dimensions cannot be erased by a zero projection");
    }

    use super::*;
    use alloc::vec;

    #[test]
    fn specification_same_unit_sums_convert_once_without_serialization_rounding() {
        for (raw,expected) in [("calc(100dpi - 4dpi)","calc(1dppx)"),("calc(100in - 4in)","calc(9216px)"),("calc(100ms - 4ms)","calc(0.096s)")] {
            let mut expression=parse_numeric_expression(raw).unwrap();
            expression.simplify_absolute_units();
            assert_eq!(expression.serialize().as_deref(),Some(expected),"{raw}");
        }
    }

    #[test]
    fn specification_computed_math_folds_resolved_operators_and_keeps_unknown_bases() {
        for(raw,expected)in [
            ("calc(70% + 10% * sign(999px))","calc(80%)"),
            ("calc(70% + 10% * sign(-1px))","calc(60%)"),
            ("calc(70% + 10% * sign(0px))","calc(70%)"),
            ("calc(10px * (6 / 2))","calc(30px)"),
            ("calc(10px / 2px)","calc(5)"),
            ("min(1in, 100px)","96px"),
            ("max(1s, 2000ms)","2s"),
            ("clamp(30px, 10px, 20px)","30px"),
            ("calc(100% - 100% + 1px)","calc(0% + 1px)"),
            ("calc(1px + 10% - 2px)","calc(10% - 1px)"),
            ("calc(70% + 10% * sign(1em))","calc(70% + (10% * sign(1em)))"),
            ("min(10%, 20%)","min(10%, 20%)"),
            ("sign(10%)","sign(10%)"),
        ] {
            let mut expression=parse_numeric_expression(raw).unwrap();
            expression.simplify_absolute_units();
            assert_eq!(expression.serialize().unwrap(),expected,"{raw}");
            assert!(expression.checked_retained_bytes().unwrap()<=MAX_NUMERIC_EXPRESSION_BYTES*8);
        }
        let mut zero=parse_numeric_expression("sign(-0px)").unwrap();zero.simplify_absolute_units();
        let zero=zero.single_numeric_value().unwrap();assert_eq!(zero.value,0.0);assert!(zero.value.is_sign_negative());
    }

    #[test]
    fn shared_math_canonicalizes_primitive_products_and_function_arguments() {
        for (raw,expected) in [("calc(30deg * 2)","calc(60deg)"),
            ("calc(30deg + sign(2cqw - 10px) * 5deg)","calc(30deg + (5deg * sign(2cqw - 10px)))")] {
            let mut expression = parse_numeric_expression(raw).unwrap();
            expression.simplify_absolute_units();
            assert_eq!(expression.serialize().unwrap(), expected, "{raw}");
        }
    }

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
                if matches!(value.as_ref(), NumericExpression::Value(NumericValue {
                    value: 97.0,
                    unit: NumericUnit::Px,
                }))
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
        assert!(parse_numeric_expression("calc(sign(1px))")
            .unwrap()
            .contains_sign());

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
        assert!(length_squared
            .add(NumericType::from_unit(NumericUnit::Percent))
            .is_none());

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

/// CSS Values range checks apply to the top-level calculation. Keep IEEE
/// values inside the tree; censor NaN/signed zero and bound the final scalar
/// to the implementation's existing f32 storage range only after evaluation.
pub(super) fn computed_f32(value:f64)->f32 {
    if value.is_nan()||value==0.0{0.0}else{value.clamp(-f64::from(f32::MAX),f64::from(f32::MAX)) as f32}
}

/// Resolve the same numeric tree functions inside any CSS component value.
/// Syntax owns strings/comments/URLs, escaped names and block bounds. Ordinary
/// values stay borrowed; output allocates only after a real owner dependency.
pub(super) fn substitute_sibling_functions(input:&str,index:usize,count:usize)->Option<(alloc::borrow::Cow<'_,str>,bool)> {
    use super::syntax::{Cursor,TokenKind};
    if input.len()>super::MAX_VARIABLE_BYTES{return None;}
    let mut cursor=Cursor::new(input,0).ok()?;let mut output=String::new();let mut copied=0;let mut changed=false;
    while let Some(token)=cursor.next(){
        if token.kind!=TokenKind::Other||input.as_bytes().get(token.end)!=Some(&b'('){continue;}
        let mut at=token.start;let Some(name)=super::consume_selector_identifier(input,&mut at).filter(|_|at==token.end)else{continue;};
        let function=if name.eq_ignore_ascii_case("sibling-index"){MathFunction::SiblingIndex}else if name.eq_ignore_ascii_case("sibling-count"){MathFunction::SiblingCount}else{continue;};
        let block=super::syntax::block(input,token.end).ok()?;if !block.closed{return None;}
        let expression=parse_numeric_expression(&input[token.start..block.after])?;
        if expression.numeric_type()!=Some(NumericType::default()){return None;}
        let value=match function{MathFunction::SiblingIndex=>index,MathFunction::SiblingCount=>count,_=>unreachable!()};
        if !changed{output.try_reserve(input.len()).ok()?;}
        output.push_str(&input[copied..token.start]);
        // Tree functions are math expressions, even when their computed result
        // is an integer. Preserve that boundary so consuming grammar applies
        // computed-value range clamping rather than literal admission rules.
        output.push_str("calc(");output.push_str(&value.to_string());output.push(')');
        if output.len()>super::MAX_VARIABLE_BYTES{return None;}
        copied=block.after;cursor.position=block.after;changed=true;
    }
    if !changed{return Some((alloc::borrow::Cow::Borrowed(input),false));}
    output.push_str(&input[copied..]);
    (output.len()<=super::MAX_VARIABLE_BYTES).then_some((alloc::borrow::Cow::Owned(output),true))
}

/// Resolve numeric leaves with the same dimensional authority used by lengths.
/// Absolute dimensions retain their f64 precision; only contextual metrics use
/// the layout's existing f32 length resolver.
pub(super) fn computed_numeric_value(value: NumericValue, context: super::LengthContext, query: super::ContainerUnitContext) -> Option<NumericValue> {
    if let Some((unit, factor)) = value.unit.canonical_unit_and_factor() {
        let value = NumericValue { unit, value: value.value * factor };
        return Some(value);
    }
    if value.unit.dimension() != NumericDimension::Length { return Some(value); }
    let mut resolver = super::LengthValueBuilder { context: Some(context), query,
        allow_viewport: true, scalar: false, sign_input_depth: 0, context_dependent: false };
    let (pixels, dimension) = resolver.value(value)?;
    dimension.then_some(NumericValue { unit: NumericUnit::Px, value: f64::from(pixels) })
}

/// Units whose basis cannot be changed by an authored CSS property.
pub(super) fn computationally_independent_unit(unit:NumericUnit)->bool {
    use NumericUnit::*;
    matches!(unit,Number|Percent|Px|In|Cm|Mm|Q|Pt|Pc|Deg|Grad|Rad|Turn|S|Ms|Hz|KHz|Dpi|Dpcm|Dppx
        |Vw|Vh|Vi|Vb|Vmin|Vmax|Svw|Svh|Svi|Svb|Svmin|Svmax|Lvw|Lvh|Lvi|Lvb|Lvmin|Lvmax|Dvw|Dvh|Dvi|Dvb|Dvmin|Dvmax)
}
