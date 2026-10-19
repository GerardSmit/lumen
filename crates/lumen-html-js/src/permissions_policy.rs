//! Document navigation policy and node-bound introspection. Declarations and
//! inheritance algorithms are shared; the browser's existing Origin remains
//! the only identity authority, including sandboxed opaque origins.
use super::*;
use browsing_context::Origin;
use lumen_common::limits::ByteBudget;
use lumen_common::permissions_policy::{self as policy, Declarations, DefaultAllowlist, Feature};
use std::sync::Arc;

// Advertise only policy-controlled features with real supported consumers.
// Unsupported microphone/audio sources and autoplay must not be presented as
// implemented merely because a declaration can be parsed.
pub(crate) const FEATURES: [Feature; 3] = [
    Feature {
        name: "focus-without-user-activation",
        default: DefaultAllowlist::SelfOrigin,
    },
    Feature {
        name: "fullscreen",
        default: DefaultAllowlist::SelfOrigin,
    },
    Feature {
        name: "camera",
        default: DefaultAllowlist::SelfOrigin,
    },
];
pub(crate) const FOCUS: usize = 0;
pub(crate) const FULLSCREEN: usize = 1;
pub(crate) const CAMERA: usize = 2;

struct Policy {
    origin: Arc<Origin>,
    inherited: [bool; FEATURES.len()],
    declared: Option<Arc<Declarations>>,
}
impl Policy {
    fn declared_matches(&self, index: usize, origin: &Origin) -> Option<bool> {
        self.declared
            .as_ref()?
            .get(index)
            .map(|list| list.matches(&origin.serialize(), origin.same_origin(&self.origin), false))
    }
    fn value(&self, index: usize, origin: &Origin) -> bool {
        policy::feature_value(self.inherited[index], self.declared_matches(index, origin))
    }
    fn allows(&self, index: usize, origin: &Origin) -> bool {
        policy::check_policy(
            self.inherited[index],
            self.declared_matches(index, origin),
            FEATURES[index].default,
            origin.same_origin(&self.origin),
        )
    }
}
#[derive(Default)]
pub(crate) struct State {
    policies: Option<Rc<DocumentPolicies>>,
    sender: Option<scheduling::TaskSender>,
}
pub(crate) struct DocumentPolicies {
    enforce: Policy,
    report: Policy,
    budget: Arc<ByteBudget>,
    creation_url: Arc<lumen_common::limits::BudgetedString>,
}
pub(crate) fn install(ctx: &mut Ctx, realm: &Rc<DomRealm>) -> OpResult<()> {
    realm.permissions_policy.borrow_mut().sender = Some(scheduling::task_sender(ctx)?);
    Ok(())
}
fn quota(_: policy::Error) -> OpError {
    OpError::new("QuotaExceededError", "Permissions Policy storage exhausted")
}

