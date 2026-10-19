//! Typed compound endpoints for CSS transitions. No property-string grammar
//! or font/layout lookup runs while sampling these immutable computed values.
use crate::{css::{Style, computed_values}, paint::{BackgroundSize, BackgroundSizeKind, BoxShadow, LengthPercentage, Rgba}};
use alloc::{vec::Vec, string::String};

fn mix(a: f32, b: f32, progress: f64) -> Option<f32> {
    let value = (f64::from(a) + (f64::from(b) - f64::from(a)) * progress) as f32;
    value.is_finite().then_some(value)
}

fn length(a: LengthPercentage, b: LengthPercentage, progress: f64) -> Option<LengthPercentage> {
    Some(LengthPercentage { pixels: mix(a.pixels, b.pixels, progress)?, fraction: mix(a.fraction, b.fraction, progress)? })
}

fn list_length(a: usize, b: usize) -> Option<usize> {
    if a == 0 || b == 0 { return None; }
    let (mut x, mut y) = (a, b);
    while y != 0 { (x, y) = (y, x % y); }
    let count = (a / x).checked_mul(b)?;
    (count <= crate::css::MAX_BACKGROUND_GEOMETRY_VALUES).then_some(count)
}

fn empty_shadow(inset: bool) -> BoxShadow {
    BoxShadow { offset_x: 0.0, offset_y: 0.0, blur: 0.0, spread: 0.0,
        color: Rgba { r: 0, g: 0, b: 0, a: 0 }, inset }
}

pub fn serialize_shadow_list(property:&str,values:&[BoxShadow])->Option<String> {
    let values=(!values.is_empty()).then_some(values);
    match property {
        "text-shadow"=>computed_values::text_shadows_css_value(values),
        "box-shadow"=>computed_values::shadows_css_value(values),_=>None,
    }
}

fn interpolate_shadow_values(a:&[BoxShadow],b:&[BoxShadow],progress:f64,
    mut color:impl FnMut(usize,Rgba,Rgba,f32)->Rgba)->Option<Vec<BoxShadow>> {
    let count=a.len().max(b.len());
    if !progress.is_finite()||count>64||a.iter().zip(b).any(|(a,b)|a.inset!=b.inset){return None;}
    let mut values=Vec::new();values.try_reserve_exact(count).ok()?;
    for index in 0..count {
        let inset=a.get(index).or_else(||b.get(index))?.inset;
        let a=a.get(index).copied().unwrap_or_else(||empty_shadow(inset));
        let b=b.get(index).copied().unwrap_or_else(||empty_shadow(inset));
        values.push(BoxShadow{offset_x:mix(a.offset_x,b.offset_x,progress)?,offset_y:mix(a.offset_y,b.offset_y,progress)?,
            blur:mix(a.blur,b.blur,progress)?.max(0.0),spread:mix(a.spread,b.spread,progress)?,
            color:color(index,a.color,b.color,progress as f32),inset});
    }
    Some(values)
}

pub fn interpolate_shadow_lists(property:&str,a:&[BoxShadow],b:&[BoxShadow],progress:f64,
    color:impl Fn(Rgba,Rgba,f32)->Rgba)->Option<String> {
    serialize_shadow_list(property,&interpolate_shadow_values(a,b,progress,|_,a,b,progress|color(a,b,progress))?)
}

