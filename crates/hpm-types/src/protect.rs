//! DataProtector: input guards + output hooks over typed values (F2), all pinned
//! to one immutable `ProtectionContext` per call (review edit 3).

use std::any::{Any, TypeId};
use std::collections::BTreeSet;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use crate::BoxFut;
use crate::context::ProgressSink;
use crate::context::RequestContext;
use crate::operation::{OperationSet, Subject};
use crate::tool::{OutputClass, ResultBody, ToolError};

/// Selected versioned rules. Opaque to the dispatcher.
#[derive(Debug)]
pub struct ProtectionRules {
    pub version: u64,
    pub rules: serde_json::Value,
}

/// Built once at admission; never reloaded between channels of one call.
/// `on_unanalyzable` is pinned with the rules (D-06): with `Deny` (default) the
/// dispatcher denies any call whose set holds an effective Execute op
/// (`EXECUTE_UNDER_PROTECTION`, RFC §13 Q2). `PatternsOnly` is the explicit, scoped
/// exception the protector selects for this call; it is audited via the pinned
/// `rules.version`.
#[derive(Debug)]
#[non_exhaustive]
pub struct ProtectionContext {
    pub subject: Subject,
    pub ops: Arc<OperationSet>,
    pub output: OutputClass,
    pub rules: Arc<ProtectionRules>,
    pub on_unanalyzable: OnUnanalyzable,
    /// Counts and ids this call's hooks recorded (PS-5).
    tally: DpTally,
    /// Typed extension slots attached by the protector (PS-5).
    /// `AssertUnwindSafe` keeps `ProtectionContext` `UnwindSafe` as in 7.1.0 (a
    /// `dyn Any` is not); the values are shared and never mutated through this slot.
    state: Vec<(TypeId, AssertUnwindSafe<Arc<dyn Any + Send + Sync>>)>,
}

impl ProtectionContext {
    pub fn new(
        subject: Subject,
        ops: Arc<OperationSet>,
        output: OutputClass,
        rules: Arc<ProtectionRules>,
        on_unanalyzable: OnUnanalyzable,
    ) -> Self {
        Self {
            subject,
            ops,
            output,
            rules,
            on_unanalyzable,
            tally: DpTally::new(),
            state: Vec::new(),
        }
    }

    /// The call's data-protection tally. Created with the context and zero until a
    /// protector records into it; `PassThrough` never touches it.
    pub fn tally(&self) -> &DpTally {
        &self.tally
    }

    /// Attaches one value of type `T` (for example the call's compiled rule set),
    /// replacing an earlier value of the same type. Read it back with [`Self::state`].
    pub fn with_state<T: Any + Send + Sync>(mut self, s: Arc<T>) -> Self {
        let id = TypeId::of::<T>();
        let s = AssertUnwindSafe(s as Arc<dyn Any + Send + Sync>);
        match self.state.iter_mut().find(|(t, _)| *t == id) {
            Some(slot) => slot.1 = s,
            None => self.state.push((id, s)),
        }
        self
    }

    /// The value attached for `T` by [`Self::with_state`], if any.
    pub fn state<T: Any + Send + Sync>(&self) -> Option<&T> {
        let id = TypeId::of::<T>();
        self.state
            .iter()
            .find(|(t, _)| *t == id)
            .and_then(|(_, a)| a.0.downcast_ref::<T>())
    }
}

/// What data protection did to one call: counts and rule ids, never values
/// (audit `dp`, spec §5.8). Collected through a [`DpTally`].
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
#[non_exhaustive]
pub struct DpSummary {
    /// Joined (sorted, `,`) profile names that applied.
    pub profile: Option<String>,
    /// Occurrences masked by a field entry.
    pub masked_field: u32,
    /// Spans masked by a detector.
    pub masked_pattern: u32,
    pub removed: u32,
    pub redacted: u32,
    pub hashed: u32,
    /// 1 when the result was withheld or an input guard denied.
    pub blocked: u32,
    /// `DATA_PROTECTION_*` / `MASKED_VALUE_IN_WRITE` code when blocked.
    pub guard: Option<String>,
    /// Binding and entry ids that fired (at most 64, sorted).
    pub rule_ids: Vec<String>,
    /// Exemptions applied (audited).
    pub exemption_ids: Vec<String>,
}

impl DpSummary {
    /// Field-entry and detector masks together.
    pub fn masked(&self) -> u32 {
        self.masked_field.saturating_add(self.masked_pattern)
    }
}