fn sandboxed_origin(owner: &DomRealm, node: Option<NodeId>) -> bool {
    if owner.lifecycle.sandboxed_origin.get() {
        return true;
    }
    let Some(node) = node else {
        return false;
    };
    let session = owner.session.borrow();
    let document = session.document();
    lumen_html::forms::html_element_local_name(document, node) == Some("iframe")
        && document
            .get_attribute_ns_ref(node, None, "sandbox")
            .ok()
            .flatten()
            .is_some_and(|flags| {
                !flags
                    .split_ascii_whitespace()
                    .any(|flag| flag.eq_ignore_ascii_case("allow-same-origin"))
            })
}
fn declared_origin(owner: &DomRealm, node: NodeId, parent: &Origin) -> Origin {
    // §7.2 tests the captured sandbox-origin flag, not whether an unrelated
    // data/about document happens to have an opaque origin.
    if sandboxed_origin(owner, Some(node)) {
        return Origin::opaque();
    }
    let session = owner.session.borrow();
    let document = session.document();
    if document
        .get_attribute_ns_ref(node, None, "srcdoc")
        .ok()
        .flatten()
        .is_some()
    {
        return parent.clone();
    }
    if let Some(source) = document
        .get_attribute_ns_ref(node, None, "src")
        .ok()
        .flatten()
    {
        if let Ok(url) = lumen_common::url::parse(source, Some(&owner.base_url())) {
            return Origin::from_url(&url.href());
        }
    }
    parent.clone()
}
fn container(
    owner: &DomRealm,
    node: Option<NodeId>,
    budget: Arc<ByteBudget>,
) -> OpResult<(Declarations, Origin)> {
    let origin = owner
        .document_origin()
        .ok_or_else(|| OpError::new("InvalidStateError", "container document has no origin"))?;
    let Some(node) = node else {
        return Ok((
            Declarations::empty(&FEATURES, budget).map_err(quota)?,
            origin,
        ));
    };
    let target = declared_origin(owner, node, &origin);
    let session = owner.session.borrow();
    let document = session.document();
    if lumen_html::forms::html_element_local_name(document, node) != Some("iframe") {
        return Ok((
            Declarations::empty(&FEATURES, budget).map_err(quota)?,
            target,
        ));
    }
    let mut declarations = Declarations::parse_attribute(
        document
            .get_attribute_ns_ref(node, None, "allow")
            .map_err(dom_error)?
            .unwrap_or(""),
        &FEATURES,
        true,
        budget,
    )
    .map_err(quota)?;
    if document
        .get_attribute_ns_ref(node, None, "allowfullscreen")
        .map_err(dom_error)?
        .is_some()
    {
        declarations.add_legacy_all(FULLSCREEN);
    }
    Ok((declarations, target))
}
fn inherited(
    parent: &Policy,
    parent_origin: &Origin,
    target: &Origin,
    container: &Declarations,
    declared_origin: &Origin,
) -> [bool; FEATURES.len()] {
    std::array::from_fn(|index| {
        policy::inherit_feature(
            parent.value(index, parent_origin),
            parent.value(index, target),
            container.get(index).map(|list| {
                list.matches(
                    &target.serialize(),
                    target.same_origin(parent_origin),
                    target.same_origin(declared_origin),
                )
            }),
            FEATURES[index].default,
            target.same_origin(parent_origin),
        )
    })
}
impl DomRealm {
    /// The existing first-response-URL seam may replace a provisional native
    /// about:blank identity queried during creation. This runs once, before
    /// parsed bootstrap candidates' actual permission admission; it never runs
    /// for subsequent URL/history changes to the loaded Document.
    pub(crate) fn finalize_initial_permissions_identity(&self) {
        let current=self.permissions_policy.borrow().policies.clone();
        let Some(current)=current else{return;};
        let replacement=(||->OpResult<_>{
            let origin=Arc::new(self.document_origin().ok_or_else(||OpError::new("InvalidStateError","response document has no origin"))?);
            let creation_url=self.permissions_creation_url(&current.budget)?;
            Ok(Rc::new(DocumentPolicies{
                enforce:Policy{origin:origin.clone(),inherited:current.enforce.inherited,declared:current.enforce.declared.clone()},
                report:Policy{origin,inherited:current.report.inherited,declared:current.report.declared.clone()},
                budget:current.budget.clone(),creation_url,
            }))
        })();
        match replacement{
            Ok(replacement)=>self.permissions_policy.borrow_mut().policies=Some(replacement),
            Err(_)=>{if let Some(sender)=self.permissions_policy.borrow().sender.as_ref(){sender.record_failure(scheduling::TaskDiagnosticSource::PolicyViolation,scheduling::TaskDiagnosticCause::ProducerAllocationFailed);}},
        }
    }
    fn permissions_creation_url(
        &self,
        budget: &Arc<ByteBudget>,
    ) -> OpResult<Arc<lumen_common::limits::BudgetedString>> {
        let url = self.document_identity.url.borrow();
        lumen_common::limits::BudgetedString::copy(url.as_deref().unwrap_or("about:blank"), budget)
            .map(Arc::new)
            .map_err(|_| quota(policy::Error::Capacity))
    }
    fn permissions_policies(&self) -> OpResult<Rc<DocumentPolicies>> {
        if let Some(policy) = self.permissions_policy.borrow().policies.as_ref() {
            return Ok(policy.clone());
        }
        let origin = Arc::new(
            self.document_origin()
                .ok_or_else(|| OpError::new("InvalidStateError", "document has no origin"))?,
        );
        let budget = ByteBudget::new(lumen_common::csp::MAX_POLICY_BYTES);
        let creation_url = self.permissions_creation_url(&budget)?;
        let value = Rc::new(DocumentPolicies {
            enforce: Policy {
                origin: origin.clone(),
                inherited: [true; FEATURES.len()],
                declared: None,
            },
            report: Policy {
                origin,
                inherited: [true; FEATURES.len()],
                declared: None,
            },
            budget,
            creation_url,
        });
        self.permissions_policy.borrow_mut().policies = Some(value.clone());
        Ok(value)
    }
    /// Called only for a newly created Document, after its canonical origin and
    /// embedding context are known. Later allow/src mutations affect the next
    /// navigation and iframe introspection, never this immutable snapshot.
    pub(crate) fn initialize_permissions_policy(
        &self,
        parent: &DomRealm,
        node: Option<NodeId>,
    ) -> OpResult<()> {
        let origin = self
            .document_origin()
            .ok_or_else(|| OpError::new("InvalidStateError", "new document has no origin"))?;
        self.lifecycle
            .sandboxed_origin
            .set(sandboxed_origin(parent, node));
        let parent_policy = parent.permissions_policies()?;
        let budget = ByteBudget::new(lumen_common::csp::MAX_POLICY_BYTES);
        let (container, declared_origin) = container(parent, node, budget.clone())?;
        let parent_origin = parent
            .document_origin()
            .ok_or_else(|| OpError::new("InvalidStateError", "parent document has no origin"))?;
        let creation_url = self.permissions_creation_url(&budget)?;
        let enforce = inherited(
            &parent_policy.enforce,
            &parent_origin,
            &origin,
            &container,
            &declared_origin,
        );
        let report = inherited(
            &parent_policy.report,
            &parent_origin,
            &origin,
            &container,
            &declared_origin,
        );
        let origin = Arc::new(origin);
        let value = Rc::new(DocumentPolicies {
            enforce: Policy {
                origin: origin.clone(),
                inherited: enforce,
                declared: None,
            },
            report: Policy {
                origin,
                inherited: report,
                declared: None,
            },
            budget,
            creation_url,
        });
        self.permissions_policy.borrow_mut().policies = Some(value);
        Ok(())
    }
    pub(crate) fn set_permissions_policy_headers(
        &self,
        headers: &[(String, String)],
    ) -> OpResult<()> {
        if !headers.iter().any(|(name, _)| {
            name.eq_ignore_ascii_case("Permissions-Policy")
                || name.eq_ignore_ascii_case("Permissions-Policy-Report-Only")
        }) {
            return Ok(());
        }
        let current = self.permissions_policies()?;
        let enforce =
            Declarations::parse_response(headers, false, &FEATURES, current.budget.clone())
                .map_err(quota)?;
        let report = Declarations::parse_response(headers, true, &FEATURES, current.budget.clone())
            .map_err(quota)?;
        let value = Rc::new(DocumentPolicies {
            enforce: Policy {
                origin: current.enforce.origin.clone(),
                inherited: current.enforce.inherited,
                declared: Some(Arc::new(enforce)),
            },
            report: Policy {
                origin: current.report.origin.clone(),
                inherited: current.report.inherited,
                declared: Some(Arc::new(report)),
            },
            budget: current.budget.clone(),
            creation_url: current.creation_url.clone(),
        });
        self.permissions_policy.borrow_mut().policies = Some(value);
        Ok(())
    }
    pub(crate) fn permissions_policy_allows(&self, index: usize) -> bool {
        let Some(origin) = self.document_origin() else {
            return false;
        };
        self.browsing_context()
            .is_some_and(|context| browsing_context::is_active_document(&context, self))
            && self
                .permissions_policies()
                .is_ok_and(|policy| policy.enforce.allows(index, &origin))
    }
    /// Actual feature use (§9.10), distinct from silent policy introspection.
    /// A captured HTML TaskSender permits parser/DOM insertion consumers to
    /// queue in the owning realm without a second queue or retained DOM clone.
    pub(crate) fn use_permissions_policy(
        self: &Rc<Self>,
        ctx: Option<&mut Ctx>,
        index: usize,
    ) -> bool {
        let Some(origin) = self.document_origin() else {
            return false;
        };
        if !self
            .browsing_context()
            .is_some_and(|context| browsing_context::is_active_document(&context, self))
        {
            return false;
        }
        let Ok(policies) = self.permissions_policies() else {
            return false;
        };
        let allowed = policies.enforce.allows(index, &origin);
        if !allowed || !policies.report.allows(index, &origin) {
            self.queue_permissions_report(ctx, &policies, index, allowed, None);
        }
        allowed
    }
    fn queue_permissions_report(
        self: &Rc<Self>,
        ctx: Option<&mut Ctx>,
        policies: &Rc<DocumentPolicies>,
        index: usize,
        report_only: bool,
        potential: Option<(Option<&str>, Option<&str>)>,
    ) {
        let Some(sender) = self.permissions_policy.borrow().sender.clone() else {
            return;
        };
        let location = ctx.and_then(|ctx| ctx.current_execution_location());
        let source = location.as_ref().map(|(source, _, _)| source.as_str());
        let (allow, src) = potential.unwrap_or((None, None));
        let bytes = std::mem::size_of::<lumen_common::reporting::PermissionsBody>()
            .saturating_add(policies.creation_url.as_str().len())
            .saturating_add(FEATURES[index].name.len())
            .saturating_add(source.map_or(0, str::len))
            .saturating_add(allow.map_or(0, str::len))
            .saturating_add(src.map_or(0, str::len));
        let Some(mut lease) = policies.budget.reserve(bytes) else {
            sender.record_failure(
                scheduling::TaskDiagnosticSource::PolicyViolation,
                scheduling::TaskDiagnosticCause::ProducerAllocationFailed,
            );
            return;
        };
        let body = lumen_common::reporting::PermissionsBody {
            document_url: policies.creation_url.as_str().to_owned(),
            feature_id: FEATURES[index].name.to_owned(),
            source_file: source.map(str::to_owned),
            line_number: location.as_ref().map(|(_, line, _)| *line),
            column_number: location.as_ref().map(|(_, _, column)| *column),
            report_only,
            potential: potential.is_some(),
            allow_attribute: allow.map(str::to_owned),
            src_attribute: src.map(str::to_owned),
        };
        let actual = std::mem::size_of::<lumen_common::reporting::PermissionsBody>()
            + body.document_url.capacity()
            + body.feature_id.capacity()
            + body.source_file.as_ref().map_or(0, String::capacity)
            + body.allow_attribute.as_ref().map_or(0, String::capacity)
            + body.src_attribute.as_ref().map_or(0, String::capacity);
        if actual > bytes && !lease.grow_to(actual) {
            sender.record_failure(
                scheduling::TaskDiagnosticSource::PolicyViolation,
                scheduling::TaskDiagnosticCause::ProducerAllocationFailed,
            );
            return;
        }
        let policy = if report_only {
            &policies.report
        } else {
            &policies.enforce
        };
        let group = policy
            .declared
            .as_ref()
            .and_then(|declarations| declarations.get(index))
            .and_then(|allowlist| allowlist.report_to());
        let group = match group
            .map(|group| lumen_common::limits::BudgetedString::copy(group, &policies.budget))
            .transpose()
        {
            Ok(group) => group,
            Err(_) => {
                sender.record_failure(
                    scheduling::TaskDiagnosticSource::PolicyViolation,
                    scheduling::TaskDiagnosticCause::ProducerAllocationFailed,
                );
                return;
            }
        };
        let weak = Rc::downgrade(self);
        if let Err(failure) = sender.queue(move |ctx| {
            let _lease = lease;
            if let Some(realm) = weak.upgrade() {
                reporting::deliver(
                    &realm,
                    ctx,
                    lumen_common::reporting::ReportBody::Permissions(body),
                    group
                        .as_ref()
                        .map(lumen_common::limits::BudgetedString::as_str),
                );
            }
            Ok(())
        }) {
            sender.record_failure(
                scheduling::TaskDiagnosticSource::PolicyViolation,
                failure.cause,
            );
            drop(failure.callback);
        }
    }
    pub(crate) fn report_container_policy(
        self: &Rc<Self>,
        ctx: &mut Ctx,
        node: NodeId,
    ) -> OpResult<()> {
        let policies = self.permissions_policies()?;
        let budget = ByteBudget::new(lumen_common::csp::MAX_POLICY_BYTES);
        let (container, target) = container(self, Some(node), budget)?;
        let own = self
            .document_origin()
            .ok_or_else(|| OpError::new("InvalidStateError", "container has no origin"))?;
        let enforce = inherited(&policies.enforce, &own, &target, &container, &target);
        let report = inherited(&policies.report, &own, &target, &container, &target);
        let session = self.session.borrow();
        let document = session.document();
        let attributes = (
            document
                .get_attribute_ns_ref(node, None, "allow")
                .map_err(dom_error)?,
            document
                .get_attribute_ns_ref(node, None, "src")
                .map_err(dom_error)?,
        );
        for index in 0..FEATURES.len() {
            if !enforce[index] || !report[index] {
                self.queue_permissions_report(
                    Some(ctx),
                    &policies,
                    index,
                    enforce[index],
                    Some(attributes),
                );
            }
        }
        Ok(())
    }
}