fn serialize_source_shadow_values(property:&str,values:&[BoxShadow],colors:&[crate::css::SourceColor])->Option<String> {
    if values.is_empty(){return Some("none".into());}
    if values.len()!=colors.len()||values.len()>64{return None;}
    let mut entries=Vec::new();entries.try_reserve_exact(values.len()).ok()?;
    for (shadow,color) in values.iter().zip(colors){
        let color=color.serialize()?;
        entries.push(match property{
            "text-shadow"=>alloc::format!("{} {}px {}px {}px",color,computed_values::number(shadow.offset_x),computed_values::number(shadow.offset_y),computed_values::number(shadow.blur)),
            "box-shadow"=>alloc::format!("{} {}px {}px {}px {}px{}",color,computed_values::number(shadow.offset_x),computed_values::number(shadow.offset_y),computed_values::number(shadow.blur),computed_values::number(shadow.spread),if shadow.inset{" inset"}else{""}),
            _=>return None,
        });
    }
    Some(entries.join(", "))
}
/// Precision is preserved until the computed shadow colors enter paint.
pub fn interpolate_source_shadow_lists(property:&str,a:&crate::css::SourceShadowList,b:&crate::css::SourceShadowList,progress:f64)->Option<String> {
    let mut colors=Vec::new();colors.try_reserve_exact(a.shadows.len().max(b.shadows.len())).ok()?;
    let transparent=crate::css::SourceColor{value:lumen_common::color::Color::new(lumen_common::color::ColorSpace::Srgb,[0.0;3],0.0,0),color_function:false,expression:None};
    let values=interpolate_shadow_values(&a.shadows,&b.shadows,progress,|index,_,_,progress|{
        let color=crate::css::interpolate_source_colors(&a.color(index).unwrap_or_else(||transparent.clone()),&b.color(index).unwrap_or_else(||transparent.clone()),progress);
        let [r,g,b,a]=color.value.to_rgba8();let paint=Rgba{r,g,b,a};colors.push(color);paint
    })?;
    serialize_source_shadow_values(property,&values,&colors)
}

/// Shadow lists specify interpolation but no addition or accumulation. The
/// CSS Values default combining procedure therefore returns V_B unchanged.
/// Keep this typed compatibility entry point on the same canonical policy as
/// native effect composition; it allocates no temporary list.
pub fn compose_shadow_lists(property:&str,_a:&[BoxShadow],b:&[BoxShadow],_accumulate:bool,
    _color:impl Fn(Rgba,Rgba)->Rgba)->Option<String> {
    serialize_shadow_list(property,b)
}

/// Resolved precise endpoints are shared by native composition fallback and
/// interpolation; this does not invent an additive color/list operation.
pub fn serialize_source_shadow_list(property:&str,source:&crate::css::SourceShadowList)->Option<String> {
    let mut colors=Vec::new();colors.try_reserve_exact(source.shadows.len()).ok()?;
    for index in 0..source.shadows.len(){colors.push(source.color(index)?);}
    serialize_source_shadow_values(property,&source.shadows,&colors)
}

fn border_slot(property:&str)->Option<usize> {match property {"border-image-slice"=>Some(228),"border-image-width"=>Some(229),"border-image-outset"=>Some(230),_=>None}}
fn border_values(property:&str,a:&crate::css::BorderImage,b:&crate::css::BorderImage,left:f64,right:f64)->Option<String> {
    use crate::css::{BorderImageDimension as D,DecorationLength as L};
    if !left.is_finite()||!right.is_finite(){return None;}
    let number=|a:f32,b:f32| {let result=(f64::from(a)*left+f64::from(b)*right) as f32;result.is_finite().then_some(result.max(0.0))};
    let mut values=Vec::new();values.try_reserve_exact(4).ok()?;
    if border_slot(property)?==228 {
        if a.fill!=b.fill{return None;}
        for (a,b) in a.slice.iter().zip(b.slice.iter()) {
            if a.percentage!=b.percentage{return None;}
            let value=number(a.value,b.value)?;
            values.push(if a.percentage{alloc::format!("{}%",computed_values::number(value))}else{computed_values::number(value)});
        }
        let mut result=computed_values::four(values.try_into().ok()?);if a.fill{result.push_str(" fill");}return Some(result);
    }
    let (a,b)=if property=="border-image-width"{(&a.width,&b.width)}else{(&a.outset,&b.outset)};
    for (a,b) in a.iter().zip(b.iter()) {
        values.push(match (a,b) {
            (D::Number(a),D::Number(b))=>computed_values::number(number(*a,*b)?),
            (D::Auto,D::Auto)=>String::from("auto"),
            (D::Length(a),D::Length(b))=> {
                if let (L::Length(a),L::Length(b))=(a,b) {
                    let pixels=(f64::from(a.pixels)*left+f64::from(b.pixels)*right) as f32;
                    let percent=(f64::from(a.percent)*left+f64::from(b.percent)*right) as f32;
                    if !pixels.is_finite()||!percent.is_finite(){return None;}
                    let value=crate::css::TransformLength{pixels:if percent==0.0{pixels.max(0.0)}else{pixels},percent:if pixels==0.0{percent.max(0.0)}else{percent}};
                    L::Length(value).computed()
                }else{a.combine_css_value(b,left,right)?}
            },
            _=>return None,
        });
    }
    Some(computed_values::four(values.try_into().ok()?))
}

