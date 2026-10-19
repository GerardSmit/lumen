//! HTML's rendered text collection. Required breaks stay pending so adjoining
//! block boundaries collapse without retaining a string or map for every node.
use alloc::string::String;
use alloc::vec::Vec;
use core::ops::Range;
use crate::{css::{Display, Style, WhiteSpace}, layout::LayoutError,
    session::RenderSession, Document, Namespace, NodeId, NodeKind};

#[derive(Default)]
struct Text {
    value: String,
    required_breaks: u8,
    space: bool,
    space_character: char,
    line_has_content: bool,
    synthetic_controls_depth: usize,
}

#[derive(Default)]
pub struct CaseContext { text:String, ranges:Vec<(NodeId,Range<usize>)> }
impl CaseContext {
    fn ensure(context: &mut Option<Self>, session: &mut RenderSession, node: NodeId) -> Result<(), LayoutError> {
        if context.as_ref().is_some_and(|context| context.ranges.binary_search_by_key(&node.key(), |(id, _)| id.key()).is_ok()) { return Ok(()); }
        let mut root = session.document().flat_tree_parent(node).map_err(|_| LayoutError::InvalidTree)?.unwrap_or(node);
        for _ in 0..512 {
            if !matches!(session.document().kind(root), Ok(NodeKind::Element { .. })) { break; }
            let style = session.computed_style(root)?;
            if !style.is_inline_level() || style.is_atomic_inline() { break; }
            let Some(parent) = session.document().flat_tree_parent(root).map_err(|_| LayoutError::InvalidTree)? else { break; };
            if !matches!(session.document().kind(parent), Ok(NodeKind::Element { .. })) { break; }
            root = parent;
        }
        let mut gathered = Self::default();
        gathered.gather(session, root, 0)?;
        gathered.ranges.sort_unstable_by_key(|(id, _)| id.key());
        *context = Some(gathered);
        Ok(())
    }
    fn push(&mut self,value:&str) -> Result<(),LayoutError> {
        if self.text.len().saturating_add(value.len())>lumen_common::bidi::MAX_TEXT_BYTES {return Err(LayoutError::Text);}
        self.text.try_reserve(value.len()).map_err(|_|LayoutError::CommandLimit)?;
        self.text.push_str(value);Ok(())
    }
    fn gather(&mut self,session:&mut RenderSession,node:NodeId,depth:usize) -> Result<(),LayoutError> {
        if depth>512 {return Err(LayoutError::DepthLimit);}
        if let NodeKind::Text(value)|NodeKind::CData(value)=session.document().kind(node).map_err(|_|LayoutError::InvalidTree)? {
            let start=self.text.len();self.push(value)?;
            self.ranges.try_reserve(1).map_err(|_|LayoutError::CommandLimit)?;
            self.ranges.push((node,start..self.text.len()));return Ok(());
        }
        if !matches!(session.document().kind(node),Ok(NodeKind::Element{..})) {return Ok(());}
        let style=session.computed_style(node)?;
        if style.display==Display::None || style.display.is_table_column() {return Ok(());}
        if depth>0 && (matches!(style.position,crate::css::Position::Absolute|crate::css::Position::Fixed) || style.float != crate::css::Float::None) {return Ok(());}
        let (replace,br)=matches!(session.document().kind(node),Ok(NodeKind::Element{name,namespace:Namespace::Html,..}) if replaced(name))
            .then_some((true,false)).unwrap_or_else(||(false,matches!(session.document().kind(node),Ok(NodeKind::Element{name,namespace:Namespace::Html,..}) if name=="br")));
        if replace {self.push("\u{fffc}")?;return Ok(());}
        let boundary=style.display!=Display::Contents && !style.is_inline_level();
        if boundary || style.is_atomic_inline() {self.push("\n")?;}
        let mut child=session.document().first_child(node).map_err(|_|LayoutError::InvalidTree)?;
        while let Some(id)=child {
            child=session.document().next_sibling(id).map_err(|_|LayoutError::InvalidTree)?;
            if session.document().flat_tree_parent(id).map_err(|_|LayoutError::InvalidTree)?.is_some() {self.gather(session,id,depth+1)?;}
        }
        if br || boundary || style.is_atomic_inline() {self.push("\n")?;}
        Ok(())
    }
}

