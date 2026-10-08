//! Individual transforms share numeric syntax, computation and homogeneous
//! geometry with transform functions. Computed trees retain percentage bases.
use super::*;
use typed_numeric::{NumericExpression as E,NumericValue as V,NumericUnit as U};
use typed_transforms::TransformArgument as A;

#[derive(Clone,Debug,PartialEq)]
pub struct IndividualTransform {slot:usize,arguments:Vec<E>}
fn value(value:f64,unit:U)->E {E::Value(V{value,unit})}
fn argument(raw:&str,kind:A)->Option<E> {
    // Individual rotate uses <angle>; unlike rotate(), it has no zero-number
    // grammar exception.
    if kind==A::Angle&&typed_numeric::parse_numeric_value(raw).is_some_and(|v|v.unit==U::Number){return None;}
    typed_transforms::argument_expression(raw,kind)
}
fn parsed(slot:usize,raw:&str)->Option<IndividualTransform> {
    let tokens=components(raw)?;let mut arguments=Vec::new();arguments.try_reserve_exact(4).ok()?;
    match slot {
        239=>{
            if !(1..=3).contains(&tokens.len()){return None;}
            for(index,raw)in tokens.iter().enumerate(){arguments.push(argument(raw,if index==2{A::Length}else{A::LengthPercentage})?);}
            while arguments.len()<3{arguments.push(value(0.0,U::Px));}
        },
        241=>{
            if !(1..=3).contains(&tokens.len()){return None;}
            for raw in &tokens{
                let validated=argument(raw,A::Scale)?;
                // Bare percentages serialize as numbers; calculations retain
                // their specified numeric type until computed-value conversion.
                arguments.push(if typed_numeric::parse_numeric_value(raw).is_some(){validated}else{typed_numeric::parse_numeric_expression(raw)?});
            }
            if arguments.len()==1{arguments.push(arguments[0].clone());}
            if arguments.len()==2{arguments.push(value(1.0,U::Number));}
        },
        240=>{
            let (axis,angle)=match tokens.as_slice(){
                [angle]=>{argument(angle,A::Angle)?;(vec![value(0.0,U::Number),value(0.0,U::Number),value(1.0,U::Number)],*angle)},
                [first,second]=>{
                    let(axis,angle)=if decoded_css_keyword(first,"x")||decoded_css_keyword(first,"y")||decoded_css_keyword(first,"z"){(*first,*second)}else{(*second,*first)};
                    let components=if decoded_css_keyword(axis,"x"){[1.0,0.0,0.0]}else if decoded_css_keyword(axis,"y"){[0.0,1.0,0.0]}else if decoded_css_keyword(axis,"z"){[0.0,0.0,1.0]}else{return None;};
                    (components.into_iter().map(|n|value(n,U::Number)).collect(),angle)
                },
                [a,b,c,d]=>{
                    let(axis,angle)=if argument(d,A::Angle).is_some(){([*a,*b,*c],*d)}else{([*b,*c,*d],*a)};
                    (axis.into_iter().map(|raw|argument(raw,A::Number)).collect::<Option<Vec<_>>>()?,angle)
                },
                _=>return None,
            };
            arguments=axis;arguments.push(argument(angle,A::Angle)?);
        },
        _=>return None,
    }
    Some(IndividualTransform{slot,arguments})
}
pub(super) fn accepts(slot:usize,raw:&str)->bool {decoded_css_keyword(raw,"none")||parsed(slot,raw).is_some()}
pub(super) fn computed(slot:usize,raw:&str,context:LengthContext,query:ContainerUnitContext)->Option<Option<Arc<IndividualTransform>>> {
    if decoded_css_keyword(raw,"none"){return Some(None);}
    let mut parsed=parsed(slot,raw)?;
    for expression in &mut parsed.arguments {
        // Preserve exact source factors before contextual leaf projection.
        expression.simplify_absolute_units();
        if slot==241{expression.percentages_as_numbers()?;}
        expression.map_numeric_values(|value|typed_numeric::computed_numeric_value(value,context,query))?;
        expression.simplify_absolute_units();
        if let Some(v)=expression.single_numeric_value(){
            if !v.value.is_finite()||v.value==0.0{*expression=value(if v.value==0.0{0.0}else{f64::from(typed_numeric::computed_f32(v.value))},v.unit);}
        }
    }
    Some(Some(Arc::new(parsed)))
}
impl IndividualTransform {
    pub(super) fn checked_retained_bytes(&self)->Option<usize> {
        let mut bytes=self.arguments.capacity().checked_mul(core::mem::size_of::<E>())?;
        for argument in &self.arguments{bytes=bytes.checked_add(argument.checked_retained_bytes()?)?;}
        Some(bytes)
    }
    pub(super) fn matrix(&self,reference:[f64;2])->Option<lumen_common::dom_geometry::Matrix> {
        use lumen_common::dom_geometry::{Matrix,Point};
        let scalar=|index,basis|typed_transforms::resolved_argument_scalar(self.arguments.get(index)?,basis);
        let identity=Matrix::default();Some(match self.slot {
            239=>identity.translated(scalar(0,Some(reference[0]))?,scalar(1,Some(reference[1]))?,scalar(2,None)?),
            240=>identity.rotated_axis(scalar(0,None)?,scalar(1,None)?,scalar(2,None)?,scalar(3,None)?),
            241=>identity.scaled(scalar(0,None)?,scalar(1,None)?,scalar(2,None)?,Point::default()),
            _=>return None,
        })
    }
    fn serialized(&self,computed:bool)->Option<String> {
        let component=|index:usize|->Option<String>{let expression=self.arguments.get(index)?;
            if computed||matches!(expression,E::Value(_)){if let Some(value)=expression.single_numeric_value(){return Some(typed_numeric::serialize_numeric_value(value.value,value.unit));}}
            if computed{expression.serialize()}else{expression.clone().serialize_specified()}
        };
        let numeric=|index:usize,number:f64,unit:U|self.arguments.get(index).and_then(E::single_numeric_value).is_some_and(|value|value.value==number&&value.unit==unit);
        let mut parts=Vec::new();
        match self.slot {
            239=>{parts.push(component(0)?);if !numeric(1,0.0,U::Px)||!numeric(2,0.0,U::Px){parts.push(component(1)?);}if !numeric(2,0.0,U::Px){parts.push(component(2)?);}},
            241=>{parts.push(component(0)?);if self.arguments[0]!=self.arguments[1]||!numeric(2,1.0,U::Number){parts.push(component(1)?);}if !numeric(2,1.0,U::Number){parts.push(component(2)?);}},
            240=>{
                let axis=(0..3).map(|index|self.arguments[index].single_numeric_value().filter(|value|value.unit==U::Number).map(|value|value.value)).collect::<Option<Vec<_>>>();
                if let Some(axis)=axis {
                    let parallel=if axis[0]!=0.0&&axis[1]==0.0&&axis[2]==0.0{Some((0,"x"))}else if axis[1]!=0.0&&axis[0]==0.0&&axis[2]==0.0{Some((1,"y"))}else if axis[2]!=0.0&&axis[0]==0.0&&axis[1]==0.0{Some((2,"z"))}else{None};
                    if let Some((index,name))=parallel{
                        if index!=2{parts.push(name.into());}
                        if axis[index]<0.0{
                            let mut angle=if let E::Value(v)=self.arguments[3]{value(-v.value,v.unit)}else{E::Product(vec![value(-1.0,U::Number),self.arguments[3].clone()])};
                            angle.simplify_absolute_units();
                            parts.push(if computed{angle.single_numeric_value().map(|v|typed_numeric::serialize_numeric_value(v.value,v.unit)).or_else(||angle.serialize())?}else{if let E::Value(v)=angle{typed_numeric::serialize_numeric_value(v.value,v.unit)}else{angle.serialize_specified()?}});
                            return Some(parts.join(" "));
                        }
                    }else{for index in 0..3{parts.push(component(index)?);}}
                }else{for index in 0..3{parts.push(component(index)?);}}
                parts.push(component(3)?);
            },
            _=>return None,
        }
        Some(parts.join(" "))
    }
    pub(super) fn computed_css_value(&self)->Option<String>{self.serialized(true)}
}
pub(super) fn specified(slot:usize,raw:&str)->Option<String> {
    if decoded_css_keyword(raw,"none"){return Some("none".into());}
    parsed(slot,raw)?.serialized(false)
}