#[lumen_bind::class(name = "PermissionsPolicy", hint(js(webidl)))]
pub(crate) struct DomPermissionsPolicy {
    owner: Value,
    document: bool,
}
impl lumen::embed::NativeIdentityOwner for DomPermissionsPolicy {
    const TRACES_NATIVE_VALUES: bool = true;
    fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) {
        visit(&self.owner);
    }
    fn trace_native_identities(&self, _epoch: u64, _visit: &mut dyn FnMut(&Value)) {}
}
impl DomPermissionsPolicy {
    fn with_policy<T>(
        &self,
        ctx: &mut Ctx,
        apply: impl FnOnce(&Policy) -> OpResult<T>,
    ) -> OpResult<T> {
        let (realm, node) = ctx.with_instance::<DomNode, _>(&self.owner, |node| {
            node.realm.resolve_adopted_node(node.id)
        })?;
        if self.document {
            return apply(&realm.permissions_policies()?.enforce);
        }
        let budget = ByteBudget::new(lumen_common::csp::MAX_POLICY_BYTES);
        let (container, origin) = container(&realm, Some(node), budget)?;
        let parent = realm.permissions_policies()?;
        let parent_origin = realm
            .document_origin()
            .ok_or_else(|| OpError::new("InvalidStateError", "iframe owner has no origin"))?;
        let policy = Policy {
            inherited: inherited(
                &parent.enforce,
                &parent_origin,
                &origin,
                &container,
                &origin,
            ),
            origin: Arc::new(origin),
            declared: None,
        };
        apply(&policy)
    }
}
struct OriginArgument(Option<String>);
impl<'a> lumen_bind::FromArg<'a, lumen::embed::JsHost> for OriginArgument {
    fn from_arg(
        cx: &'a lumen::embed::ArgCx<'_>,
        value: &'a Value,
        _at: lumen_bind::Slot,
    ) -> Result<Self, Value> {
        if matches!(value, Value::Undefined) {
            return Ok(Self(None));
        }
        <lumen::embed::JsHost as lumen_bind::Host>::with_ctx(cx, |ctx| {
            ctx.coerce_string(value)
                .map(|value| Self(Some(value.to_string())))
        })
    }
}
#[lumen_bind::methods]
impl DomPermissionsPolicy {
    #[method(coerce)]
    fn allows_feature(
        &self,
        ctx: &mut Ctx,
        feature: &str,
        #[default(OriginArgument(None))] origin: OriginArgument,
    ) -> OpResult<bool> {
        let Some(index) = FEATURES.iter().position(|item| item.name == feature) else {
            return Ok(false);
        };
        self.with_policy(ctx, |policy| {
            let Some(raw) = origin.0.as_deref() else {
                return Ok(policy.allows(index, &policy.origin));
            };
            if lumen_common::url::parse(raw, None).is_err() {
                return Ok(false);
            }
            Ok(policy.allows(index, &Origin::from_url(raw)))
        })
    }
    fn features(&self) -> Vec<String> {
        FEATURES
            .iter()
            .map(|feature| feature.name.to_owned())
            .collect()
    }
    fn allowed_features(&self, ctx: &mut Ctx) -> OpResult<Vec<String>> {
        self.with_policy(ctx, |policy| {
            Ok(FEATURES
                .iter()
                .enumerate()
                .filter(|(index, _)| policy.allows(*index, &policy.origin))
                .map(|(_, feature)| feature.name.to_owned())
                .collect())
        })
    }
    #[method(coerce)]
    fn get_allowlist_for_feature(&self, ctx: &mut Ctx, feature: &str) -> OpResult<Vec<String>> {
        let Some(index) = FEATURES.iter().position(|item| item.name == feature) else {
            return Ok(Vec::new());
        };
        self.with_policy(ctx, |policy| {
            if !policy.allows(index, &policy.origin) {
                return Ok(Vec::new());
            }
            let Some(list) = policy
                .declared
                .as_ref()
                .and_then(|declarations| declarations.get(index))
            else {
                return Ok(vec![if FEATURES[index].default == DefaultAllowlist::All {
                    "*".to_owned()
                } else {
                    policy.origin.serialize()
                }]);
            };
            if list.all {
                return Ok(vec!["*".to_owned()]);
            }
            let mut result = Vec::new();
            if list.self_origin {
                result.push(policy.origin.serialize());
            }
            result.extend(list.expressions().map(str::to_owned));
            Ok(result)
        })
    }
}
pub(crate) fn object(ctx: &mut Ctx, owner: Value, document: bool) -> OpResult<Value> {
    const SLOT: &str = "#lumen_permissions_policy\u{1}value";
    if let Some(value) = ctx.native_private_value_slot(&owner, SLOT) {
        return Ok(value);
    }
    let value = ctx.new_instance(DomPermissionsPolicy {
        owner: owner.clone(),
        document,
    });
    ctx.set_native_identity_owner::<DomPermissionsPolicy>(&value)?;
    ctx.define_native_internal_value_slot(&owner, SLOT, value.clone())
        .map_err(OpError::thrown)?;
    Ok(value)
}
