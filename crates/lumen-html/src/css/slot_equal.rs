//! Per-property equality of two computed styles without serializing either.
use super::*;

/// Reusable scratch pair; each comparison copies one property's slot state
/// from both styles into otherwise initial styles and compares those.
pub struct PropertyComparer {
    initial: Style,
    left: Style,
    right: Style,
}

impl Default for PropertyComparer {
    fn default() -> Self {
        Self::new()
    }
}

impl PropertyComparer {
    pub fn new() -> Self {
        let initial = Style::initial();
        Self { left: initial.clone(), right: initial.clone(), initial }
    }

    /// Whether `name` computes to the same state in both styles. `None` when the
    /// property is custom, unknown, or may depend on state outside its own
    /// slots; callers then compare the serialized values.
    pub fn property_equal(&mut self, a: &Style, b: &Style, name: &str) -> Option<bool> {
        if name.starts_with("--") {
            return None;
        }
        if a.font_size.to_bits() != b.font_size.to_bits() || a.color != b.color
            || a.writing_mode != b.writing_mode || a.display != b.display {
            return None;
        }
        let ids = PROPERTIES[registry_index(name)?].ids;
        for &slot in ids {
            copy_slot(slot, a, &mut self.left);
            copy_slot(slot, b, &mut self.right);
            let equal = self.left == self.right;
            self.reset(slot);
            if !equal {
                return Some(false);
            }
        }
        Some(true)
    }

    /// Whether `a` and `b` agree on every property slot outside those of
    /// `names`. Returns those slots when they do: any property none of whose
    /// slots is among them (`slots_avoid`) then computes to the same state in
    /// both, which saves comparing each such property on its own. `None` when
    /// the styles differ elsewhere, in a value other slots resolve against, or
    /// a name is custom or unknown.
    pub fn equal_outside(a: &Style, b: &Style, names: &[&str]) -> Option<Vec<usize>> {
        if a.font_size.to_bits() != b.font_size.to_bits() || a.color != b.color
            || a.writing_mode != b.writing_mode || a.display != b.display {
            return None;
        }
        let mut slots = Vec::new();
        for name in names {
            if name.starts_with("--") {
                return None;
            }
            slots.extend_from_slice(PROPERTIES[registry_index(name)?].ids);
        }
        let mut patched = a.clone();
        for &slot in &slots {
            patched.relative_lengths.retain(|(key, _)| *key != slot);
            patched.relative_expressions.retain(|expression| expression.slot != slot);
            copy_slot(slot, b, &mut patched);
        }
        (patched == *b).then_some(slots)
    }

    /// Whether `name` is a known property none of whose slots is in `slots`.
    pub fn slots_avoid(name: &str, slots: &[usize]) -> bool {
        !name.starts_with("--")
            && registry_index(name).is_some_and(|index| !PROPERTIES[index].ids.iter().any(|slot| slots.contains(slot)))
    }

    fn reset(&mut self, slot: usize) {
        for style in [&mut self.left, &mut self.right] {
            copy_slot(slot, &self.initial, style);
            style.relative_lengths.retain(|(key, _)| *key != slot);
            style.relative_expressions.retain(|expression| expression.slot != slot);
        }
    }
}