/// Computed endpoints retain percentages; combination never resolves a
/// translate percentage against a guessed box.
pub(super) fn combine(slot:usize,from:&str,to:&str,operation:registered_properties::ComputedValueOperation,context:LengthContext,query:ContainerUnitContext)->Option<String>{
    use registered_properties::ComputedValueOperation as Op;
    let a=computed(slot,from,context,query)?;let b=computed(slot,to,context,query)?;
    if a.is_none()&&b.is_none(){return Some("none".into());}
    let identity=||parsed(slot,match slot{239=>"0px",240=>"0deg",241=>"1",_=>"none"});
    let a=a.as_deref().cloned().or_else(identity)?;let b=b.as_deref().cloned().or_else(identity)?;
    if slot==240 {
        let scalars=|value:&IndividualTransform|->Option<[f64;4]>{Some(core::array::from_fn(|i|value.arguments[i].single_numeric_value().map(|v|v.value).unwrap_or(f64::NAN))).filter(|values|values.iter().all(|v|v.is_finite()))};
        let result=typed_transforms::combine_rotation_arguments(scalars(&a)?,scalars(&b)?,operation)?;
        return IndividualTransform{slot,arguments:result.into_iter().enumerate().map(|(i,n)|value(n,if i==3{U::Deg}else{U::Number})).collect()}.computed_css_value();
    }
    let mut result=a.clone();
    for i in 0..3 {
        let left=&a.arguments[i];let right=&b.arguments[i];
        if let(Some(x),Some(y))=(left.single_numeric_value(),right.single_numeric_value()){
            if x.unit==y.unit{
                let number=match operation{Op::Interpolate(p)=>x.value+(y.value-x.value)*p,Op::Add=>if slot==241{x.value*y.value}else{x.value+y.value},Op::Accumulate(n)=>n*(x.value-if slot==241{1.0}else{0.0})+y.value};
                if !number.is_finite(){return None;}result.arguments[i]=value(number,x.unit);continue;
            }
        }
        let left=left.serialize()?;let right=right.serialize()?;
        let combined=match operation{Op::Interpolate(p)=>crate::animation::combine_numeric_values(&[(&left,1.0-p),(&right,p)]),Op::Add=>crate::animation::combine_numeric_values(&[(&left,1.0),(&right,1.0)]),Op::Accumulate(n)=>crate::animation::combine_numeric_values(&[(&left,n),(&right,1.0)])}?;
        result.arguments[i]=typed_numeric::parse_numeric_expression(&combined)?;
    }
    result.computed_css_value()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn specification_individual_transform_grammar_computation_and_animation(){
        let context=static_length_context();let query=ContainerUnitContext::default();
        let compute=|slot,raw|computed(slot,raw,context,query).unwrap().unwrap().computed_css_value().unwrap();
        assert_eq!(specified(240,"400grad -0.5 0 0").as_deref(),Some("x -400grad"));
        assert_eq!(specified(241,"calc(200%)").as_deref(),Some("calc(200%)"));
        assert_eq!(specified(241,"200%").as_deref(),Some("2"));
        assert_eq!(specified(239,"2cm 0px").as_deref(),Some("2cm"));
        assert_eq!(specified(241,"round(nearest, 1.5, 1)").as_deref(),Some("calc(2)"));
        assert_eq!(typed_transforms::specified("scale(progress(5%, 0%, 10%))").as_deref(),Some("scale(calc(0.5))"));
        assert_eq!(compute(241,"calc(200%)"),"2");
        assert_eq!(compute(241,"calc(1 + 100%)"),"2");
        assert_eq!(compute(241,"max(1, 50%)"),"1");
        assert_eq!(compute(241,"calc(progress(5%,0%,10%) * 200%)"),"1");
        assert_eq!(compute(241,"calc(0 / 0)"),"0");
        assert!(compute(241,"calc(1 / 0)").parse::<f64>().unwrap().is_finite());
        assert_eq!(compute(240,"100 200 300 400grad"),"100 200 300 360deg");
        assert_eq!(compute(239,"2em 25%"),"32px 25%");
        assert_eq!(compute(241,"progress(5%, 0%, 10%)"),"0.5");
        assert!(!accepts(240,"0"));assert!(!accepts(239,"1px 2px 3%"));assert!(!accepts(241,"progress(5%, 0deg, 10deg)"));
        use registered_properties::ComputedValueOperation as Op;
        let mix=|slot,a,b,op|combine(slot,a,b,op,context,query).unwrap();
        assert_eq!(mix(241,"none","3",Op::Interpolate(0.5)),"2");
        assert_eq!(mix(241,"2","5",Op::Add),"10");
        assert_eq!(mix(241,"2","5",Op::Accumulate(2.0)),"7");
        assert_eq!(mix(240,"y 0deg","y 720deg",Op::Interpolate(0.25)),"y 180deg");
        assert_eq!(mix(239,"100px","50%",Op::Interpolate(0.5)),"calc(25% + 50px)");
        assert_eq!(mix(239,"none","none",Op::Interpolate(0.5)),"none");
        let rotated=mix(240,"x 90deg","y 90deg",Op::Interpolate(0.5));
        let used=computed(240,&rotated,context,query).unwrap().unwrap().matrix([100.0,100.0]).unwrap();
        let expected=lumen_common::dom_geometry::Matrix::default().rotated_axis(1.0,1.0,0.0,2.0*libm::acos(libm::sqrt(2.0/3.0))*180.0/core::f64::consts::PI);
        for(a,b)in used.values.iter().zip(expected.values){assert!((a-b).abs()<1e-12,"{rotated}: {a} vs {b}");}
    }
    #[test]
    fn specification_individual_transform_sparse_order_presence_and_slot_lifecycle(){
        let context=static_length_context();let query=ContainerUnitContext::default();let initial=Style::initial();
        assert!(!initial.has_transform());assert!(initial.extras.is_none());
        let mut style=initial.clone();style.transform_origin=[TransformLength{pixels:0.0,percent:0.0};2];
        for(slot,raw)in[(239,"50%"),(240,"90deg"),(241,"2")]{Value::IndividualTransform(slot,computed(slot,raw,context,query).unwrap()).apply(&mut style);}
        style.transforms=Some(Arc::from([Transform::Translate(TransformLength{pixels:10.0,percent:0.0},TransformLength{pixels:0.0,percent:0.0})]));
        let rect=Rect{x:0.0,y:0.0,width:200.0,height:100.0};let used=style.transform_matrix(rect).unwrap();
        assert!((used.e-100.0).abs()<1e-5&&(used.f-20.0).abs()<1e-5);
        assert_eq!(style.computed_css_value("translate",computed_values::ComputedValueContext{resolved:true,border_box:Some(rect),..Default::default()}).as_deref(),Some("50%"));
        assert!(style.checked_private_payload_bytes().is_some());
        let mut copied=initial.clone();copy_slot_state(239,&style,&mut copied,true);assert!(copied.has_transform());
        copied.relative_expressions.push(RelativeExpression{source_url:None,slot:239,raw:Arc::from("10cqw"),context,nonnegative:false,query_dependent:true,parent_font_pending:false,current_font_pending:false});
        copy_slot_state(239,&initial,&mut copied,false);assert!(!copied.has_transform());assert!(copied.relative_expressions.is_empty());
        Value::IndividualTransform(241,computed(241,"1",context,query).unwrap()).apply(&mut copied);assert!(copied.has_transform());
        Value::IndividualTransform(241,None).apply(&mut copied);assert!(!copied.has_transform());
    }
}