/// Which counter of a [`DpSummary`] a [`DpTally::add`] raises.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DpKind {
    MaskedField,
    MaskedPattern,
    Removed,
    Redacted,
    Hashed,
    Blocked,
}

const MAX_TALLY_IDS: usize = 64;
const MAX_TALLY_ID_CHARS: usize = 80;

#[derive(Debug, Default)]
struct TallyNames {
    profiles: BTreeSet<String>,
    guard: Option<String>,
    rule_ids: BTreeSet<String>,
    exemption_ids: BTreeSet<String>,
}

/// Interior-mutable, lock-light collector. Counts and ids only; no API accepts a
/// value. Shared by every hook of one call, including progress.
#[derive(Debug, Default)]
pub struct DpTally {
    masked_field: AtomicU32,
    masked_pattern: AtomicU32,
    removed: AtomicU32,
    redacted: AtomicU32,
    hashed: AtomicU32,
    blocked: AtomicU32,
    names: Mutex<TallyNames>,
}

fn note_capped(set: &mut BTreeSet<String>, id: &str) {
    let id: String = id.chars().take(MAX_TALLY_ID_CHARS).collect();
    if set.len() < MAX_TALLY_IDS || set.contains(&id) {
        set.insert(id);
    }
}

impl DpTally {
    pub fn new() -> Self {
        Self::default()
    }

    /// Saturating.
    pub fn add(&self, kind: DpKind, n: u32) {
        let c = match kind {
            DpKind::MaskedField => &self.masked_field,
            DpKind::MaskedPattern => &self.masked_pattern,
            DpKind::Removed => &self.removed,
            DpKind::Redacted => &self.redacted,
            DpKind::Hashed => &self.hashed,
            DpKind::Blocked => &self.blocked,
        };
        let _ = c.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
            Some(v.saturating_add(n))
        });
    }

    fn names(&self) -> std::sync::MutexGuard<'_, TallyNames> {
        // A poisoned lock still holds consistent sets.
        self.names.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Deduplicated; ids past 64 are dropped; each is cut to 80 characters.
    pub fn note_rule(&self, id: &str) {
        note_capped(&mut self.names().rule_ids, id);
    }

    /// Same bounds as [`Self::note_rule`].
    pub fn note_exemption(&self, id: &str) {
        note_capped(&mut self.names().exemption_ids, id);
    }

    /// Adds a profile name that applied; the summary joins them sorted with `,`.
    pub fn set_profile(&self, name: &str) {
        self.names().profiles.insert(name.to_owned());
    }

    /// First writer wins.
    pub fn set_guard(&self, code: &str) {
        let mut n = self.names();
        if n.guard.is_none() {
            n.guard = Some(code.to_owned());
        }
    }

    pub fn snapshot(&self) -> DpSummary {
        let n = self.names();
        let get = |c: &AtomicU32| c.load(Ordering::Relaxed);
        DpSummary {
            profile: (!n.profiles.is_empty()).then(|| {
                n.profiles
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(",")
            }),
            masked_field: get(&self.masked_field),
            masked_pattern: get(&self.masked_pattern),
            removed: get(&self.removed),
            redacted: get(&self.redacted),
            hashed: get(&self.hashed),
            blocked: get(&self.blocked),
            guard: n.guard.clone(),
            rule_ids: n.rule_ids.iter().cloned().collect(),
            exemption_ids: n.exemption_ids.iter().cloned().collect(),
        }
    }
}

/// D-06 profile setting for tools whose accessed tables cannot be resolved statically
/// (scripts, workflows). Owner decision 2026-11-13 (RFC §13 Q2): `Deny` is the default
/// whenever a profile applies; `PatternsOnly` is an explicit, scoped, audited exception.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum OnUnanalyzable {
    #[default]
    Deny,
    PatternsOnly,
}

/// v1: forbidden query operands are rejected, never rewritten (edit 1).
#[derive(Debug)]
#[non_exhaustive]
pub enum InputVerdict {
    Allow,
    Block { reason: String },
}

#[derive(Debug)]
pub struct Protected<T> {
    pub value: T,
    /// Count of values withheld/masked (audited; no values).
    pub withheld: u32,
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProtectError {
    /// Detector/classifier failed or unknown shape under active protection: fail closed.
    #[error("data protection unavailable")]
    Unavailable(#[source] Box<dyn std::error::Error + Send + Sync>),
}

pub trait DataProtector: Send + Sync {
    /// SU default: true → hooks skipped → 6.2.0 bytes.
    fn is_passthrough(&self) -> bool;

    /// Selects and pins the rule version for this call.
    fn pin<'a>(
        &'a self,
        ctx: &'a RequestContext,
        subject: Subject,
        ops: Arc<OperationSet>,
        output: OutputClass,
    ) -> BoxFut<'a, Result<Arc<ProtectionContext>, ProtectError>>;

    /// Whole operation set (returned, written, predicate, ordering fields).
    fn guard_input<'a>(
        &'a self,
        p: &'a ProtectionContext,
        args: &'a serde_json::Value,
    ) -> BoxFut<'a, Result<InputVerdict, ProtectError>>;