/// Case projection for a selected original CharacterData range. The source
/// offsets remain UTF-16 DOM offsets; full surrounding inline context supplies
/// conditional mappings even when only part of a word is selected.
pub fn transformed_source_text(session: &mut RenderSession, node: NodeId,
    offset: usize, count: usize, context: &mut Option<CaseContext>) -> Result<String, LayoutError> {
    let selected = session.document().substring_data(node, offset, count).map_err(|_| LayoutError::InvalidTree)?;
    let Some(parent) = session.document().flat_tree_parent(node).map_err(|_| LayoutError::InvalidTree)? else { return Ok(selected); };
    let style = session.computed_style(parent)?;
    if style.text_transform == crate::css::TextTransform::None { return Ok(selected); }
    let original = match session.document().kind(node).map_err(|_| LayoutError::InvalidTree)? {
        NodeKind::Text(value) | NodeKind::CData(value) => value.clone(),
        _ => return Err(LayoutError::InvalidTree),
    };
    let prefix = session.document().substring_data(node, 0, offset).map_err(|_| LayoutError::InvalidTree)?;
    let bytes = prefix.len()..prefix.len().saturating_add(selected.len());
    let single_character=original.chars().take(2).count()==1;
    CaseContext::ensure(context, session, node)?;
    let language = session.document().content_language(node).map_err(|_| LayoutError::InvalidTree)?;
    // A boundary between the two units of a supplementary scalar produces an
    // unpaired surrogate. Preserve that DOM substring instead of indexing a
    // non-boundary byte offset into the scalar source.
    let projected = if original.get(bytes.clone()) == Some(selected.as_str()) {
        let context = context.as_ref().ok_or(LayoutError::InvalidTree)?;
        if let Ok(index) = context.ranges.binary_search_by_key(&node.key(), |(id, _)| id.key()) {
            let start = context.ranges[index].1.start;
            lumen_common::case_transform::transform_case_source_range(&context.text, start + bytes.start..start + bytes.end, style.text_transform, language,single_character)
        } else {
            lumen_common::case_transform::transform_case_source_range(&original, bytes, style.text_transform, language,single_character)
        }
    } else {
        lumen_common::case_transform::transform_case_source_range(&selected, 0..selected.len(), style.text_transform, language,single_character)
    }.map_err(|_| LayoutError::Text)?;
    Ok(projected.into_owned())
}

impl Text {
    fn push(&mut self, ch: char) -> Result<(), LayoutError> {
        if self.value.len().saturating_add(ch.len_utf8()) > lumen_common::bidi::MAX_TEXT_BYTES {
            return Err(LayoutError::Text);
        }
        self.value.try_reserve(ch.len_utf8()).map_err(|_| LayoutError::CommandLimit)?;
        self.value.push(ch);
        Ok(())
    }
    fn flush_breaks(&mut self) -> Result<(), LayoutError> {
        let count = core::mem::take(&mut self.required_breaks);
        if !self.value.is_empty() {
            for _ in 0..count { self.push('\n')?; }
        }
        Ok(())
    }
    fn boundary(&mut self, count: u8) {
        self.required_breaks = self.required_breaks.max(count);
        self.space = false;
        self.line_has_content = false;
    }
    fn flush_space(&mut self) -> Result<(), LayoutError> {
        if core::mem::take(&mut self.space) { self.push(self.space_character)?; }
        Ok(())
    }
    fn literal_break(&mut self, ch: char) -> Result<(), LayoutError> {
        self.space = false;
        self.flush_breaks()?;
        self.push(ch)?;
        self.line_has_content = false;
        Ok(())
    }
    fn text(&mut self, value: &str, whitespace: WhiteSpace, mode: crate::css::TextTransform) -> Result<(), LayoutError> {
        let collapse = matches!(whitespace, WhiteSpace::Normal | WhiteSpace::NoWrap | WhiteSpace::PreLine);
        let preserve_break = !matches!(whitespace, WhiteSpace::Normal | WhiteSpace::NoWrap);
        let mut chars = value.chars().peekable();
        while let Some(mut ch) = chars.next() {
            if ch == '\r' {
                if chars.peek() == Some(&'\n') { chars.next(); }
                ch = '\n';
            }
            if ch == '\n' && preserve_break {
                self.literal_break('\n')?;
            } else if collapse && matches!(ch, ' ' | '\t' | '\n' | '\x0c') {
                if !self.space && self.line_has_content {
                    self.space = true;
                    self.space_character = if mode.full_width() {'\u{3000}'} else {' '};
                }
            } else {
                self.flush_breaks()?;
                self.flush_space()?;
                mode.visit_display_character(ch,|mapped|self.push(mapped))?;
                self.line_has_content = true;
            }
        }
        Ok(())
    }
    fn atomic_start(&mut self) -> Result<(), LayoutError> {
        self.flush_space()?;
        self.line_has_content = false;
        Ok(())
    }
    fn box_text(&mut self, value: &str, hard_break_after: bool) -> Result<(), LayoutError> {
        if !value.is_empty() {
            self.flush_breaks()?;
            self.flush_space()?;
            for ch in value.chars() { self.push(ch)?; }
            self.line_has_content = !value.ends_with('\n');
        }
        if hard_break_after { self.literal_break('\n')?; }
        Ok(())
    }
    fn atomic_end(&mut self) {
        self.space = false;
        self.line_has_content = true;
    }
}

