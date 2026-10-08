use super::*;

/// Computed decoration lengths keep percentages relative to the originating
/// font, not to a containing block. Only nonlinear math needs rare storage.
#[derive(Clone, Debug, PartialEq)]
pub enum DecorationLength {
    Auto,
    FromFont,
    LineWidth(u8),
    Length(TransformLength),
    Expression(Arc<DecorationExpression>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct DecorationExpression {
    expression:typed_numeric::NumericExpression,
}

// Reuse the shared CSS length type/operation policy for AST evaluation. This
// adapter performs no parsing, allocation, or independent unit arithmetic.
struct DecorationEvaluation(LengthValueBuilder);
impl typed_numeric::NumericExpressionContext for DecorationEvaluation {
    type Value=(f32,bool);
    fn unit(&mut self,value:typed_numeric::NumericValue)->Option<Self::Value> {
        typed_numeric::NumericExpressionBuilder::value(&mut self.0,value)
    }
    fn add(&mut self,left:Self::Value,right:Self::Value)->Option<Self::Value> {
        use typed_numeric::NumericExpressionBuilder;
        let mut result=self.0.begin_sum(left)?;self.0.push_sum(&mut result,right,false)?;self.0.finish_sum(result,true)
    }
    fn multiply(&mut self,left:Self::Value,right:Self::Value)->Option<Self::Value> {
        use typed_numeric::NumericExpressionBuilder;
        let mut result=self.0.begin_product(left)?;self.0.push_product(&mut result,right,false)?;self.0.finish_product(result,true)
    }
    fn minimum(&mut self,left:Self::Value,right:Self::Value)->Option<Self::Value> {
        use typed_numeric::NumericExpressionBuilder;
        let mut result=self.0.begin_min(left)?;self.0.push_min(&mut result,right)?;self.0.finish_min(result)
    }
    fn maximum(&mut self,left:Self::Value,right:Self::Value)->Option<Self::Value> {
        use typed_numeric::NumericExpressionBuilder;
        let mut result=self.0.begin_max(left)?;self.0.push_max(&mut result,right)?;self.0.finish_max(result)
    }
    fn clamp(&mut self,lower:Self::Value,value:Self::Value,upper:Self::Value)->Option<Self::Value> {
        typed_numeric::NumericExpressionBuilder::clamp(&mut self.0,lower,value,upper)
    }
    fn negate(&mut self,value:Self::Value)->Option<Self::Value> {
        typed_numeric::NumericExpressionBuilder::negate(&mut self.0,value)
    }
    fn invert(&mut self,value:Self::Value)->Option<Self::Value> {
        typed_numeric::NumericExpressionBuilder::invert(&mut self.0,value)
    }
    fn sign(&mut self,value:Self::Value)->Option<Self::Value> {
        typed_numeric::NumericExpressionBuilder::sign(&mut self.0,value)
    }
}

impl DecorationLength {
    /// Interpolate computed lengths without resolving percentages against a
    /// layout box. Nonlinear endpoints reuse the canonical numeric AST policy.
    pub fn interpolate_css_value(&self, other: &Self, progress: f64) -> Option<String> {
        self.combine_css_value(other,1.0-progress,progress)
    }

    /// Weighted computed-value arithmetic shared by interpolation and addition.
    pub fn combine_css_value(&self, other:&Self, left_weight:f64, right_weight:f64)->Option<String> {
        if !left_weight.is_finite()||!right_weight.is_finite(){return None;}
        let affine = |value: &Self| match value {
            Self::Length(value) => Some(*value),
            Self::LineWidth(value) => Some(TransformLength { pixels: f32::from(*value), percent: 0.0 }),
            _ => None,
        };
        if let (Some(a),Some(b))=(affine(self),affine(other)) {
            let mix=|a:f32,b:f32| {
                let value=(f64::from(a)*left_weight+f64::from(b)*right_weight) as f32;
                value.is_finite().then_some(value)
            };
            return Some(Self::Length(TransformLength {pixels:mix(a.pixels,b.pixels)?,percent:mix(a.percent,b.percent)?}).computed());
        }
        use typed_numeric::{NumericExpression as Expression,NumericValue,NumericUnit};
        let expression=|value:&Self|match value {
            Self::Expression(value)=>Some(value.expression.clone()),
            value=>affine(value).map(|value|Expression::Sum(vec![
                Expression::Value(NumericValue {value:f64::from(value.pixels),unit:NumericUnit::Px}),
                Expression::Value(NumericValue {value:f64::from(value.percent),unit:NumericUnit::Percent}),
            ])),
        };
        let weighted=|value,weight|Expression::Product(vec![value,
            Expression::Value(NumericValue{value:weight,unit:NumericUnit::Number})]);
        Expression::Calc(Box::new(Expression::Sum(vec![weighted(expression(self)?,left_weight),
            weighted(expression(other)?,right_weight)]))).serialize()
    }

    /// Heap storage retained by this value, excluding its inline enum storage.
    /// Shared expressions are conservatively charged for each retained owner.
    pub(crate) fn retained_bytes(&self) -> Option<usize> {
        match self {
            Self::Expression(value)=>(2*core::mem::size_of::<usize>())
                .checked_add(core::mem::size_of::<DecorationExpression>())?
                .checked_add(value.expression.checked_retained_bytes()?),
            _=>Some(0),
        }
    }
    pub fn serialize(&self) -> String {
        match self {
            Self::Auto => "auto".into(),
            Self::FromFont => "from-font".into(),
            Self::LineWidth(value) => match value {1=>"thin",3=>"medium",_=>"thick"}.into(),
            Self::Expression(value) => value.expression.serialize().expect("validated computed decoration expression"),
            Self::Length(value) => computed_values::percentage(value.pixels,value.percent),
        }
    }
    pub fn computed(&self) -> String {
        match self {Self::LineWidth(value)=>alloc::format!("{value}px"),_=>self.serialize()}
    }
    pub fn used(&self, font_size:f32) -> Option<f32> {
        match self {
            Self::Auto|Self::FromFont=>None,
            Self::LineWidth(value)=>Some(f32::from(*value)),
            Self::Length(value)=>Some(value.pixels + value.percent * font_size / 100.0),
            Self::Expression(value)=>{
                let mut context=DecorationEvaluation(LengthValueBuilder {
                    context:Some(LengthContext{percent:Some(font_size),..static_length_context()}),
                    query:ContainerUnitContext::default(),allow_viewport:true,
                    scalar:false,sign_input_depth:0,context_dependent:false,
                });
                value.expression.evaluate(&mut context).map(|value|value.0)
            },
        }
    }
}

pub(super) fn serialize_decoration_length(raw:&str)->Option<String> {
    let mut expression=typed_numeric::parse_numeric_expression(raw)?;
    if let typed_numeric::NumericExpression::Value(value)=&mut expression {
        if value.unit==typed_numeric::NumericUnit::Number && value.value==0.0 {value.unit=typed_numeric::NumericUnit::Px;}
    }
    expression.serialize()
}

pub(super) fn decoration_length_value(slot:usize,raw:&str)->Option<Value> {
    let raw=ascii_lower(raw.trim());
    let keyword=match raw.as_ref() {
        "auto"=>Some(DecorationLength::Auto),
        "from-font" if slot==214=>Some(DecorationLength::FromFont),
        "thin" if slot==214=>Some(DecorationLength::LineWidth(1)),
        "medium" if slot==214=>Some(DecorationLength::LineWidth(3)),
        "thick" if slot==214=>Some(DecorationLength::LineWidth(5)),
        _=>None,
    };
    if let Some(value)=keyword{return Some(Value::DecorationLength(slot,value));}
    // The length grammar is signed. Used thickness is rounded and clamped by
    // the painter; negative authored values remain valid computed values.
    contextual_length(&raw,Some(LengthContext{percent:Some(100.0),..static_length_context()}))?;
    Some(Value::ContextLength(slot,Box::from(raw.as_ref()),false))
}

pub(super) fn decoration_length_with_context(raw:&str,context:LengthContext,query:ContainerUnitContext)->Option<DecorationLength> {
    let mut parsed=if raw.contains('%') && math_function(raw) {typed_numeric::parse_numeric_expression(raw)}else{None};
    fn dependence(expression:&typed_numeric::NumericExpression)->(bool,bool) {
        use typed_numeric::{NumericExpression as E,NumericUnit as U};
        match expression {
            E::Value(value)=>(value.unit==U::Percent,false),
            E::Identifier(_)=>(false,false),
            E::Calc(value)|E::Negate(value)=>dependence(value),
            E::Invert(value)|E::Sign(value)=>{let (dependent,nonlinear)=dependence(value);(dependent,nonlinear||dependent)},
            E::Sum(values)|E::Product(values)|E::Min(values)|E::Max(values)|E::Function(_,values)=>{
                let mut count=0;let mut nonlinear=false;
                for value in values {let (dependent,complex)=dependence(value);count+=usize::from(dependent);nonlinear|=complex;}
                nonlinear|=count!=0 && matches!(expression,E::Min(_)|E::Max(_)|E::Function(..));
                nonlinear|=count>1 && matches!(expression,E::Product(_));
                (count!=0,nonlinear)
            }
            E::Clamp(lower,value,upper)=>{let a=dependence(lower);let b=dependence(value);let c=dependence(upper);(a.0||b.0||c.0,a.1||b.1||c.1||a.0||b.0||c.0)},
        }
    }
    if parsed.as_ref().is_some_and(|expression|dependence(expression).1) {
        let mut expression=parsed.take()?;
        fn compute(expression:&mut typed_numeric::NumericExpression,context:LengthContext,query:ContainerUnitContext)->Option<()> {
            use typed_numeric::{NumericExpression as E,NumericUnit as U};
            match expression {
                E::Value(value) if !matches!(value.unit,U::Number|U::Percent)=>{
                    let raw=typed_numeric::serialize_numeric_value(value.value,value.unit);
                    value.value=f64::from(contextual_length_with_query(&raw,Some(context),true,query)?);
                    value.unit=U::Px;
                }
                E::Value(_)=>{},
                E::Identifier(_)=>return None,
                E::Function(_,_)=>expression.map_numeric_values(|value|typed_numeric::computed_numeric_value(value,context,query))?,
                E::Calc(value)|E::Negate(value)|E::Invert(value)|E::Sign(value)=>compute(value,context,query)?,
                E::Sum(values)|E::Product(values)|E::Min(values)|E::Max(values)=>for value in values {compute(value,context,query)?;},
                E::Clamp(lower,value,upper)=>{compute(lower,context,query)?;compute(value,context,query)?;compute(upper,context,query)?;},
            }
            Some(())
        }
        compute(&mut expression,context,query)?;
        expression.simplify_absolute_units();
        // Validate the canonical CSSOM output bound once before sharing this
        // immutable AST; painting subsequently evaluates it without strings.
        expression.serialize()?;
        return Some(DecorationLength::Expression(Arc::new(DecorationExpression{expression})));
    }
    let pixels=contextual_length_with_query(raw,Some(LengthContext{percent:Some(0.0),..context}),true,query)?;
    let hundred=contextual_length_with_query(raw,Some(LengthContext{percent:Some(100.0),..context}),true,query)?;
    Some(DecorationLength::Length(TransformLength{pixels,percent:hundred-pixels}))
}

#[cfg(test)]
mod percentage_unit_tests {
    use super::*;
    #[test]
    fn specification_computed_length_percentages_keep_units_through_serialization_and_composition() {
        let context=static_length_context();
        let query=ContainerUnitContext::no_container(context.viewport);
        let low=decoration_length_with_context("10%",context,query).unwrap();
        let high=decoration_length_with_context("90%",context,query).unwrap();
        let thirty=decoration_length_with_context("30%",context,query).unwrap();
        assert_eq!(thirty.computed(),"30%");
        assert_eq!(low.interpolate_css_value(&high,0.75).as_deref(),Some("70%"));
        assert_eq!(low.combine_css_value(&high,1.0,1.0).as_deref(),Some("100%"));
        assert_eq!(thirty.used(200.0),Some(60.0));
        assert_eq!(core::mem::size_of::<TransformLength>(),core::mem::size_of::<LengthPercentage>(),"the computed-unit repair does not enlarge affine state");
    }
}