    /// `Json`/`Schema` are not exempt; instance-controlled free text is scanned.
    fn protect_result<'a>(
        &'a self,
        p: &'a ProtectionContext,
        body: ResultBody,
    ) -> BoxFut<'a, Result<Protected<ResultBody>, ProtectError>>;

    /// Infallible: must redact or replace with fixed safe text.
    fn protect_error<'a>(
        &'a self,
        p: &'a ProtectionContext,
        err: ToolError,
    ) -> BoxFut<'a, ToolError>;

    /// `None` drops the message.
    fn protect_progress(&self, p: &ProtectionContext, message: String) -> Option<String>;

    fn protect_resource<'a>(
        &'a self,
        p: &'a ProtectionContext,
        body: serde_json::Value,
    ) -> BoxFut<'a, Result<Protected<serde_json::Value>, ProtectError>>;
}

/// The only progress sink a handler can reach after admission: every message passes
/// `protect_progress` under the call's pinned context. Built by the dispatcher at
/// admission (RFC §10.1 step 8). Attaching it to a context needs an
/// [`crate::context::AdmissionKey`].
pub struct ProtectedProgress {
    inner: Arc<dyn ProgressSink>,
    protector: Arc<dyn DataProtector>,
    pinned: Arc<ProtectionContext>,
}

impl ProtectedProgress {
    pub fn new(
        inner: Arc<dyn ProgressSink>,
        protector: Arc<dyn DataProtector>,
        pinned: Arc<ProtectionContext>,
    ) -> Self {
        Self {
            inner,
            protector,
            pinned,
        }
    }
}