fn replaced(name: &str) -> bool {
    matches!(name, "input" | "textarea" | "img" | "canvas" | "iframe" | "audio" | "video" | "embed")
}

fn is_rendered(session: &mut RenderSession, node: NodeId) -> Result<bool, LayoutError> {
    // HTML defines rendering by associated layout boxes. A completed host
    // frame is authoritative even when CSS alone would permit a box.
    if session.rendered_text_boxes().is_some() {
        return Ok(session.layout_rect(node).is_some());
    }
    let mut cursor = Some(node);
    let mut root = false;
    let mut depth = 0;
    while let Some(current) = cursor {
        if depth > 512 { return Err(LayoutError::DepthLimit); }
        depth += 1;
        if current == session.document().root() { root = true; break; }
        if matches!(session.document().kind(current), Ok(NodeKind::Element { .. })) {
            let style = session.computed_style(current)?;
            let display = style.display;
            if display == Display::None || current == node && (display == Display::Contents || display.is_table_column()) { return Ok(false); }
            if current != node && matches!(session.document().kind(current),
                Ok(NodeKind::Element {name, namespace: Namespace::Html, ..}) if replaced(name)) {
                return Ok(false);
            }
            if current != node && crate::layout::replacement_content_url(&style)?.is_some() {
                return Ok(false);
            }
        }
        cursor = session.document().flat_tree_parent(current).map_err(|_| LayoutError::InvalidTree)?;
    }
    Ok(root)
}

fn next_rendered_role(session: &mut RenderSession, node: NodeId, role: Display,
    container_role: Display) -> Result<bool, LayoutError> {
    let mut container = session.document().parent(node).map_err(|_| LayoutError::InvalidTree)?;
    while let Some(parent) = container {
        if matches!(session.document().kind(parent), Ok(NodeKind::Element { .. }))
            && session.computed_style(parent)?.display == container_role { break; }
        container = session.document().parent(parent).map_err(|_| LayoutError::InvalidTree)?;
    }
    let Some(container) = container else { return Ok(false) };
    let mut cursor = next_after_subtree(session.document(), container, node)?;
    while let Some(next) = cursor {
        if matches!(session.document().kind(next), Ok(NodeKind::Element { .. })) {
            let style = session.computed_style(next)?;
            if style.display == role && is_rendered(session, next)? { return Ok(true); }
            // A nested table never supplies a sibling cell/row to its outer table.
            if style.display.is_table() || style.display == Display::None {
                cursor = next_after_subtree(session.document(), container, next)?;
                continue;
            }
        }
        cursor = crate::selector::next_descendant(session.document(), container, next)
            .map_err(|_| LayoutError::InvalidTree)?;
    }
    Ok(false)
}

fn next_after_subtree(document: &Document, root: NodeId, mut node: NodeId) -> Result<Option<NodeId>, LayoutError> {
    loop {
        if node == root { return Ok(None); }
        if let Some(next) = document.next_sibling(node).map_err(|_| LayoutError::InvalidTree)? { return Ok(Some(next)); }
        let Some(parent) = document.parent(node).map_err(|_| LayoutError::InvalidTree)? else { return Ok(None) };
        node = parent;
    }
}