pub fn interpolate_border_image(property:&str,a:&crate::css::BorderImage,b:&crate::css::BorderImage,progress:f64)->Option<String> {
    border_values(property,a,b,1.0-progress,progress)
}
/// By-computed-value addition replaces the whole endpoint on any mismatched
/// component (including fill, number/percentage or auto versus a length).
pub fn compose_border_image(property:&str,a:&crate::css::BorderImage,b:&crate::css::BorderImage)->Option<String> {
    border_values(property,a,b,1.0,1.0)
}

/// The host supplies its existing color interpolator, keeping image/color
/// implementation dependencies outside the HTML/layout core.
pub fn interpolate_compound_transition(
    property: &str, from: &Style, to: &Style, progress: f64,
    color: impl Fn(Rgba, Rgba, f32) -> Rgba,
) -> Option<String> {
    if !progress.is_finite() { return None; }
    match property {
        "border-image-slice"|"border-image-width"|"border-image-outset"=> {
            let initial=crate::css::BorderImage::default();
            interpolate_border_image(property,from.border_image.as_deref().unwrap_or(&initial),to.border_image.as_deref().unwrap_or(&initial),progress)
        },
        "text-indent" => Some(computed_values::lp(length(from.text_indent,to.text_indent,progress)?)),
        "word-spacing" => Some(computed_values::lp(length(from.word_spacing,to.word_spacing,progress)?)),
        "text-decoration-thickness" => from.text_decoration_thickness.interpolate_css_value(&to.text_decoration_thickness,progress),
        "text-underline-offset" => from.text_underline_offset.interpolate_css_value(&to.text_underline_offset,progress),
        "box-shadow"|"text-shadow" => {
            let slot=if property=="text-shadow"{219}else{58};
            let empty=crate::css::SourceShadowList{shadows:alloc::sync::Arc::from([]),colors:None};
            let a=from.source_shadow_list(slot);let b=to.source_shadow_list(slot);
            if a.as_ref().is_some_and(|list|list.colors.is_some())||b.as_ref().is_some_and(|list|list.colors.is_some()){
                interpolate_source_shadow_lists(property,a.as_ref().unwrap_or(&empty),b.as_ref().unwrap_or(&empty),progress)
            }else{interpolate_shadow_lists(property,a.as_ref().map_or(&[],|list|list.shadows.as_ref()),b.as_ref().map_or(&[],|list|list.shadows.as_ref()),progress,color)}
        },
        "background-position" => {
            let default = [[LengthPercentage::default(); 2]];
            let a = from.background_position.as_deref().unwrap_or(&default);
            let b = to.background_position.as_deref().unwrap_or(&default);
            let count = list_length(a.len(),b.len())?;
            let mut values = Vec::new();
            values.try_reserve_exact(count).ok()?;
            for index in 0..count {
                let (a,b) = (a[index % a.len()],b[index % b.len()]);
                values.push([length(a[0],b[0],progress)?,length(a[1],b[1],progress)?]);
            }
            computed_values::background_positions_css_value(Some(&values))
        }
        "background-size" => {
            if from.background_size_math.is_some() || to.background_size_math.is_some() {
                return from.interpolate_background_size_math(to, progress);
            }
            let default = [BackgroundSize::AUTO];
            let a = from.background_size.as_deref().unwrap_or(&default);
            let b = to.background_size.as_deref().unwrap_or(&default);
            let count = list_length(a.len(),b.len())?;
            let mut values = Vec::new();
            values.try_reserve_exact(count).ok()?;
            for index in 0..count {
                let (a,b) = (a[index % a.len()],b[index % b.len()]);
                if a.kind != b.kind { return None; }
                if a.kind != BackgroundSizeKind::Explicit { values.push(a); continue; }
                let axis = |a,b| match (a,b) {
                    (Some(a),Some(b)) => Some(Some(length(a,b,progress)?)),
                    (None,None) => Some(None),
                    _ => None,
                };
                values.push(BackgroundSize { kind: a.kind, width: axis(a.width,b.width)?, height: axis(a.height,b.height)? });
            }
            computed_values::background_sizes_css_value(Some(&values))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::css::{self,StyleIndex};
    fn style(source: &str) -> Style {
        let document = crate::html::parse(&alloc::format!("<div id=target style='{source}'></div>"),32).unwrap();
        let node = crate::selector::query_selector(&document,document.root(),"#target").unwrap().unwrap();
        css::compute_node(&document,node,None,&StyleIndex::new(Vec::new())).unwrap()
    }
    fn black(_:Rgba,_:Rgba,_:f32)->Rgba { Rgba{r:0,g:0,b:0,a:255} }
    #[test]
    fn specification_border_image_computed_animation_preserves_quad_types_fill_and_nonnegative_ranges() {
        let a=style("border-image-slice:10 20% fill;border-image-width:10px auto auto 20;border-image-outset:1 2px");
        let b=style("border-image-slice:110 120% fill;border-image-width:110px auto auto 120;border-image-outset:3 6px");
        assert_eq!(interpolate_compound_transition("border-image-slice",&a,&b,0.5,black).as_deref(),Some("60 70% fill"));
        assert_eq!(interpolate_compound_transition("border-image-width",&a,&b,0.5,black).as_deref(),Some("60px auto auto 70"));
        assert_eq!(interpolate_compound_transition("border-image-outset",&a,&b,0.5,black).as_deref(),Some("2 4px"));
        assert_eq!(interpolate_compound_transition("border-image-width",&a,&b,-0.3,black).as_deref(),Some("0px auto auto 0"));
        let (a,b)=(a.border_image.as_ref().unwrap(),b.border_image.as_ref().unwrap());
        assert_eq!(compose_border_image("border-image-slice",a,b).as_deref(),Some("120 140% fill"));
        assert_eq!(compose_border_image("border-image-width",a,b).as_deref(),Some("120px auto auto 140"));
        let mismatch=style("border-image-slice:10% 20 fill;border-image-width:10 auto auto 20");
        assert!(interpolate_border_image("border-image-slice",a,mismatch.border_image.as_ref().unwrap(),0.5).is_none());
        assert!(compose_border_image("border-image-width",a,mismatch.border_image.as_ref().unwrap()).is_none());
        let missing_fill=style("border-image-slice:10 20%");
        assert!(interpolate_border_image("border-image-slice",a,missing_fill.border_image.as_ref().unwrap(),0.5).is_none());
        let lengths=style("border-image-width:10px");let percentages=style("border-image-width:20%");
        let sample=interpolate_compound_transition("border-image-width",&lengths,&percentages,1.5,black).unwrap();
        let parsed=style(&alloc::format!("border-image-width:{sample}"));
        assert_eq!(parsed.border_image.as_ref().unwrap().width[0].used(0.0,120.0,0.0),31.0,"mixed length/percentage extrapolation is clamped at used value, not component-wise");
        assert_eq!(crate::css::transition_value_kind("border-image-source"),crate::css::TransitionValueKind::Discrete);
        assert_eq!(crate::css::transition_value_kind("border-image-repeat"),crate::css::TransitionValueKind::Discrete);
    }
    #[test]
    fn specification_text_shadow_typed_interpolation_and_nonadditive_composition() {
        let a=style("text-shadow:black 2px 4px 6px");let b=style("text-shadow:black 4px 8px 10px,red 20px 30px");
        let sample=interpolate_compound_transition("text-shadow",&a,&b,0.5,black).unwrap();
        let computed=style(&alloc::format!("text-shadow:{sample}"));let values=computed.text_shadows.as_ref().unwrap();
        assert_eq!((values[0].offset_x,values[0].offset_y,values[0].blur),(3.0,6.0,8.0));
        assert_eq!((values[1].offset_x,values[1].offset_y),(10.0,15.0));
        let add=compose_shadow_lists("text-shadow",a.text_shadows.as_deref().unwrap(),b.text_shadows.as_deref().unwrap(),false,|a,_|a).unwrap();
        assert_eq!(Some(add),serialize_shadow_list("text-shadow",b.text_shadows.as_deref().unwrap()),"nonadditive addition returns V_B");
        let sum=compose_shadow_lists("text-shadow",a.text_shadows.as_deref().unwrap(),b.text_shadows.as_deref().unwrap(),true,|a,_|a).unwrap();
        assert_eq!(Some(sum),serialize_shadow_list("text-shadow",b.text_shadows.as_deref().unwrap()),"nonadditive accumulation returns V_B");
        let none=Style::initial();assert_eq!(interpolate_compound_transition("text-shadow",&none,&none,0.5,black).as_deref(),Some("none"));
        let inset=style("box-shadow:inset black 1px 2px");let outer=style("box-shadow:black 2px 4px");
        assert!(interpolate_compound_transition("box-shadow",&inset,&outer,0.5,black).is_none());
        assert_eq!(compose_shadow_lists("box-shadow",inset.shadows.as_deref().unwrap(),outer.shadows.as_deref().unwrap(),true,|a,_|a),serialize_shadow_list("box-shadow",outer.shadows.as_deref().unwrap()));
    }

    #[test]
    fn specification_transition_decoration_lengths_preserve_font_relative_math() {
        let a=style("text-decoration-thickness:calc(25% + 3px);text-underline-offset:min(100%, 12px)");
        let b=style("text-decoration-thickness:calc(75% + 13px);text-underline-offset:calc(30% + 8px)");
        let thickness=interpolate_compound_transition("text-decoration-thickness",&a,&b,0.5,black).unwrap();
        let offset=interpolate_compound_transition("text-underline-offset",&a,&b,0.5,black).unwrap();
        let sampled=style(&alloc::format!("text-decoration-thickness:{thickness};text-underline-offset:{offset}"));
        assert_eq!(sampled.text_decoration_thickness.used(20.0),Some(18.0));
        assert_eq!(sampled.text_decoration_thickness.used(40.0),Some(28.0));
        assert_eq!(sampled.text_underline_offset.used(20.0),Some(13.0));
        assert_eq!(sampled.text_underline_offset.used(0.0),Some(4.0));
        assert!(interpolate_compound_transition("text-underline-offset",&a,&style("text-underline-offset:auto"),0.5,black).is_none());
    }
    #[test]
    fn specification_transition_inline_lengths_keep_independent_percentage_components() {
        let a=style("text-indent:calc(10% + 4px);word-spacing:calc(20% + 2px)");
        let b=style("text-indent:calc(30% + 12px);word-spacing:calc(60% + 6px)");
        let indent=interpolate_compound_transition("text-indent",&a,&b,0.5,black).unwrap();
        let spacing=interpolate_compound_transition("word-spacing",&a,&b,0.5,black).unwrap();
        let sampled=style(&alloc::format!("text-indent:{indent};word-spacing:{spacing}"));
        assert!((sampled.text_indent.fraction-0.2).abs()<0.0001);
        assert_eq!(sampled.text_indent.pixels,8.0);
        assert!((sampled.word_spacing.fraction-0.4).abs()<0.0001);
        assert_eq!(sampled.word_spacing.pixels,4.0);
        let relative=style("font-size:20px;word-spacing:calc(25% + 2em)");
        assert_eq!(relative.word_spacing.pixels,40.0,"font-dependent spacing resolves its length component against the final font");
        assert_eq!(relative.word_spacing.fraction,0.25,"font resolution retains the independent space-glyph percentage component");
        let negative=style("word-spacing:calc(-25% - 2px)");
        assert_eq!(negative.word_spacing.pixels,-2.0);
        assert_eq!(negative.word_spacing.fraction,-0.25);
    }
    #[test]
    fn specification_transition_compound_shadow_padding_and_pairwise_incompatibility() {
        let a=style("box-shadow:2px 4px 6px 8px black");
        let b=style("box-shadow:6px 8px 10px 12px black,4px 8px 12px 16px black inset");
        let result=interpolate_compound_transition("box-shadow",&a,&b,0.5,black).unwrap();
        let parsed=style(&alloc::format!("box-shadow:{result}"));
        let values=parsed.shadows.as_deref().unwrap();
        assert_eq!((values[0].offset_x,values[0].blur,values[0].spread),(4.0,8.0,10.0));
        assert_eq!((values[1].offset_x,values[1].blur,values[1].inset),(2.0,6.0,true));
        assert!(interpolate_compound_transition("box-shadow",&a,&style("box-shadow:2px 4px black inset"),0.5,black).is_none());
    }
    #[test]
    fn specification_transition_compound_repeatable_lists_keep_percentages_and_auto() {
        let a=style("background-position:0% 0%,100% 20px;background-size:10px auto,20px 40px");
        let b=style("background-position:100% 10px,0% 20px,50% 30px;background-size:30px auto,40px 60px");
        let result=interpolate_compound_transition("background-position",&a,&b,0.5,black).unwrap();
        let parsed=style(&alloc::format!("background-position:{result}"));
        let values=parsed.background_position.as_deref().unwrap();
        assert_eq!(values.len(),6);
        assert_eq!((values[0][0].fraction,values[0][1].pixels),(0.5,5.0));
        assert_eq!((values[5][0].fraction,values[5][1].pixels),(0.75,25.0));
        let eight=(0..8).map(|index|alloc::format!("{}% 0px",index*10)).collect::<Vec<_>>().join(",");
        let seven=(0..7).map(|index|alloc::format!("{}% 10px",index*10)).collect::<Vec<_>>().join(",");
        let long=interpolate_compound_transition("background-position",&style(&alloc::format!("background-position:{eight}")),
            &style(&alloc::format!("background-position:{seven}")),0.5,black).unwrap();
        assert_eq!(style(&alloc::format!("background-position:{long}")).background_position.as_deref().unwrap().len(),56,
            "computed geometry list LCM is independent of the image/render layer budget");
        let eight_sizes=(0..8).map(|index|alloc::format!("{}px auto",10+index)).collect::<Vec<_>>().join(",");
        let seven_sizes=(0..7).map(|index|alloc::format!("{}px auto",30+index)).collect::<Vec<_>>().join(",");
        let long_sizes=interpolate_compound_transition("background-size",&style(&alloc::format!("background-size:{eight_sizes}")),
            &style(&alloc::format!("background-size:{seven_sizes}")),0.5,black).unwrap();
        let parsed_sizes=style(&alloc::format!("background-size:{long_sizes}"));
        let values=parsed_sizes.background_size.as_deref().unwrap();
        assert_eq!(values.len(),56);
        assert_eq!(values[0].width.unwrap().pixels,20.0);
        assert!(values.iter().all(|value|value.height.is_none()));
        assert!(!css::supports_declaration("background-image",&["none";9].join(",")),"image allocation remains capped independently of repeatable geometry");
        let size=interpolate_compound_transition("background-size",&a,&b,0.5,black).unwrap();
        let parsed=style(&alloc::format!("background-size:{size}"));
        let values=parsed.background_size.as_deref().unwrap();
        assert_eq!(values[0].width.unwrap().pixels,20.0);assert!(values[0].height.is_none());
        assert!(interpolate_compound_transition("background-size",&a,&style("background-size:cover"),0.5,black).is_none());
    }
}
