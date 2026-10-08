//! HTML computed heading levels, shared by rendering and heading consumers.

use crate::{Document, Error, Namespace, NodeId, NodeKind};

pub fn heading_rank(kind:&NodeKind)->Option<u8> {
    let NodeKind::Element { namespace: Namespace::Html, name, .. } = kind else {
        return None;
    };
    match crate::svg::local_name(name) {"h1"=>Some(1),"h2"=>Some(2),"h3"=>Some(3),"h4"=>Some(4),"h5"=>Some(5),"h6"=>Some(6),_=>None}
}

pub fn computed_heading_level(document: &Document, node: NodeId) -> Result<Option<u8>, Error> {
    let Some(level)=heading_rank(document.kind(node)?) else {
        return Ok(None);
    };
    let mut level: usize = usize::from(level);
    let mut ancestor = Some(node);
    while let Some(current) = ancestor {
        if matches!(document.kind(current)?, NodeKind::Element { namespace: Namespace::Html, .. }) {
            if let Some(offset) = document.get_attribute_ns_ref(current, None, "headingoffset")?
                .and_then(|value| crate::forms::parse_nonnegative_integer_capped(value, 9))
            {
                level = level.saturating_add(offset);
            }
            if level >= 9 || document.get_attribute_ns_ref(current, None, "headingreset")?.is_some() {
                return Ok(Some(level.min(9) as u8));
            }
        }
        ancestor = match document.parent(current)? {
            Some(parent) => match document.shadow_host(parent)? {
                Some(host) => Some(host),
                None if matches!(document.kind(parent)?, NodeKind::Element { .. }) => Some(parent),
                None => None,
            },
            None => None,
        };
    }
    Ok(Some(level.min(9) as u8))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specification_heading_offsets_follow_real_ancestry_resets_and_shadow_hosts() {
        let mut document = crate::html::parse("<article headingoffset=2><h1 id=outer>A</h1><section headingoffset=1 headingreset><h2 id=reset>B</h2></section><div id=host headingoffset=3></div></article>", 64).unwrap();
        let find = |document: &Document, selector| crate::selector::query_selector(document, document.root(), selector).unwrap().unwrap();
        assert_eq!(computed_heading_level(&document, find(&document, "#outer")).unwrap(), Some(3));
        assert_eq!(computed_heading_level(&document, find(&document, "#reset")).unwrap(), Some(3));
        let host = find(&document, "#host");
        let root = document.attach_shadow(host, crate::ShadowMode::Open).unwrap();
        let heading = document.create(NodeKind::Element { namespace: Namespace::Html, name: "h2".into(), attributes: alloc::vec![] }).unwrap();
        document.append(root, heading).unwrap();
        assert_eq!(computed_heading_level(&document, heading).unwrap(), Some(7));
        document.set_attribute(heading, "headingoffset", " +8 trailing").unwrap();
        assert_eq!(computed_heading_level(&document, heading).unwrap(), Some(9));
        document.set_attribute(heading, "headingoffset", "9999999999999999999999999999999999999").unwrap();
        assert_eq!(computed_heading_level(&document, heading).unwrap(), Some(9));
        document.set_attribute(heading, "headingoffset", "-1").unwrap();
        assert_eq!(computed_heading_level(&document, heading).unwrap(), Some(7));
        document.set_attribute(host, "headingreset", "").unwrap();
        assert_eq!(computed_heading_level(&document, heading).unwrap(), Some(5));
    }
}
