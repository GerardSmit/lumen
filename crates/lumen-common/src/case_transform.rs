//! Source-aware Unicode case transformation. Context always comes from the
//! original inline text, never from an already transformed prefix.
use crate::ucd::{self, flag};
use std::{borrow::Cow, ops::Range};

const ACCENT:u8=1;
const DIAERESIS:u8=2;
const SUBSCRIPT:u8=4;
const OTHER_GREEK_MARK:u8=8;
fn greek_mark(c:char)->u8 {
    match c {
        '\u{300}'|'\u{301}'|'\u{302}'|'\u{303}'|'\u{311}'|'\u{342}'=>ACCENT,
        '\u{308}'=>DIAERESIS,'\u{344}'=>ACCENT|DIAERESIS,'\u{345}'=>SUBSCRIPT,
        '\u{304}'|'\u{306}'|'\u{313}'|'\u{314}'|'\u{343}'=>OTHER_GREEK_MARK,_=>0,
    }
}
#[derive(Clone,Copy,Default)]
struct GreekLetter { upper:Option<char>, marks:u8, subscripts:usize }
fn greek_letter(c:char)->Option<GreekLetter> {
    if !matches!(c as u32,0x370..=0x3ff|0x1f00..=0x1fff|0x2126) || !cased(c){return None;}
    fn decompose(cp:u32,letter:&mut GreekLetter) {
        let mut mapping=ucd::canonical_mapping(cp,ucd::Version::Current).peekable();
        if mapping.peek().is_some(){for cp in mapping {decompose(cp,letter);}return;}
        let Some(c)=char::from_u32(cp) else{return;};
        let mark=greek_mark(c);
        if mark!=0 {letter.marks|=mark;letter.subscripts+=usize::from(mark&SUBSCRIPT!=0);}
        else if letter.upper.is_none() {letter.upper=ucd::char_type(cp).upper().next().and_then(char::from_u32);}
    }
    let mut result=GreekLetter::default();decompose(c as u32,&mut result);
    result.upper.filter(|c|matches!(*c as u32,0x370..=0x3ff))?;
    Some(result)
}
fn greek_vowel(c:char)->bool {matches!(c,'Α'|'Ε'|'Η'|'Ι'|'Ο'|'Υ'|'Ω')}
#[derive(Clone,Copy,Default)]
struct GreekCluster {
    end:usize, letter:GreekLetter, gain_diaeresis:bool,
    precomposed_gain:bool, disjunctive_eta:bool, first_accent:Option<usize>,
}
#[derive(Default)]
struct GreekContext { offset:usize, after_cased:bool, accented_vowel:bool, precomposed_accent:bool, cluster:Option<GreekCluster> }
impl GreekContext {
    fn advance(&mut self,text:&str,at:usize)->Option<GreekCluster> {
        if at<self.offset {*self=Self::default();}
        while self.offset<=at && self.offset<text.len() {
            let start=self.offset;let c=text[start..].chars().next()?;self.offset+=c.len_utf8();
            if self.cluster.is_some_and(|cluster|start<cluster.end){continue;}
            self.cluster=None;
            if let Some(letter)=greek_letter(c) {
                let upper=letter.upper?;let mut marks=letter.marks;let mut end=self.offset;let mut first_accent=None;
                for mark in text[end..].chars() {
                    let data=greek_mark(mark);if data==0 {break;}
                    if first_accent.is_none() && data&ACCENT!=0 {first_accent=Some(end);}
                    marks|=data;end+=mark.len_utf8();
                }
                let gain_diaeresis=self.accented_vowel && matches!(upper,'Ι'|'Υ');
                if gain_diaeresis {marks|=DIAERESIS;}
                let disjunctive_eta=upper=='Η' && marks&ACCENT!=0 && marks&SUBSCRIPT==0 && !self.after_cased
                    && !text[end..].chars().find(|&c|!ignorable(c)).is_some_and(cased);
                self.cluster=Some(GreekCluster {end,letter,gain_diaeresis,precomposed_gain:gain_diaeresis&&self.precomposed_accent,disjunctive_eta,first_accent});
                self.accented_vowel=greek_vowel(upper) && marks&(ACCENT|DIAERESIS)==ACCENT;
                self.precomposed_accent=letter.marks&ACCENT!=0;
                self.after_cased=true;
            } else {
                self.accented_vowel=false;self.precomposed_accent=false;
                if !ignorable(c){self.after_cased=cased(c);}
            }
        }
        self.cluster.filter(|cluster|at<cluster.end)
    }
}
fn word_initial((start,value):(usize,&str))->Option<usize> {
    value.char_indices().find(|(_,c)|ucd::char_type(*c as u32).is(flag::ALPHA)).map(|(offset,_)|start+offset)
}
type WordStarts<'a>=std::iter::Peekable<std::iter::FilterMap<unicode_segmentation::UWordBoundIndices<'a>,fn((usize,&str))->Option<usize>>>;
/// Transient original-text context reused across ordered inline fragments.
/// Word segmentation and Greek context each advance once; no per-node cache.
pub struct CaseContext<'a> { text:&'a str, starts:Option<WordStarts<'a>>, greek:GreekContext }
impl<'a> CaseContext<'a> {
    pub fn new(text:&'a str)->Self {Self {text,starts:None,greek:GreekContext::default()}}
    fn initial(&mut self,at:usize)->bool {
        let starts=self.starts.get_or_insert_with(||ucd::word_boundaries(self.text).filter_map(word_initial as fn((usize,&str))->Option<usize>).peekable());
        while starts.peek().is_some_and(|&start|start<at){starts.next();}
        starts.peek()==Some(&at)
    }
    pub fn visit<E>(&mut self,range:Range<usize>,mode:CaseTransform,language:Option<&str>,emit:impl FnMut(char,Range<usize>)->Result<(),E>)->Result<(),E> {
        self.visit_source(range,mode,language,self.text.chars().take(2).count()==1,emit)
    }
    /// Math-auto eligibility belongs to the original Text node, not a merged
    /// span, selected substring, or already collapsed paragraph.
    pub fn visit_source<E>(&mut self,range:Range<usize>,mode:CaseTransform,language:Option<&str>,single_character:bool,mut emit:impl FnMut(char,Range<usize>)->Result<(),E>)->Result<(),E> {
        if mode==CaseTransform::MathAuto {
            for (offset,c) in self.text[range.clone()].char_indices() {
                let mapped=if single_character {ucd::math_italic(c as u32).and_then(char::from_u32).unwrap_or(c)}else {c};
                emit(mapped,range.start+offset..range.start+offset+c.len_utf8())?;
            }
            return Ok(());
        }
        visit_case_range_with_context(self,range,mode.case(),language,|c,source| {
            mode.visit_display_character(c,|mapped|emit(mapped,source.clone()))
        })
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(u8)]
pub enum CaseTransform {
    #[default] None=0, Uppercase=1, Lowercase=2, Capitalize=3,
    FullWidth=4, UppercaseFullWidth=5, LowercaseFullWidth=6, CapitalizeFullWidth=7,
    FullSizeKana=8, UppercaseFullSizeKana=9, LowercaseFullSizeKana=10, CapitalizeFullSizeKana=11,
    FullWidthFullSizeKana=12, UppercaseFullWidthFullSizeKana=13, LowercaseFullWidthFullSizeKana=14, CapitalizeFullWidthFullSizeKana=15,
    MathAuto=16,
}
impl CaseTransform {
    pub fn parse(value: &str) -> Option<Self> {
        if value.trim_ascii()=="none" {return Some(Self::None);}
        let mut flags=0;let mut any=false;
        for value in value.split_ascii_whitespace() {
            any=true;
            let bit=match value {"uppercase"=>1,"lowercase"=>2,"capitalize"=>3,"full-width"=>4,"full-size-kana"=>8,"math-auto"=>16,_=>return None};
            if bit<=3 {if flags&3!=0{return None;}}else if flags&bit!=0 {return None;}
            flags|=bit;
        }
        if !any {return None;}
        Some(match flags {1=>Self::Uppercase,2=>Self::Lowercase,3=>Self::Capitalize,4=>Self::FullWidth,5=>Self::UppercaseFullWidth,6=>Self::LowercaseFullWidth,7=>Self::CapitalizeFullWidth,8=>Self::FullSizeKana,9=>Self::UppercaseFullSizeKana,10=>Self::LowercaseFullSizeKana,11=>Self::CapitalizeFullSizeKana,12=>Self::FullWidthFullSizeKana,13=>Self::UppercaseFullWidthFullSizeKana,14=>Self::LowercaseFullWidthFullSizeKana,15=>Self::CapitalizeFullWidthFullSizeKana,16=>Self::MathAuto,_=>return None})
    }
    pub fn as_str(self) -> &'static str { match self {Self::None=>"none",Self::Uppercase=>"uppercase",Self::Lowercase=>"lowercase",Self::Capitalize=>"capitalize",Self::FullWidth=>"full-width",Self::UppercaseFullWidth=>"uppercase full-width",Self::LowercaseFullWidth=>"lowercase full-width",Self::CapitalizeFullWidth=>"capitalize full-width",Self::FullSizeKana=>"full-size-kana",Self::UppercaseFullSizeKana=>"uppercase full-size-kana",Self::LowercaseFullSizeKana=>"lowercase full-size-kana",Self::CapitalizeFullSizeKana=>"capitalize full-size-kana",Self::FullWidthFullSizeKana=>"full-width full-size-kana",Self::UppercaseFullWidthFullSizeKana=>"uppercase full-width full-size-kana",Self::LowercaseFullWidthFullSizeKana=>"lowercase full-width full-size-kana",Self::CapitalizeFullWidthFullSizeKana=>"capitalize full-width full-size-kana",Self::MathAuto=>"math-auto"} }
    fn case(self)->Self {match (self as u8)&3 {1=>Self::Uppercase,2=>Self::Lowercase,3=>Self::Capitalize,_=>Self::None}}
    pub fn full_width(self)->bool {(self as u8)&4!=0}
    pub fn full_size_kana(self)->bool {(self as u8)&8!=0}
    pub fn needs_case_context(self)->bool {(self as u8)&3!=0}
    /// Case conversion retains source context; width and kana conversion follow
    /// the caller's CSS white-space processing.
    pub fn before_display(self)->Self {if self==Self::MathAuto {self}else{self.case()}}
    pub fn display_only(self)->Self {match (self as u8)&12 {4=>Self::FullWidth,8=>Self::FullSizeKana,12=>Self::FullWidthFullSizeKana,_=>Self::None}}
    pub fn visit_display_character<E>(self,c:char,mut emit:impl FnMut(char)->Result<(),E>)->Result<(),E> {
        let mut output=|c|emit(if self.full_size_kana(){full_size_kana(c)}else{c});
        if self.full_width() {
            let mut narrow=ucd::tagged_mapping(c as u32,ucd::DecompositionKind::Narrow).peekable();
            if narrow.peek().is_some() {
                for cp in narrow {if let Some(c)=char::from_u32(cp){output(c)?;}}
                return Ok(());
            }
            output(ucd::full_width_inverse(c as u32).and_then(char::from_u32).unwrap_or(c))
        }else{output(c)}
    }
}

/// Complete normative CSS Text Appendix G mapping; width conversion precedes
/// this operation, while full-size-kana alone preserves half-width forms.
fn full_size_kana(c:char)->char {
    let cp=match c as u32 {
        0x3041|0x3043|0x3045|0x3047|0x3049|0x3063|0x3083|0x3085|0x3087|0x308e|
        0x30a1|0x30a3|0x30a5|0x30a7|0x30a9|0x30c3|0x30e3|0x30e5|0x30e7|0x30ee=>(c as u32)+1,
        0x3095=>0x304b,0x3096=>0x3051,0x1b132=>0x3053,0x1b150=>0x3090,0x1b151=>0x3091,0x1b152=>0x3092,
        0x30f5=>0x30ab,0x30f6=>0x30b1,0x1b155=>0x30b3,
        0x31f0=>0x30af,0x31f1=>0x30b7,0x31f2=>0x30b9,0x31f3=>0x30c8,0x31f4=>0x30cc,
        0x31f5=>0x30cf,0x31f6=>0x30d2,0x31f7=>0x30d5,0x31f8=>0x30d8,0x31f9=>0x30db,0x31fa=>0x30e0,
        0x31fb..=0x31ff=>0x30e9+(c as u32-0x31fb),0x1b164..=0x1b167=>0x30f0+(c as u32-0x1b164),
        0xff67..=0xff6b=>0xff71+(c as u32-0xff67),0xff6c..=0xff6e=>0xff94+(c as u32-0xff6c),0xff6f=>0xff82,
        _=>return c,
    };char::from_u32(cp).expect("normative kana scalar")
}

fn cased(c:char) -> bool { ucd::char_type(c as u32).is(flag::CASED) }
fn ignorable(c:char) -> bool { ucd::char_type(c as u32).is(flag::CASE_IGNORABLE) }
fn combining(c:char) -> u8 { ucd::props(c as u32, ucd::Version::Current).combining }
fn soft_dotted(c:char) -> bool {
    let Some(ranges)=crate::unicode_props::lookup("Soft_Dotted",None) else {return false;};
    let cp=c as u32;let index=ranges.partition_point(|&(_,last)|last<cp);
    ranges.get(index).is_some_and(|&(first,_)|first<=cp)
}
fn language_is(language:Option<&str>, expected:&str) -> bool {
    language.and_then(|value| value.split('-').next()).is_some_and(|value| value.eq_ignore_ascii_case(expected))
}
fn final_sigma(text:&str, at:usize, end:usize) -> bool {
    text[..at].chars().rev().find(|&c| !ignorable(c)).is_some_and(cased)
        && !text[end..].chars().find(|&c| !ignorable(c)).is_some_and(cased)
}
fn after(text:&str, at:usize, expected:char, stop_above:bool) -> bool {
    for c in text[..at].chars().rev() {
        if c == expected { return true; }
        let class=combining(c);
        if class==0 || stop_above && class==230 { break; }
    }
    false
}
fn before_dot(text:&str,end:usize) -> bool {
    for c in text[end..].chars() {
        if c=='\u{307}' { return true; }
        if matches!(combining(c),0|230) { break; }
    }
    false
}
fn more_above(text:&str,end:usize) -> bool {
    for c in text[end..].chars() {
        match combining(c) {230=>return true,0=>break,_=>{}}
    }
    false
}
/// Visit transformed scalars with their original UTF-8 source range. Expansion
/// scalars share that range; deleted combining marks emit no scalar. Callers
/// can retain a sparse map only where their selection/hit API needs one.
pub fn visit_case_range<E>(text:&str, range:Range<usize>, mode:CaseTransform,
    language:Option<&str>, emit:impl FnMut(char,Range<usize>)->Result<(),E>) -> Result<(),E> {
    CaseContext::new(text).visit(range,mode,language,emit)
}
fn visit_case_range_with_context<E>(context:&mut CaseContext<'_>,range:Range<usize>,mode:CaseTransform,
    language:Option<&str>,mut emit:impl FnMut(char,Range<usize>)->Result<(),E>)->Result<(),E> {
    let text=context.text;
    if mode==CaseTransform::None {
        for (offset,c) in text[range.clone()].char_indices() {emit(c,range.start+offset..range.start+offset+c.len_utf8())?;}
        return Ok(());
    }
    let turkic=language_is(language,"tr") || language_is(language,"az");
    let lithuanian=language_is(language,"lt");
    let dutch=mode==CaseTransform::Capitalize && language_is(language,"nl");
    let greek=mode==CaseTransform::Uppercase && language_is(language,"el");
    let mut dutch_i=dutch && range.start>0 && matches!(text.as_bytes().get(range.start-1),Some(b'i'|b'I'))
        && context.initial(range.start-1);
    for (local,c) in text[range.clone()].char_indices() {
        let at=range.start+local;
        let end=at+c.len_utf8();
        let source=at..end;
        let initial=mode==CaseTransform::Capitalize && context.initial(at);
        let title=mode==CaseTransform::Capitalize && (initial || dutch && dutch_i && matches!(c,'j'|'J'));
        dutch_i=initial && matches!(c,'i'|'I');
        let lower=mode==CaseTransform::Lowercase;
        let upper=mode==CaseTransform::Uppercase || title;
        if greek {
            if let Some(cluster)=context.greek.advance(text,at) {
                let mark=greek_mark(c);
                if mark!=0 {
                    if mark&SUBSCRIPT!=0 {emit('Ι',source.clone())?;}
                    if mark&DIAERESIS!=0 {emit('\u{308}',source.clone())?;}
                    if cluster.disjunctive_eta && cluster.letter.marks&ACCENT==0 && cluster.first_accent==Some(at) {emit('\u{301}',source)?;}
                    continue;
                }
                let mut upper=cluster.letter.upper.expect("Greek letter has an uppercase base");
                let composed_diaeresis=cluster.letter.marks&DIAERESIS!=0 || cluster.precomposed_gain;
                if cluster.disjunctive_eta && cluster.letter.marks&ACCENT!=0 {upper='Ή';}
                else if composed_diaeresis {upper=match upper {'Ι'=>'Ϊ','Υ'=>'Ϋ',_=>upper};}
                emit(upper,source.clone())?;
                if cluster.gain_diaeresis && !composed_diaeresis {emit('\u{308}',source.clone())?;}
                for _ in 0..cluster.letter.subscripts {emit('Ι',source.clone())?;}
                continue;
            }
        }
        if lower && c=='Σ' && final_sigma(text,at,end) { emit('ς',source)?; continue; }
        if turkic {
            if upper && c=='i' { emit('İ',source)?; continue; }
            if lower && c=='İ' { emit('i',source)?; continue; }
            if lower && c=='I' && !before_dot(text,end) { emit('ı',source)?; continue; }
            if lower && c=='\u{307}' && after(text,at,'I',true) { continue; }
        }
        if lithuanian {
            if upper && c=='\u{307}' && text[..at].chars().rev().find(|&c| matches!(combining(c),0|230))
                .is_some_and(soft_dotted) { continue; }
            if lower && matches!(c,'I'|'J'|'Į') && more_above(text,end) {
                for cp in ucd::char_type(c as u32).lower() { if let Some(c)=char::from_u32(cp) {emit(c,source.clone())?;} }
                emit('\u{307}',source)?; continue;
            }
            if lower && matches!(c,'Ì'|'Í'|'Ĩ') {
                emit('i',source.clone())?;emit('\u{307}',source.clone())?;
                emit(match c {'Ì'=>'\u{300}','Í'=>'\u{301}',_=>'\u{303}'},source)?;continue;
            }
        }
        let kind=ucd::char_type(c as u32);
        if upper || lower {
            let mapping=if title {kind.title()} else if upper {kind.upper()} else {kind.lower()};
            for cp in mapping {if let Some(c)=char::from_u32(cp) {emit(c,source.clone())?;} }
        } else {emit(c,source)?;}
    }
    Ok(())
}

/// Bounded transformation with an allocation-free `none`/unchanged result.
pub fn transform_case_range<'a>(text:&'a str, range:Range<usize>, mode:CaseTransform,
    language:Option<&str>) -> Result<Cow<'a,str>, &'static str> {
    transform_case_source_range(text,range,mode,language,text.chars().take(2).count()==1)
}
pub fn transform_case_source_range<'a>(text:&'a str, range:Range<usize>, mode:CaseTransform,
    language:Option<&str>,single_character:bool) -> Result<Cow<'a,str>, &'static str> {
    let value=text.get(range.clone()).ok_or("invalid text source range")?;
    if mode==CaseTransform::None {return Ok(Cow::Borrowed(value));}
    let mut result:Option<String>=None;
    let mut expected=value.chars();
    let mut matched=0;
    CaseContext::new(text).visit_source(range,mode,language,single_character,|c,_| {
        if result.is_none() {
            if expected.next()==Some(c) {matched+=c.len_utf8();return Ok(());}
            if matched>crate::bidi::MAX_TEXT_BYTES {return Err("transformed text limit exceeded");}
            let mut output=String::new();
            output.try_reserve(matched).map_err(|_|"text allocation failed")?;
            output.push_str(&value[..matched]);result=Some(output);
        }
        let output=result.as_mut().expect("case output initialized");
        if output.len().saturating_add(c.len_utf8())>crate::bidi::MAX_TEXT_BYTES {return Err("transformed text limit exceeded");}
        output.try_reserve(c.len_utf8()).map_err(|_|"text allocation failed")?;
        output.push(c);Ok(())
    })?;
    if let Some(result)=result {Ok(Cow::Owned(result))}
    else if matched==value.len() {Ok(Cow::Borrowed(value))}
    else {
        if matched>crate::bidi::MAX_TEXT_BYTES {return Err("transformed text limit exceeded");}
        let mut output=String::new();output.try_reserve(matched).map_err(|_|"text allocation failed")?;
        output.push_str(&value[..matched]);Ok(Cow::Owned(output))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn specification_display_transforms_share_pinned_width_kana_math_and_source_ranges() {
        assert_eq!(std::mem::size_of::<CaseTransform>(),1);
        let transform=|text:&str,value:&str|transform_case_range(text,0..text.len(),CaseTransform::parse(value).unwrap(),None).unwrap().into_owned();
        assert_eq!(transform("aß ｧｶﾞ ①ﬀ","full-size-kana full-width uppercase"),"ＡＳＳ\u{3000}アカ\u{3099}\u{3000}①ＦＦ");
        assert_eq!(transform("ぁゕゖ\u{1b132}\u{1b150}\u{1b151}\u{1b152}ㇰㇿ\u{1b155}\u{1b164}\u{1b165}\u{1b166}\u{1b167}ｧｯｬｭｮ","full-size-kana"),"あかけこゐゑをクロコヰヱヲンｱﾂﾔﾕﾖ");
        // CSS Text's complete normative Appendix G, including Kana Extended-B.
        assert_eq!(transform("ぁぃぅぇぉゕゖ\u{1b132}っゃゅょゎ\u{1b150}\u{1b151}\u{1b152}ァィゥェォヵㇰヶ\u{1b155}ㇱㇲッㇳㇴㇵㇶㇷㇸㇹㇺャュョㇻㇼㇽㇾㇿヮ\u{1b164}\u{1b165}\u{1b166}\u{1b167}ｧｨｩｪｫｯｬｭｮ","full-size-kana"),
            "あいうえおかけこつやゆよわゐゑをアイウエオカクケコシスツトヌハヒフヘホムヤユヨラリルレロワヰヱヲンｱｲｳｴｵﾂﾔﾕﾖ");
        assert_eq!(transform("h","math-auto"),"ℎ");
        assert_eq!(transform("∂","math-auto"),"\u{1d715}");
        assert_eq!(transform("ab","math-auto"),"ab");
        assert_eq!(transform("1","math-auto"),"1");
        for value in ["none uppercase","math-auto full-width","math-auto math-auto","full-width full-width","capitalize lowercase","none\u{3000}"] {assert!(CaseTransform::parse(value).is_none(),"{value}");}
        assert_eq!(CaseTransform::parse("full-size-kana uppercase full-width").unwrap().as_str(),"uppercase full-width full-size-kana");
        let mut context=CaseContext::new("ab");let mut mapped=Vec::new();
        context.visit_source(0..1,CaseTransform::MathAuto,None,true,|c,range|{mapped.push((c,range));Ok::<_,()>(())}).unwrap();
        context.visit_source(1..2,CaseTransform::MathAuto,None,true,|c,range|{mapped.push((c,range));Ok::<_,()>(())}).unwrap();
        assert_eq!(mapped,vec![('\u{1d44e}',0..1),('\u{1d44f}',1..2)]);
        assert_eq!(transform_case_source_range("ab",0..1,CaseTransform::MathAuto,None,false).unwrap(),"a","selected single character cannot change a multi-character Text node");
        for cp in 0..0x110000 {
            let Some(c)=char::from_u32(cp) else {continue;};
            let mut wide=ucd::tagged_mapping(cp,ucd::DecompositionKind::Wide);
            if let Some(original)=wide.next(){assert!(wide.next().is_none());assert_eq!(ucd::full_width_inverse(original),Some(cp));}
            let mut narrow=ucd::tagged_mapping(cp,ucd::DecompositionKind::Narrow);
            if let Some(expected)=narrow.next(){assert!(narrow.next().is_none());let value=c.to_string();assert_eq!(transform(&value,"full-width"),char::from_u32(expected).unwrap().to_string());}
        }
    }
    #[test]
    fn contextual_greek_uppercase_reuses_original_ranges_and_pinned_decomposition() {
        let map=|text:&str,language|transform_case_range(text,0..text.len(),CaseTransform::Uppercase,language).unwrap().into_owned();
        assert_eq!(map("καλημέρα αύριο",Some("el-Grek")),"ΚΑΛΗΜΕΡΑ ΑΥΡΙΟ");
        assert_eq!(map("ευφυΐα Νεράιδα",Some("el")),"ΕΥΦΥΪΑ ΝΕΡΑΪΔΑ");
        assert_eq!(map("ήσουν ή εγώ ή εσύ",Some("el")),"ΗΣΟΥΝ Ή ΕΓΩ Ή ΕΣΥ");
        assert_eq!(map("Ι\u{308}\u{301}Ρ",Some("el")),"Ι\u{308}Ρ");
        assert_eq!(map("η\u{301}",Some("el")),"Η\u{301}");
        assert_eq!(map("ᾄ ῥ ῶ",Some("el")),"ΑΙ Ρ Ω");
        assert_eq!(map("καλημέρα",None),"ΚΑΛΗΜΈΡΑ");
        assert_eq!(transform_case_range("καλημέρα",0.."καλημέρα".len(),CaseTransform::Capitalize,Some("el")).unwrap(),"Καλημέρα");
        let text="Νεράιδα ή Ι\u{308}\u{301}Ρ";
        let mut context=CaseContext::new(text);let mut result=String::new();let mut ranges=Vec::new();
        for (offset,c) in text.char_indices() {
            context.visit(offset..offset+c.len_utf8(),CaseTransform::Uppercase,Some("el"),|c,range| {result.push(c);ranges.push(range);Ok::<_,()>(())}).unwrap();
        }
        assert_eq!(result,map(text,Some("el")),"style boundaries preserve contextual mapping");
        assert!(ranges.iter().all(|range|text.get(range.clone()).is_some()));
        let text="ijsland foo-bar";let mut context=CaseContext::new(text);let mut result=String::new();
        for (offset,c) in text.char_indices() {context.visit(offset..offset+c.len_utf8(),CaseTransform::Capitalize,Some("nl"),|c,_|{result.push(c);Ok::<_,()>(())}).unwrap();}
        assert_eq!(result,"IJsland Foo-Bar","one word cursor spans all fragments");
    }
    #[test]
    fn full_case_mapping_uses_original_context_and_language_without_source_mutation() {
        let map=|text:&str,mode,language| transform_case_range(text,0..text.len(),mode,language).unwrap().into_owned();
        assert_eq!(map("Maß ﬃ",CaseTransform::Uppercase,None),"MASS FFI");
        assert_eq!(map("ΟΣ ΟΣΑ ΟΣ'",CaseTransform::Lowercase,None),"ος οσα ος'");
        assert_eq!(map("I İ i ı I\u{307}",CaseTransform::Lowercase,Some("tr-Latn")),"ı i i ı i");
        assert_eq!(map("i ı",CaseTransform::Uppercase,Some("az")),"İ I");
        assert_eq!(map("I\u{301} Í",CaseTransform::Lowercase,Some("lt")),"i\u{307}\u{301} i\u{307}\u{301}");
        assert_eq!(map("i\u{307}\u{301}",CaseTransform::Uppercase,Some("lt")),"I\u{301}");
        assert_eq!(map("ßeta john's foo_bar foo-bar",CaseTransform::Capitalize,None),"Sseta John's Foo_bar Foo-Bar");
        assert_eq!(map("ijsland",CaseTransform::Capitalize,Some("nl")),"IJsland");
        assert_eq!(transform_case_range("abc",1..2,CaseTransform::Capitalize,None).unwrap(),"b");
        assert_eq!(transform_case_range("ijsland",1..2,CaseTransform::Capitalize,Some("nl")).unwrap(),"J");
        let text="ß";
        let mut source=Vec::new();
        visit_case_range(text,0..text.len(),CaseTransform::Uppercase,None,|c,range| {source.push((c,range));Ok::<_,()>(())}).unwrap();
        assert_eq!(source,vec![('S',0..2),('S',0..2)]);
        assert_eq!(text,"ß");
        assert!(matches!(transform_case_range("abc",0..3,CaseTransform::None,None).unwrap(),Cow::Borrowed(_)));
    }
}
