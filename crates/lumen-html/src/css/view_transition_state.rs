//! Styles of generated view-transition pseudo-elements that depend on the
//! captured elements rather than on the cascade alone.
use super::*;

impl Style {
    /// The writing mode, direction, colour scheme and compositing state that a
    /// captured element lends to its `::view-transition-group()`.
    pub fn adopt_captured_group_properties(&mut self, captured: &Style) {
        self.writing_mode = captured.writing_mode.clone();
        self.direction = captured.direction.clone();
        self.text_orientation = captured.text_orientation.clone();
        self.color_scheme = captured.color_scheme.clone();
        for property in PROPERTIES {
            if matches!(property.name, "mix-blend-mode" | "backdrop-filter") {
                for &slot in property.ids {
                    copy_slot(slot, captured, self);
                }
            }
        }
    }

    /// The user-agent animations, blending and isolation of one generated
    /// pseudo-element, given which of the old and new snapshots exist. Author
    /// `animation-name` declarations replace the user-agent animations.
    pub fn apply_view_transition_state(&mut self, pseudo: PseudoElement, capture: &str, old: bool, new: bool) {
        let paired = old && new;
        let user_agent = self.animation[0].is_none();
        let (present, fade) = match pseudo {
            PseudoElement::ViewTransitionGroup => {
                if user_agent {
                    let name = if paired {
                        alloc::format!("-ua-view-transition-group-anim-{capture}")
                    } else {
                        String::from("none")
                    };
                    self.animation[0] = Some(Arc::from(name));
                }
                return;
            }
            PseudoElement::ViewTransitionImagePair => {
                if paired {
                    extra_props::set_stored(self, "isolation", Some("isolate"));
                }
                return;
            }
            PseudoElement::ViewTransitionOld => (old, "-ua-view-transition-fade-out"),
            PseudoElement::ViewTransitionNew => (new, "-ua-view-transition-fade-in"),
            _ => return,
        };
        if !user_agent {
            return;
        }
        if !present {
            self.animation[0] = Some(Arc::from("none"));
            return;
        }
        if paired {
            self.animation[0] = Some(Arc::from(alloc::format!("{fade}, -ua-mix-blend-mode-plus-lighter")));
            extra_props::set_stored(self, "mix-blend-mode", Some("plus-lighter"));
        } else {
            self.animation[0] = Some(Arc::from(fade));
        }
    }
}
