//! Transition snapshots taken against effect samples that the live cascade
//! does not show, and the part of a snapshot a style change can reach.
use super::*;

impl RenderSession {
    /// Runs `capture` with the Animation-origin effect declarations of each
    /// listed target replaced, as if they were published, then restores the
    /// published ones. Nothing is invalidated: style caches, retained layout
    /// and the frame keep describing the published samples, so only
    /// transition snapshot captures and starting styles belong inside.
    pub fn with_animation_effect_overrides<R>(&mut self,
        overrides:&[(NodeId,Option<css::PseudoElement>,Vec<(String,String)>)],
        capture:impl FnOnce(&mut Self)->Result<R,LayoutError>)->Result<R,LayoutError> {
        if overrides.is_empty() {return capture(self);}
        self.refresh_rules_only()?;
        let mut installed=Vec::new();
        let mut failure=None;
        for (node,pseudo,pairs) in overrides {
            if !matches!(self.document.kind(*node),Ok(NodeKind::Element{..})) {continue;}
            let rules=self.rules.as_mut().expect("stylesheets initialized");
            if let Err(error)=rules.set_pseudo_effect_declarations(*node,*pseudo,css::EffectOrigin::Animation,pairs) {
                failure=Some(LayoutError::Css(error));
                break;
            }
            installed.push((*node,*pseudo));
            self.effect_override_nodes.insert(node.key());
        }
        let result=match failure {Some(error)=>Err(error),None=>capture(self)};
        for (node,pseudo) in installed {
            let published=self.animated_styles.iter()
                .find(|(target,target_pseudo,origin,_)|*target==node && *target_pseudo==pseudo && *origin==css::EffectOrigin::Animation)
                .map_or(&[][..],|(..,pairs)|pairs.as_slice());
            if let Some(rules)=self.rules.as_mut() {
                // These pairs were admitted when they were published.
                let _=rules.set_pseudo_effect_declarations(node,pseudo,css::EffectOrigin::Animation,published);
            }
        }
        self.effect_override_nodes.clear();
        result
    }

    /// Records that the samples the next snapshot is taken against changed for
    /// `nodes` since the last one, as publishing them would have.
    pub fn note_snapshot_effect_changes(&mut self,nodes:&[NodeId]) {
        if self.snapshot_basis.is_none() || self.snapshot_effects_unbounded {return;}
        for node in nodes {
            self.snapshot_effect_nodes.insert(node.key());
            if self.snapshot_effect_nodes.len()>4096 {
                self.snapshot_effect_nodes.clear();
                self.snapshot_effects_unbounded=true;
                return;
            }
        }
    }

    /// The connected elements whose after-change style a snapshot following
    /// `previous` may compute differently from `previous`: those mutations
    /// and effect publications since it reach, everything below them, elements
    /// styled from container queries and elements with pseudo-element entries.
    /// Every other element keeps its earlier style. `None` when the snapshot
    /// cannot extend `previous`, the rules or another non-document input
    /// changed, or the scope exceeds `limit` elements.
    pub fn transition_restyle_scope(&mut self,text:Option<&dyn TextShaper>,previous:&Arc<TransitionSnapshot>,limit:usize)->Option<Vec<NodeId>> {
        let epoch=self.transition_input_epoch().ok()?;
        if !previous.epoch.differs_only_in_document(&epoch) {return None;}
        let basis=SnapshotBasis{version:epoch.document_version,text_generation:text.map(TextShaper::generation),
            had_effects:self.rules.as_ref().is_some_and(css::StyleIndex::has_effect_declarations)};
        let mut roots:Vec<u128>={
            let base=self.snapshot_extension(previous,&epoch,basis)?;
            base.dirty.iter().chain(&base.subtrees).copied().collect()
        };
        for input in &previous.nodes {
            if input.pseudo.is_some() || input.style.has_query_container_dependencies() || input.style.has_inherited_query_container_dependencies() {
                roots.push(input.node.key());
            }
        }
        roots.sort_unstable();
        roots.dedup();
        let mut scope=alloc::collections::BTreeSet::new();
        for key in roots {
            if scope.contains(&key) {continue;}
            let root=NodeId::from_key(key);
            if !self.document.is_connected_element(root) {continue;}
            let mut node=root;
            loop {
                if matches!(self.document.kind(node),Ok(NodeKind::Element{..})) && scope.insert(node.key()) && scope.len()>limit {return None;}
                match crate::selector::next_shadow_including_descendant(&self.document,root,node) {
                    Ok(Some(next))=>node=next,
                    Ok(None)=>break,
                    Err(_)=>return None,
                }
            }
        }
        // Inheritance follows the composed parents the snapshot recorded, which
        // a light-tree walk misses for slotted elements. Entries are recorded
        // parent before child within each tree; repeat for slot crossings.
        for _ in 0..4 {
            let mut grown=false;
            for input in &previous.nodes {
                if input.parent.is_some_and(|parent|scope.contains(&parent.key())) && !scope.contains(&input.node.key())
                    && self.document.is_connected_element(input.node) {
                    scope.insert(input.node.key());
                    grown=true;
                    if scope.len()>limit {return None;}
                }
            }
            if !grown {
                return Some(scope.into_iter().map(NodeId::from_key).filter(|node|self.document.is_connected_element(*node)).collect());
            }
        }
        None
    }
}