fn children(session: &mut RenderSession, node: NodeId, style: &Style,
    output: &mut Text, context:&mut Option<CaseContext>, depth: usize, select_only: bool, optgroup_only: bool) -> Result<(), LayoutError> {
    if depth > 512 { return Err(LayoutError::DepthLimit); }
    let composed = session.document().composes(node).map_err(|_| LayoutError::InvalidTree)?;
    let mut cursor = session.document().first_child(node).map_err(|_| LayoutError::InvalidTree)?;
    while let Some(child) = cursor {
        cursor = session.document().next_sibling(child).map_err(|_| LayoutError::InvalidTree)?;
        let flat_parent = if composed {
            let parent = session.document().flat_tree_parent(child).map_err(|_| LayoutError::InvalidTree)?;
            if parent.is_none() { continue; }
            parent
        } else { Some(node) };
        if select_only || optgroup_only {
            let allowed = matches!(session.document().kind(child),
                Ok(NodeKind::Element {name, namespace: Namespace::Html, ..})
                    if name == "option" || select_only && name == "optgroup");
            if !allowed {
                // Wrappers have no boxes in the select model, but their option
                // descendants remain eligible for the specified child boxes.
                children(session, child, style, output, context, depth + 1, select_only, optgroup_only)?;
                continue;
            }
        }
        if flat_parent != Some(node) && matches!(session.document().kind(child), Ok(NodeKind::Text(_) | NodeKind::CData(_))) {
            let inherited = session.computed_style(flat_parent.ok_or(LayoutError::InvalidTree)?)?;
            collect(session, child, &inherited, output, context, depth + 1)?;
        } else {
            collect(session, child, style, output, context, depth + 1)?;
        }
    }
    Ok(())
}

fn collect(session: &mut RenderSession, node: NodeId, parent: &Style,
    output: &mut Text, context:&mut Option<CaseContext>, depth: usize) -> Result<(), LayoutError> {
    if depth > 512 { return Err(LayoutError::DepthLimit); }
    match session.document().kind(node).map_err(|_| LayoutError::InvalidTree)? {
        NodeKind::Text(value) | NodeKind::CData(value) => {
            if !parent.visibility_visible {return Ok(());}
            if output.synthetic_controls_depth == 0 {
                if let Some(boxes) = session.rendered_text_boxes() {
                    let start = boxes.partition_point(|value| value.node.key() < node.key());
                    for value in boxes[start..].iter().take_while(|value| value.node == node) {
                        output.box_text(&value.text[value.range.clone()], value.hard_break_after)?;
                    }
                    return Ok(());
                }
            }
            if parent.text_transform==crate::css::TextTransform::None {return output.text(value,parent.white_space,parent.text_transform);}
            if !parent.text_transform.needs_case_context() {
                let converted=lumen_common::case_transform::transform_case_range(value,0..value.len(),parent.text_transform.before_display(),None).map_err(|_|LayoutError::Text)?;
                return output.text(&converted,parent.white_space,parent.text_transform);
            }
            let value=value.clone();
            if parent.text_transform.needs_case_context() {CaseContext::ensure(context, session, node)?;}
            let language=session.document().content_language(node).map_err(|_|LayoutError::InvalidTree)?;
            let source=context.as_ref().filter(|_|parent.text_transform.needs_case_context()).and_then(|context| {
                context.ranges.binary_search_by_key(&node.key(),|(id,_)|id.key()).ok().map(|index|(context,context.ranges[index].1.clone()))
            });
            let transformed=if let Some((context,range))=source {
                lumen_common::case_transform::transform_case_source_range(&context.text,range,parent.text_transform.before_display(),language,value.chars().take(2).count()==1)
            }else{lumen_common::case_transform::transform_case_range(&value,0..value.len(),parent.text_transform.before_display(),language)}.map_err(|_|LayoutError::Text)?;
            return output.text(&transformed,parent.white_space,parent.text_transform);
        }
        NodeKind::Element { .. } => {}
        _ => return Ok(()),
    }
    let mut style = session.computed_style(node)?;
    if style.display == Display::None || style.display.is_table_column() { return Ok(()); }
    let (is_html, paragraph, br, replace, select, optgroup, option, suppress) = match session.document().kind(node)
        .map_err(|_| LayoutError::InvalidTree)? {
        NodeKind::Element {name, namespace, ..} => {
            let html = *namespace == Namespace::Html;
            (html, html && name == "p", html && name == "br", html && replaced(name),
                html && name == "select", html && name == "optgroup", html && name == "option",
                *namespace == Namespace::Svg && matches!(name.as_str(), "defs" | "stop" | "symbol" | "title" | "desc"))
        }
        _ => return Ok(()),
    };
    if suppress { return Ok(()); }
    if style.display == Display::Contents {
        return children(session, node, &style, output, context, depth, false, false);
    }
    if select { style.display = Display::Inline; }
    if optgroup || option { style.display = Display::Block; }
    let visible = style.visibility_visible;
    let block = visible && !style.is_inline_level() && matches!(style.display,
        Display::Block | Display::FlowRoot | Display::ListItem | Display::Table | Display::TableCaption | Display::Flex | Display::Grid);
    let count = if paragraph && visible { 2 } else if block { 1 } else { 0 };
    if count > 0 { output.boundary(count); }
    let atomic = visible && (replace || style.is_atomic_inline());
    if atomic { output.atomic_start()?; }
    let previous_controls_depth = output.synthetic_controls_depth;
    if select || optgroup || option { output.synthetic_controls_depth += 1; }
    if !replace && crate::layout::replacement_content_url(&style)?.is_none() {
        children(session, node, &style, output, context, depth, is_html && select, is_html && optgroup)?;
    }
    output.synthetic_controls_depth = previous_controls_depth;
    if atomic { output.atomic_end(); }
    if visible && br { output.literal_break('\n')?; }
    let next_cell = if style.display != Display::TableCell { false } else if let Some(member) = session.rendered_table_member(node, Display::TableCell) {
        member.context.cell + 1 < member.context.cell_count
    } else { next_rendered_role(session, node, Display::TableCell, Display::TableRow)? };
    if visible && style.display == Display::TableCell && next_cell {
        output.space = false;
        output.flush_breaks()?;
        output.push('\t')?;
        output.line_has_content = false;
    }
    let next_row = if style.display != Display::TableRow { false } else if let Some(member) = session.rendered_table_member(node, Display::TableRow) {
        member.context.row + 1 < member.context.row_count
    } else { next_rendered_role(session, node, Display::TableRow, Display::Table)? };
    if visible && style.display == Display::TableRow && next_row {
        output.literal_break('\n')?;
    }
    if count > 0 { output.boundary(count); }
    Ok(())
}