impl ProgressSink for ProtectedProgress {
    fn emit(&self, progress: f64, total: Option<f64>, message: Option<String>) -> BoxFut<'_, ()> {
        let message = message.and_then(|m| self.protector.protect_progress(&self.pinned, m));
        self.inner.emit(progress, total, message)
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};

    use super::*;

    fn ctx() -> ProtectionContext {
        ProtectionContext::new(
            Subject::Tool("t"),
            Arc::new(OperationSet::default()),
            OutputClass::None,
            Arc::new(ProtectionRules {
                version: 1,
                rules: serde_json::Value::Null,
            }),
            OnUnanalyzable::Deny,
        )
    }

    #[test]
    fn context_stays_unwind_safe() {
        fn assert_unwind<T: std::panic::UnwindSafe + std::panic::RefUnwindSafe>() {}
        assert_unwind::<ProtectionContext>();
        assert_unwind::<DpTally>();
    }

    #[test]
    fn new_context_has_a_zero_tally() {
        assert_eq!(ctx().tally().snapshot(), DpSummary::default());
        assert_eq!(DpSummary::default().masked(), 0);
    }

    #[test]
    fn state_round_trips_by_type() {
        #[derive(Debug, PartialEq)]
        struct Effective(u32);
        let c = ctx();
        assert!(c.state::<Effective>().is_none());
        let c = c
            .with_state(Arc::new(Effective(7)))
            .with_state(Arc::new(String::from("other")));
        assert_eq!(c.state::<Effective>(), Some(&Effective(7)));
        assert_eq!(c.state::<String>().map(String::as_str), Some("other"));
        assert!(c.state::<u8>().is_none());
        // The same type replaces its slot.
        let c = c.with_state(Arc::new(Effective(8)));
        assert_eq!(c.state::<Effective>(), Some(&Effective(8)));
    }

    #[test]
    fn tally_counts_saturate_and_masked_adds() {
        let t = DpTally::new();
        t.add(DpKind::MaskedField, u32::MAX);
        t.add(DpKind::MaskedField, 5);
        t.add(DpKind::MaskedPattern, 2);
        t.add(DpKind::Removed, 1);
        t.add(DpKind::Redacted, 2);
        t.add(DpKind::Hashed, 3);
        t.add(DpKind::Blocked, 1);
        let s = t.snapshot();
        assert_eq!(s.masked_field, u32::MAX);
        assert_eq!(s.masked(), u32::MAX);
        assert_eq!(
            (s.masked_pattern, s.removed, s.redacted, s.hashed, s.blocked),
            (2, 1, 2, 3, 1)
        );
    }

    #[test]
    fn tally_ids_are_deduped_sorted_capped_and_truncated() {
        let t = DpTally::new();
        t.note_rule("b");
        t.note_rule("a");
        t.note_rule("b");
        assert_eq!(t.snapshot().rule_ids, ["a", "b"]);
        let long = "x".repeat(200);
        t.note_rule(&long);
        assert!(t.snapshot().rule_ids.contains(&"x".repeat(80)));
        for i in 0..100 {
            t.note_rule(&format!("r{i:03}"));
        }
        let ids = t.snapshot().rule_ids;
        assert_eq!(ids.len(), 64);
        assert!(ids.windows(2).all(|w| w[0] < w[1]));
        // An id already present is still accepted at the cap; a new one is dropped.
        t.note_rule("a");
        t.note_rule("brand-new");
        assert_eq!(t.snapshot().rule_ids.len(), 64);
        t.note_exemption("e1");
        t.note_exemption("e1");
        assert_eq!(t.snapshot().exemption_ids, ["e1"]);
    }

    #[test]
    fn tally_profile_joins_and_first_guard_wins() {
        let t = DpTally::new();
        assert_eq!(t.snapshot().profile, None);
        t.set_profile("pii");
        t.set_profile("hr");
        t.set_profile("pii");
        assert_eq!(t.snapshot().profile.as_deref(), Some("hr,pii"));
        t.set_guard("DATA_PROTECTION_A");
        t.set_guard("DATA_PROTECTION_B");
        assert_eq!(t.snapshot().guard.as_deref(), Some("DATA_PROTECTION_A"));
    }

    struct Counting;
    impl DataProtector for Counting {
        fn is_passthrough(&self) -> bool {
            false
        }
        fn pin<'a>(
            &'a self,
            _: &'a RequestContext,
            _: Subject,
            _: Arc<OperationSet>,
            _: OutputClass,
        ) -> BoxFut<'a, Result<Arc<ProtectionContext>, ProtectError>> {
            unreachable!()
        }
        fn guard_input<'a>(
            &'a self,
            _: &'a ProtectionContext,
            _: &'a serde_json::Value,
        ) -> BoxFut<'a, Result<InputVerdict, ProtectError>> {
            unreachable!()
        }
        fn protect_result<'a>(
            &'a self,
            _: &'a ProtectionContext,
            _: ResultBody,
        ) -> BoxFut<'a, Result<Protected<ResultBody>, ProtectError>> {
            unreachable!()
        }
        fn protect_error<'a>(
            &'a self,
            _: &'a ProtectionContext,
            _: ToolError,
        ) -> BoxFut<'a, ToolError> {
            unreachable!()
        }
        fn protect_progress(&self, p: &ProtectionContext, message: String) -> Option<String> {
            p.tally().add(DpKind::MaskedPattern, 1);
            Some(message)
        }
        fn protect_resource<'a>(
            &'a self,
            _: &'a ProtectionContext,
            _: serde_json::Value,
        ) -> BoxFut<'a, Result<Protected<serde_json::Value>, ProtectError>> {
            unreachable!()
        }
    }

    struct Quiet;
    impl ProgressSink for Quiet {
        fn emit(&self, _: f64, _: Option<f64>, _: Option<String>) -> BoxFut<'_, ()> {
            Box::pin(async {})
        }
    }

    #[test]
    fn progress_counts_land_in_the_calls_tally() {
        let pinned = Arc::new(ctx());
        let sink = ProtectedProgress::new(Arc::new(Quiet), Arc::new(Counting), pinned.clone());
        for _ in 0..3 {
            let mut fut = pin!(sink.emit(1.0, None, Some("m".into())));
            let mut cx = Context::from_waker(Waker::noop());
            assert!(matches!(fut.as_mut().poll(&mut cx), Poll::Ready(())));
        }
        assert_eq!(pinned.tally().snapshot().masked_pattern, 3);
    }

    #[test]
    fn summary_serializes_with_contract_field_names() {
        let t = DpTally::new();
        t.add(DpKind::MaskedField, 1);
        let v = serde_json::to_value(t.snapshot()).expect("json");
        for k in [
            "profile",
            "masked_field",
            "masked_pattern",
            "removed",
            "redacted",
            "hashed",
            "blocked",
            "guard",
            "rule_ids",
            "exemption_ids",
        ] {
            assert!(v.get(k).is_some(), "{k}");
        }
    }
}
