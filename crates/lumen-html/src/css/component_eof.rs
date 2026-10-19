//! CSS Syntax EOF closure for bounded declaration component values.
use super::*;
use alloc::borrow::Cow;

/// Serialize the implicit closing tokens of an EOF-terminated component block.
/// Ordinary values remain borrowed. Token boundaries, quoted contents, escapes,
/// mismatched delimiters and depth limits come from the shared CSS block lexer.
#[cfg(test)]
pub(super) fn complete_component_eof(raw: &str) -> Option<Cow<'_,str>> {
    syntax::complete(raw).ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn function_eof_closure_uses_shared_component_boundaries() {
        for (raw,expected) in [
            ("translateX(55px","translateX(55px)"),
            ("translateX(calc(20px + 35px","translateX(calc(20px + 35px))"),
            ("var(--x, [value {x}","var(--x, [value {x}])"),
            ("translateX(55px/* ) ] */","translateX(55px/* ) ] */)"),
            ("foo('()]') bar(1","foo('()]') bar(1)"),
        ] {assert_eq!(complete_component_eof(raw).unwrap(),expected);}
        assert!(matches!(complete_component_eof("translateX(55px)"),Some(Cow::Borrowed(_))));
        assert_eq!(complete_component_eof("translateX(55px]").unwrap(),"translateX(55px])","a mismatched closing token does not close another block");
        assert!(complete_component_eof("translateX(55px))").is_none());
        assert!(complete_component_eof(&"(".repeat(33)).is_none());
        assert!(matches!(complete_component_eof(&"x".repeat(MAX_VARIABLE_BYTES+1)),Some(Cow::Borrowed(_))));
        assert!(complete_component_eof(&"x".repeat(MAX_CSS_BYTES+1)).is_none());
    }

    #[test]
    fn declaration_eof_functions_preserve_grammar_and_previous_valid_values() {
        let mut block=DeclarationBlock::default();
        assert!(block.set("transform","translateX(55px",false).unwrap());
        assert_eq!(block.value("transform").unwrap().0,"translateX(55px)");
        for invalid in ["translateX(55px]","translateX(55px))","translateX(55px; color:red","translateX(55px garbage"] {
            assert!(!block.set("transform",invalid,false).unwrap(),"{invalid}");
            assert_eq!(block.value("transform").unwrap().0,"translateX(55px)");
        }
        assert!(block.set("transform","translateX(calc(20px + 35px",false).unwrap());
        assert!(block.value("transform").unwrap().0.ends_with("))"));
        assert!(block.set("transform","translateX(var(--offset, 55px",false).unwrap());
        assert_eq!(block.value("transform").unwrap().0,"translateX(var(--offset, 55px))");
        assert!(block.set("content","'literal ( ) /* */'",false).unwrap());
        assert!(block.value("content").unwrap().0.contains("literal ( ) /* */"));
    }
}