/// The shared HTML innerText/outerText getter. The host synchronizes rendering
/// first; this collection consumes the session's actual computed CSS context.
pub fn get(session: &mut RenderSession, node: NodeId) -> Result<String, LayoutError> {
    if !matches!(session.document().kind(node), Ok(NodeKind::Element { .. })) {
        return Err(LayoutError::InvalidTree);
    }
    if !is_rendered(session, node)? {
        let mut result = Text::default();
        let mut cursor = session.document().first_child(node).map_err(|_| LayoutError::InvalidTree)?;
        while let Some(child) = cursor {
            if let NodeKind::Text(text) | NodeKind::CData(text) = session.document().kind(child).map_err(|_| LayoutError::InvalidTree)? {
                for ch in text.chars() { result.push(ch)?; }
            }
            cursor = crate::selector::next_descendant(session.document(), node, child).map_err(|_| LayoutError::InvalidTree)?;
        }
        return Ok(result.value);
    }
    let style = session.computed_style(node)?;
    let is_replaced = matches!(session.document().kind(node),
        Ok(NodeKind::Element {name, namespace:Namespace::Html, ..}) if replaced(name));
    if is_replaced || crate::layout::replacement_content_url(&style)?.is_some() { return Ok(String::new()); }
    let select = matches!(session.document().kind(node), Ok(NodeKind::Element {name, namespace:Namespace::Html, ..}) if name == "select");
    let optgroup = matches!(session.document().kind(node), Ok(NodeKind::Element {name, namespace:Namespace::Html, ..}) if name == "optgroup");
    let mut output = Text::default();
    if select || optgroup || matches!(session.document().kind(node), Ok(NodeKind::Element {name, namespace:Namespace::Html, ..}) if name == "option") {
        output.synthetic_controls_depth = 1;
    }
    let mut context=None;
    children(session,node,&style,&mut output,&mut context,0,select,optgroup)?;
    Ok(output.value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{html, selector};

    fn text(markup: &str) -> String {
        let document = html::parse(markup, 512).unwrap();
        let target = selector::query_selector(&document, document.root(), "#target").unwrap().unwrap();
        get(&mut RenderSession::new(document), target).unwrap()
    }

    #[test]
    fn specification_modern_transforms_rendered_selection_preserve_text_node_identity() {
        assert_eq!(text("<div id=target style='text-transform:full-width'>a  \t<span> </span><span>\n</span>b</div>"),"ａ\u{3000}ｂ", "Phase I collapses original spaces across Text nodes before width conversion");
        assert_eq!(text("<div id=target style='text-transform:full-width;white-space:pre-wrap'>a   b</div>"),"ａ\u{3000}\u{3000}\u{3000}ｂ", "preserved spaces remain separate full-width separators");
        let document=html::parse("<div id=target style='text-transform:full-width full-size-kana'>a ｧ</div><p id=math style='text-transform:math-auto'><span>a</span><span>ab</span><span>h</span></p>",64).unwrap();
        let target=selector::query_selector(&document,document.root(),"#target").unwrap().unwrap();
        let math=selector::query_selector(&document,document.root(),"#math").unwrap().unwrap();
        let source=document.first_child(target).unwrap().unwrap();
        let mut session=RenderSession::new(document);
        assert_eq!(get(&mut session,target).unwrap(),"ａ\u{3000}ア");
        assert_eq!(get(&mut session,math).unwrap(),"\u{1d44e}abℎ");
        let mut context=None;
        assert_eq!(transformed_source_text(&mut session,source,0,1,&mut context).unwrap(),"ａ");
        let second=selector::query_selector(session.document(),session.document().root(),"#math span:nth-child(2)").unwrap().unwrap();
        let second_source=session.document().first_child(second).unwrap().unwrap();
        assert_eq!(transformed_source_text(&mut session,second_source,0,1,&mut context).unwrap(),"a");
        assert!(matches!(session.document().kind(source),Ok(NodeKind::Text(value)) if value=="a ｧ"));
    }

    #[test]
    fn specification_contextual_case_rendered_and_selected_ranges_keep_original_sources() {
        assert_eq!(text("<div id=target lang=el style='text-transform:uppercase'>Νε<span>ρά</span>ιδα ή Ι<span>\u{308}\u{301}</span>Ρ</div>"), "ΝΕΡΑΪΔΑ Ή Ι\u{308}Ρ");
        // Case words ignore out-of-flow boundaries, while HTML rendered-text
        // step 9 still requires breaks around their blockified boxes.
        assert_eq!(text("<div id=target style='text-transform:capitalize'>p<span style='position:absolute'></span>ass</div>"), "P\nass");
        let document = html::parse("<div id=target lang=el style='text-transform:uppercase'>Νερά<span>ιδα</span></div><p id=expansion style='text-transform:uppercase'>aßb</p>", 64).unwrap();
        let target = selector::query_selector(&document, document.root(), "#target span").unwrap().unwrap();
        let source = document.first_child(target).unwrap().unwrap();
        let expansion = selector::query_selector(&document, document.root(), "#expansion").unwrap().unwrap();
        let expansion_source = document.first_child(expansion).unwrap().unwrap();
        let mut session = RenderSession::new(document);
        let mut context = None;
        assert_eq!(transformed_source_text(&mut session, source, 0, 1, &mut context).unwrap(), "Ϊ", "preceding accent outside selected range supplies diaeresis");
        assert_eq!(transformed_source_text(&mut session, expansion_source, 1, 1, &mut context).unwrap(), "SS");
        assert!(matches!(session.document().kind(expansion_source), Ok(NodeKind::Text(value)) if value == "aßb"));
    }

    #[test]
    fn rendered_case_transforms_share_inline_context_and_keep_dom_text_unchanged() {
        for (markup,expected) in [
            ("<div id=target style='text-transform:uppercase'>Maß <span>kitty</span></div>","MASS KITTY"),
            ("<div lang=tr><div id=target style='text-transform:uppercase'>i ı</div></div>","İ I"),
            ("<div lang=tr><div id=target lang='' style='text-transform:uppercase'>i</div></div>","I"),
            ("<div style='text-transform:capitalize'>a<span id=target>b</span>c</div>","b"),
            ("<div style='text-transform:capitalize'>hello <span id=target>world</span></div>","World"),
            ("<div lang=nl style='text-transform:capitalize'>i<span id=target>jsland</span></div>","Jsland"),
            ("<div style='text-transform:lowercase'>Ο<span id=target>Σ</span>Α</div>","σ"),
            ("<div style='text-transform:lowercase'>Ο<span id=target>Σ</span></div>","ς"),
            ("<div id=target style='display:none;text-transform:uppercase'>Maß</div>","Maß"),
            ("<div id=target style='text-transform:capitalize'>john's foo_bar foo-bar</div>","John's Foo_bar Foo-Bar"),
        ] {assert_eq!(text(markup),expected,"{markup}");}
        let document=html::parse("<div id=target style='text-transform:uppercase'>Maß</div>",16).unwrap();
        let target=selector::query_selector(&document,document.root(),"#target").unwrap().unwrap();
        let source=document.first_child(target).unwrap().unwrap();
        let mut session=RenderSession::new(document);
        assert_eq!(get(&mut session,target).unwrap(),"MASS");
        assert!(matches!(session.document().kind(source).unwrap(),NodeKind::Text(value) if value=="Maß"));
        session.document_mut().set_attribute(target,"lang","tr").unwrap();
        session.document_mut().replace_data(source,"i").unwrap();
        assert_eq!(get(&mut session,target).unwrap(),"İ");
    }

    #[test]
    fn specification_rendered_text_collects_css_boxes_without_generated_or_replaced_content() {
        let cases = [
            ("<div id=target> abc <span>  def </span> ghi </div>", "abc def ghi"),
            ("<div id=target> a <br> b <div> c </div> d </div>", "a\nb\nc\nd"),
            ("<div id=target>a<p>b</p><p></p><div>c</div>d</div>", "a\n\nb\n\nc\nd"),
            ("<div id=target>a<input value=ignored> b<img alt=ignored> c</div>", "a b c"),
            ("<div id=target>a <input> b</div>", "a  b"),
            ("<div id=target>a<div></div><input></div>", "a"),
            ("<div id=target>a<span style='display:inline-block'> b </span> c</div>", "ab c"),
            ("<div id=target>abc <span style='display:inline-block'> def </span> ghi</div>", "abc def ghi"),
            ("<div id=target><pre> a\tb\n c </pre><div>d</div></div>", " a\tb\n c \nd"),
            ("<div id=target style='white-space:pre-line'> a \n  b \n c </div>", "a\nb\nc"),
            ("<div id=target style='width:0'>abc def\u{00ad}ghi</div>", "abc def\u{00ad}ghi"),
            ("<div id=target>before<span style='display:none'> hidden </span>after</div>", "beforeafter"),
            ("<div id=target style='visibility:hidden'>hidden<span style='visibility:visible'> visible </span></div>", "visible"),
            ("<div id=target>a<span style='display:contents'> b </span>c</div>", "a b c"),
            ("<div id=target>a<p style='display:contents'>b</p>c</div>", "abc"),
            ("<div id=target style='display:contents'> a  <p>b</p> c </div>", " a  b c "),
            ("<div id=target><script style='display:block'>shown</script><style style='display:block'>also</style></div>", "shown\nalso"),
            ("<div id=target><object>fallback</object><span style='content:url(image.png)'>ignored</span></div>", "fallback"),
            ("<div id=target style='display:none'> a  <p>b</p> c </div>", " a  b c "),
            ("<div style='display:none'><div id=target> a  b </div></div>", " a  b "),
            ("<div id=target><textarea>ignored</textarea><canvas>ignored</canvas><button>kept</button></div>", "kept"),
            ("<div id=target>a<select><option>one</option><optgroup><option>two</option></optgroup></select>b</div>", "a\none\ntwo\nb"),
            ("<div id=target><table><tr><td> a </td><td>b</td></tr><tr><td>c</td><td>d</td></tr></table></div>", "a\tb\nc\td"),
            ("<div id=target><table><tr><td>a<table><tr><td>x</td><td>y</td></tr></table></td><td>b</td></tr></table></div>", "a\nx\ty\n\tb"),
        ];
        for (markup, expected) in cases { assert_eq!(text(markup), expected, "{markup}"); }
        assert_eq!(text("<style>#target::before{content:'ignored'}</style><div id=target>text</div>"), "text");
        let document = html::parse("<div id=target>  raw <p>text</p>  </div>", 32).unwrap();
        let target = selector::query_selector(&document, document.root(), "#target").unwrap().unwrap();
        let mut session = RenderSession::new(document);
        session.document_mut().remove(target).unwrap();
        assert_eq!(get(&mut session, target).unwrap(), "  raw text  ");
    }
}
