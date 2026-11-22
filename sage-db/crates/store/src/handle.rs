//! The owning Store handle: lock, head, coverage, outcome index, snapshot and fence.
//!
//! ## Why this exists
//!
//! `commit` takes a caller-supplied `ManifestFacts`. That is fine for exercising the persistence sequence, but it
//! lets a caller assert digests the candidate does not actually produce — so a free helper cannot demonstrate Store
//! correctness. The handle derives the facts **from the candidate itself**: it projects each event's canonical body,
//! assembles state/graph/bookkeeping, and chains the result against the previous head.
//!
//! ## The canonical bytes are retained, not re-derived
//!
//! Canonical state body bytes come from `projection::project_event` applied to the **raw `OwnedEvent` fields** and
//! are kept across versions. Re-encoding a typed `FoldedRecord` would lose optional-field presence — an absent
//! `Option` and an empty one are different byte streams — so the typed fold is used for records, graph and
//! bookkeeping, and the projection bytes are the canonical-state source.
//!
//! ## What the handle owns
//!
//! * the exclusive lock, held for its lifetime so nothing else can commit;
//! * the capability gate, evaluated once — `product_eligible` is a predicate, so the handle is what makes it matter;
//! * `head`: the committed version this handle last published;
//! * `coverage`: whether the outcome index covers all committed history (drives `Incomplete` vs `Absent`);
//! * the outcome index, for the idempotency-first check;
//! * the retained canonical projection bytes;
//! * a **fence**: once an `Unknown` occurs the handle stops accepting writes. It does not pretend to resolve.
//!
//! Existing-root open, resolution and diagnostic verification share full Genesis replay and integrity checks.
//! Historical reads authenticate actual committed file bytes and retain opaque record owners; no business inference lives here.

use crate::names::EntryName;
use crate::open::{OutcomeIncomplete, OutcomeLookup};
use sage_db_format::projection::project_event;
use sage_db_format::wire::Reader;
use sage_db_types::framing::{FormatOutcome, Limits, Refusal};
use sage_db_types::identity::KbUuid;
use sage_db_types::identity::{AssertionId, TermId};
use sage_db_types::ledger::SharedLedger;
use sage_db_types::persist::{
    ImportSessionRecord, KbMetadataRecord, RecordSet, RequestOutcomeEvent, StateEntry, Watermark,
};
use sage_db_types::prepared::PreparedTransaction;
use sage_db_types::record::{AssertionRecord, TermMeta};

use crate::commit::{
    CommitOutcome, CommitRefusal, CommitTarget, ManifestFacts, RecordedOutcome, commit_parts,
};
use crate::fs::{FileSystem, FsError, FsErrorKind};

/// A construction-time probe step, so a refusal identifies itself.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConstructionStep {
    /// Checking whether `CURRENT` exists.
    ProbeCurrent,
    /// Checking that the initialized families are present.
    ProbeLayout,
}

/// Why a handle refused a commit before attempting one.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum HandleRefusal {
    /// Writes are fenced until a full resolution of the observed root succeeds.
    Fenced,
    /// The backend does not affirmatively support the required primitives with durable sync.
    NotProductEligible,
    /// A construction-time probe failed for a reason other than "no CURRENT yet".
    Filesystem {
        /// Which probe.
        step: ConstructionStep,
        /// The seam's error, intact.
        error: FsError,
    },
    /// The root already has committed history and this handle has indexed none of it.
    ///
    /// Real reopen is P1-5; until it exists, constructing an empty handle over history would make "absent" a false
    /// answer about outcomes the store never read.
    ExistingRootUnsupported,
    /// The request id is already recorded with the same digest: return the stored outcome, do not commit again.
    AlreadyCommitted {
        /// The version that recorded it.
        version: u64,
    },
    /// The request id is recorded with a different digest.
    ///
    /// A reuse of an idempotency key for different content, never a retry.
    IdempotencyMismatch,
    /// The outcome index does not cover all committed history, so "absent" cannot be concluded.
    OutcomeIndexIncomplete,
    /// The candidate belongs to a different KB.
    WrongKb {
        /// This handle's KB.
        expected: KbUuid,
        /// What the candidate claims.
        found: KbUuid,
    },
    /// The caller's stated expectation is not the current head.
    ExpectVersionMismatch {
        /// What the caller expected.
        expected: u64,
        /// The actual head; `None` before the first commit.
        found: Option<u64>,
    },
    /// The head is at `u64::MAX`, so no further version exists.
    VersionOverflow {
        /// The head that cannot advance.
        head: u64,
    },
    /// The candidate's version is not the head's successor.
    VersionConflict {
        /// Version the caller expected to follow.
        expected: u64,
        /// What the candidate claims.
        found: u64,
    },
    /// The store could not finish deriving the candidate's facts.
    Derivation(CommitRefusal),
    /// The retained extent could not be represented in `u64`.
    ///
    /// An arithmetic limit: no allocation was attempted and no allowance consulted, so this is neither an allocation
    /// failure nor budget exhaustion.
    ExtentOverflow,
    /// The pin owner's layout could not be computed, so nothing was allocated and nothing reserved.
    PinLayout,
    /// The retained payload was not already admitted on this ledger.
    ///
    /// Not budget: no allowance would produce credit that was never admitted. Not allocation: no allocator was consulted.
    PinTransfer(crate::lease::TransferError),
    /// Derived history framing could not be prepared before persistence.
    History(crate::open::OpenRefusal),
}

/// The outcome of one commit attempt through the handle.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum HandleOutcome {
    /// This attempt reached the durable point.
    Committed {
        /// Version committed.
        version: u64,
        /// What the transaction recorded; genesis records none.
        recorded: RecordedOutcome,
    },
    /// The transaction was already committed earlier; the stored outcome is returned without any write.
    AlreadyCommitted {
        /// The original version.
        version: u64,
        /// The original outcome.
        recorded: RecordedOutcome,
    },
    /// Attempted authority is definitely unchanged.
    NotCommitted {
        /// Why.
        reason: HandleRefusal,
    },
    /// Attempted authority is ambiguous. **The handle is now fenced** and this is not a resolution.
    Unknown,
}

/// One committed version, as the handle remembers it.
///
/// Keeps only what a real caller reads: the chain digest for the next version's chain and the manifest name/hash for
/// predecessor naming. `entries`, `watermarks` and `snapshot` were previously retained so that a lease could account for
/// them — circular reasoning, since keeping storage alive to justify its own token is not retention. Their RAM copies are
/// dropped here; the committed files and the physical fold/disk history are untouched, so nothing authoritative is lost.
#[derive(Debug)]
struct Committed {
    /// Chain digest after this version.
    chain_digest: [u8; 32],
    /// The manifest file name this version published, retained so the next version can name its predecessor
    /// without re-deriving it.
    manifest_name: String,
    _manifest_retained: crate::lease::OwnedReservation,
    /// That manifest's file hash, retained for the same reason.
    manifest_sha256: [u8; 32],
}
/// The owning Store handle.
pub struct Store<F: FileSystem> {
    fs: F,
    root: F::Dir,
    families: crate::commit::Families<F>,
    /// Held for the handle's lifetime; dropping it releases the KB lock.
    _lock: F::Lock,
    kb_uuid: KbUuid,
    head: Option<u64>,
    /// The LAST committed version's predecessor metadata.
    ///
    /// The vector kept every row and grew geometrically while one slot was charged, and only `.last()` was ever read.
    /// Keeping the single value is the real caller shape. This is RAM only: committed files and the physical fold/disk
    /// revision history are untouched, so no committed revision is lost.
    last_committed: Option<Committed>,
    /// Whether the index covers the committed history represented by this handle.
    /// The public getter additionally requires CURRENT authority to be decided.
    coverage_complete: bool,
    fenced: bool,
    /// `Unknown` was returned, so no current disk authority is established.
    authority_undecided: bool,
    /// A verification failure was terminal. Serving stops and a later verify cannot clear it.
    terminal_corrupt: bool,
    /// The observation this handle's current basis was published from, when it has one.
    ///
    /// Held so a historical search runs over the SAME basis the view serves — never a second, freshly-read observation.
    last_persist_stop: Option<sage_db_types::query::ControlStop>,
    /// The physical fold: ONE authority for current records, graph coverage, high water marks and cumulative
    /// outcomes. Deriving graph or counters from `prepared.events` alone would drop records committed earlier, which
    /// is exactly the bug a per-candidate view has.
    fold: crate::fold::StateFold,
    /// The published immutable pin for the current version, if one has been published.
    ///
    /// This is the OWNER: a reader retains an old pin by cloning this handle and the pin keeps its own reservation until
    /// the last clone drops. A stored `SnapshotId` is identity metadata and is not a substitute.
    pin: Option<crate::pin::Snapshot>,
    /// THE current publication basis: one tagged view, or none.
    ///
    /// `view`, the legacy accessors, coverage, the write gate and the outcome query all derive from this one slot, so they
    /// cannot disagree. A `Durable` tag here is only ever set by a successful whole-chain sync.
    current_view: Option<crate::view::TaggedView>,
    /// Owned derived transaction framing, on the same committed chain as the publication.
    history_frames: Option<crate::history::HistoryFrame>,
}

/// Borrowed files and the durable basis a backup must bind before selecting a version.
/// This exposes no new public storage handle; the backup borrows the locked Store.
pub(crate) struct BackupSource<'a, F: FileSystem> {
    pub(crate) fs: &'a F,
    pub(crate) root: &'a F::Dir,
    pub(crate) families: &'a crate::commit::Families<F>,
    pub(crate) basis: crate::view::ViewBasis,
}

impl<F: FileSystem> Store<F> {
    /// Borrow the existing locked source only when its canonical read gate is durable.
    pub(crate) fn backup_source(&self) -> Result<BackupSource<'_, F>, OutcomeIncomplete> {
        Ok(BackupSource {
            fs: &self.fs,
            root: &self.root,
            families: &self.families,
            basis: self.durable_read_basis()?,
        })
    }

    /// Source read failures withdraw serving just as diagnostic verify does.
    /// Destination-copy failures must never call this source-only hook.
    pub(crate) fn backup_source_refused(&mut self, error: &crate::open::OpenRefusal) {
        if error.is_corrupt() {
            self.mark_terminal_corrupt();
        } else {
            self.withdraw_current();
        }
    }

    /// Construct a handle for an EXISTING root that already holds committed history.
    ///
    /// Distinct from [`Store::open_initializing`], which refuses a root with a CURRENT because it exists to wrap a proven-fresh
    /// root. Here a CURRENT is expected: the caller has already observed and verified the chain, and replay will populate the
    /// fold before anything is served. No view is published until that succeeds.
    ///
    /// # Errors
    ///
    /// Acquisition already established eligibility, identity and the held lock; this constructor publishes nothing.
    pub(crate) fn open_reopening(
        fs: F,
        root: F::Dir,
        families: crate::commit::Families<F>,
        lock: F::Lock,
        kb_uuid: KbUuid,
    ) -> Self {
        Self {
            fs,
            root,
            families,
            _lock: lock,
            kb_uuid,
            head: None,
            last_committed: None,
            // Coverage is established by replay, not asserted here: a handle that has replayed nothing yet covers nothing.
            coverage_complete: false,
            fenced: false,
            authority_undecided: false,
            terminal_corrupt: false,
            last_persist_stop: None,
            fold: crate::fold::StateFold::new(),
            pin: None,
            current_view: None,
            history_frames: None,
        }
    }

    /// Construct a handle for a root that is still being initialized.
    ///
    /// Deliberately NOT public and NOT ready: coverage stays false, so this handle cannot answer ordinary requests and
    /// cannot report definitive absence. Only [`crate::init::bootstrap`] reaches it, holding a lock it never released.
    ///
    /// # Errors
    ///
    /// `NotProductEligible` for a backend that cannot honour durability; `Filesystem` for a probe that failed for a reason
    /// other than "no CURRENT yet".
    pub(crate) fn open_initializing(
        fs: F,
        root: F::Dir,
        families: crate::commit::Families<F>,
        lock: F::Lock,
        kb_uuid: KbUuid,
    ) -> Result<Self, HandleRefusal> {
        if !fs.capabilities().product_eligible() {
            return Err(HandleRefusal::NotProductEligible);
        }
        // NOTE: no CURRENT/family inspection here. Freshness was proven by `prepare_fresh` under the lock this handle now
        // owns; re-deriving it from an absent CURRENT plus present families is the check the ruling rejects, because a
        // torn non-empty root looks identical.
        // A root with a CURRENT has committed history, so an empty handle over it would answer "absent" for
        // outcomes it never read. Only `NotFound` establishes that no CURRENT exists: treating every error as
        // absence would turn PermissionDenied/Io into "fresh", which is the same defect as a false absent.
        match fs.size(
            &root,
            crate::names::EntryName::Root(crate::names::RootName::Current),
        ) {
            Ok(_size) => return Err(HandleRefusal::ExistingRootUnsupported),
            Err(error) if error.kind() == FsErrorKind::NotFound => {}
            Err(error) => {
                return Err(HandleRefusal::Filesystem {
                    step: ConstructionStep::ProbeCurrent,
                    error,
                });
            }
        }
        // The V3 layout must already exist: an arbitrary CURRENT-less directory is not a fresh KB.
        for family in crate::commit::required_families() {
            if let Err(error) = fs.open_family(&root, family, crate::fs::CreateDir::Never) {
                return Err(HandleRefusal::Filesystem {
                    step: ConstructionStep::ProbeLayout,
                    error,
                });
            }
        }
        Ok(Self {
            fs,
            root,
            families,
            _lock: lock,
            kb_uuid,
            head: None,
            last_committed: None,
            // Genesis must actually commit before the initializing handle claims coverage.
            coverage_complete: false,
            fenced: false,
            // Attempted authority is decided from the moment the handle exists; only an ambiguous outcome clears it.
            authority_undecided: false,
            terminal_corrupt: false,
            last_persist_stop: None,
            fold: crate::fold::StateFold::new(),
            // A fresh handle has an empty current set; it is not indexed against prior history (P1-5 owns open).
            pin: None,
            current_view: None,
            history_frames: None,
        })
    }

    /// The version this handle last committed, or `None` before its first commit.
    #[must_use]
    pub const fn head(&self) -> Option<u64> {
        self.head
    }

    /// Declare the handle ready after an actual Genesis commit.
    ///
    /// `pub(crate)` and separate from construction so readiness cannot be asserted by a constructor: it is a
    /// consequence of the commit, not of the container looking empty.
    pub(crate) fn mark_ready(&mut self) {
        self.coverage_complete = true;
    }

    /// The published pin for the current version, if any.
    ///
    /// This is the contract-bound way a reader retains the old pin: clone the returned handle, and its reservation survives
    /// until the last clone drops — including after this `Store` is gone.
    #[must_use]
    pub fn published_pin(&self) -> Option<crate::pin::Snapshot> {
        // ONE publication basis, and legacy accessors are DURABLE-ONLY: they read the tagged view slot and refuse to serve an
        // `Unverified` pin as if it were the current durable authority. The raw materialized pin is never exposed here.
        //
        // K12/6.2: while attempted authority is UNDECIDED there is no current view at all, so this answers None; previously
        // issued pins stay alive and immutable for whoever already holds one.
        if self.authority_undecided {
            return None;
        }
        self.current_view
            .as_ref()
            .filter(|view| view.durability() == crate::view::ViewDurability::Durable)
            .map(crate::view::TaggedView::snapshot_owned)
    }

    /// Whether attempted authority is undecided after an ambiguous commit outcome.
    ///
    /// Distinct from [`Self::is_fenced`]: fencing stops writes, this hides the current view. Both come from `Unknown`, but
    /// K12b's orphan-health case is a different reason and must not be collapsed into the same condition.
    #[must_use]
    pub const fn authority_undecided(&self) -> bool {
        self.authority_undecided
    }

    /// The first caller stop noticed in the most recent persistence attempt.
    ///
    /// Diagnostic only: it does not mean the attempt aborted, and a pre-persistence refusal does not erase it.
    #[must_use]
    pub const fn last_persist_stop(&self) -> Option<sage_db_types::query::ControlStop> {
        self.last_persist_stop
    }

    /// Publish a completed replay: adopt the fold, build the pin from the real current set, expose the view slot.
    ///
    /// Separate from replay so `verify` can run the same engine WITHOUT publishing anything.
    fn publish_materialized(
        &mut self,
        mut fold: crate::fold::StateFold,
        current_set: Vec<StateEntry>,
        observed: &crate::open::ObservedRoot,
        history_frames: Option<crate::history::HistoryFrame>,
        ledger: &mut dyn crate::lease::LeaseLedger,
    ) -> Result<(), crate::open::OpenRefusal> {
        use crate::open::{CorruptReason, OpenRefusal};
        let tip = observed
            .chain
            .first()
            .ok_or(OpenRefusal::Corrupt(CorruptReason::VersionDiscontinuity))?;
        if observed.current.kb_uuid != self.kb_uuid {
            return Err(OpenRefusal::IdentityMismatch {
                expected: self.kb_uuid,
                found: observed.current.kb_uuid,
            });
        }
        let profile = sage_db_types::relation::RuleProfile::from_u16(tip.rules_profile).ok_or(
            OpenRefusal::Unsupported(crate::open::UnsupportedReason::RulesProfile(
                tip.rules_profile,
            )),
        )?;
        let snapshot = sage_db_types::graph::SnapshotId {
            kb_uuid: self.kb_uuid,
            version: sage_db_types::identity::KbVersion::new(observed.current.version),
            rules: sage_db_types::relation::RuleSet::from_trusted_parts(
                profile,
                sage_db_types::digest::RulesDigest::new(sage_db_types::digest::Digest::from_bytes(
                    tip.rules_digest,
                )),
            ),
            graph_digest: sage_db_types::digest::GraphDigest::new(
                sage_db_types::digest::Digest::from_bytes(tip.graph_digest),
            ),
        };
        let name = crate::names::render_name_bounded(tip.manifest_name, ledger)
            .map_err(OpenRefusal::Refused)?;
        let retained = ledger
            .adopt_retained(
                u64::try_from(name.capacity()).map_err(|_| OpenRefusal::ExtentOverflow)?,
            )
            .map_err(OpenRefusal::PinTransfer)?;
        let previous = Committed {
            chain_digest: tip.chain_digest,
            manifest_name: name,
            _manifest_retained: retained,
            manifest_sha256: tip.manifest_sha256,
        };
        let pin =
            crate::pin::prepare(ledger, snapshot, current_set).map_err(crate::open::pin_failure)?;
        match fold.adopt_container(ledger) {
            crate::fold::FoldOutcome::Ok(()) => {}
            crate::fold::FoldOutcome::Refused(r) => return Err(OpenRefusal::Refused(r)),
            crate::fold::FoldOutcome::Defect(e) => return Err(crate::open::fold_failure(e)),
        }
        // Every fallible preparation completed. These moves publish one coherent replayed basis.
        let view = crate::view::TaggedView::new(
            pin.clone(),
            crate::view::ViewDurability::Unverified,
            observed.basis(),
        );
        self.head = Some(observed.current.version);
        self.last_committed = Some(previous);
        self.fold = fold;
        self.history_frames = history_frames;
        self.pin = Some(pin);
        self.current_view = Some(view);
        self.coverage_complete = false;
        self.authority_undecided = false;
        self.fenced = true;
        Ok(())
    }

    /// Verify the INTEGRITY of the root this handle serves, without changing anything.
    ///
    /// Same observation, chain validation, full replay and digest comparison the open path uses — but this performs **no**
    /// authoritative write, no repair, no sync-for-readiness, no root substitution and no `Ready` promotion. It answers whether
    /// the observed bytes are internally consistent, which is a different claim from "durably committed".
    ///
    /// Corruption found here invalidates normal current serving; a later successful diagnostic verify does **not** clear that
    /// terminal state.
    ///
    /// # Errors
    ///
    /// [`crate::open::OpenRefusal`] with its real class: budget/stop stay canonical, I/O stays `Filesystem`, unsupported stays
    /// distinct from corruption.
    pub fn verify(
        &mut self,
        limits: &Limits,
        ledger: &mut dyn crate::lease::LeaseLedger,
    ) -> Result<(), crate::open::OpenRefusal> {
        let result: Result<(), crate::open::OpenRefusal> = (|| {
            let observed =
                crate::open::observe_root(&self.fs, &self.root, &self.families, limits, ledger)?;
            self.check_observed_identity(&observed)?;
            replay_into_fold(&self.fs, &self.families, &observed.chain, limits, ledger)?;
            Ok(())
        })();
        if let Err(error) = &result {
            if error.is_corrupt() {
                self.mark_terminal_corrupt();
            } else {
                self.withdraw_current();
            }
        }
        result
    }

    fn check_observed_identity(
        &self,
        observed: &crate::open::ObservedRoot,
    ) -> Result<(), crate::open::OpenRefusal> {
        if observed.current.kb_uuid != self.kb_uuid {
            return Err(crate::open::OpenRefusal::IdentityMismatch {
                expected: self.kb_uuid,
                found: observed.current.kb_uuid,
            });
        }
        Ok(())
    }

    /// Withdraw fresh current claims; explicitly issued pins remain independent owners.
    fn withdraw_current(&mut self) {
        self.fenced = true;
        self.current_view = None;
        self.coverage_complete = false;
    }

    fn mark_terminal_corrupt(&mut self) {
        self.terminal_corrupt = true;
        self.withdraw_current();
    }

    /// Whether a terminal corruption verdict is in force.
    #[must_use]
    pub const fn is_terminal_corrupt(&self) -> bool {
        self.terminal_corrupt
    }

    /// Resolve an already-open handle whose authority is undecided.
    ///
    /// One fresh observation of CURRENT this attempt, the same chain validation, the same full replay engine and the same
    /// ordered whole-chain sync the open path uses. This is the ONLY supported way to leave an undecided state: there is no
    /// "clear the flag" shortcut, because a flag would be a second authority beside CURRENT.
    ///
    /// A successful resolution makes the CURRENT-named root Ready — old or new. It does **not** mean the unknown request
    /// committed; that is answered only by [`Store::outcome`] under G0.
    pub fn resolve(
        &mut self,
        limits: &Limits,
        ledger: &mut dyn crate::lease::LeaseLedger,
    ) -> crate::open::Resolution {
        use crate::open::{OpenRefusal, Resolution};
        if self.terminal_corrupt {
            return Resolution::Corrupt(crate::open::CorruptReason::TerminalCorrupt);
        }
        // A new resolution may not answer from the previous observation if any step refuses.
        self.withdraw_current();
        let prepared = (|| {
            let mut observed =
                crate::open::observe_root(&self.fs, &self.root, &self.families, limits, ledger)?;
            self.check_observed_identity(&observed)?;
            let (fold, current_set, history_frames) =
                replay_into_fold(&self.fs, &self.families, &observed.chain, limits, ledger)?;
            observed.referenced_blobs = committed_blob_union(&fold, ledger)?;
            self.publish_materialized(fold, current_set, &observed, history_frames, ledger)?;
            Ok::<_, OpenRefusal>(observed)
        })();
        let observed = match prepared {
            Ok(observed) => observed,
            Err(error) => {
                if error.is_corrupt() {
                    self.mark_terminal_corrupt();
                }
                return map_open_refusal(error);
            }
        };
        match sync_observed(self, &observed, ledger) {
            Ok(()) => match self.promote_view_durable(observed.basis()) {
                Ok(()) => Resolution::Ready,
                Err(error) => {
                    self.withdraw_current();
                    map_open_refusal(error)
                }
            },
            Err((step, cause)) => Resolution::VerifiedUnsynced { step, cause },
        }
    }

    /// Bounded historical revision lookup over the replayed committed chain.
    ///
    /// Newest-first with both `budget.max_events` and the caller's ledger enforced. Absence requires having searched the whole
    /// materialized chain; a bound or ledger refusal is `Incomplete`, never `Absent`.
    #[must_use]
    pub fn history(
        &mut self,
        identity: &crate::fold::RecordIdentity,
        revision: u64,
        budget: crate::history::HistoryBudget,
        limits: &Limits,
        ledger: &mut dyn crate::lease::LeaseLedger,
    ) -> crate::history::HistoryOutcome {
        use crate::history::{HistoryIncomplete, HistoryOutcome};
        if self.terminal_corrupt {
            return HistoryOutcome::Error(crate::open::OpenRefusal::Corrupt(
                crate::open::CorruptReason::TerminalCorrupt,
            ));
        }
        if let Err(stop) = ledger.checkpoint() {
            return HistoryOutcome::Incomplete {
                reason: HistoryIncomplete::Refused(Refusal::Stopped(stop)),
                scanned_events: 0,
            };
        }
        let Some(view) = self.view() else {
            return HistoryOutcome::Incomplete {
                reason: HistoryIncomplete::NoCurrentView,
                scanned_events: 0,
            };
        };
        let outcome = crate::history::lookup(
            &self.fs,
            &self.families,
            &self.fold,
            self.history_frames.as_ref(),
            view.basis(),
            view.durability(),
            identity,
            revision,
            budget,
            limits,
            ledger,
        );
        if let HistoryOutcome::Error(error) = &outcome
            && error.is_corrupt()
        {
            self.mark_terminal_corrupt();
        }
        outcome
    }

    /// Look up a committed request's original outcome, under G0.
    ///
    /// The answer is only definitive when this handle holds a **Durable** current basis: the same root validated, fully replayed
    /// and synced, with complete coverage. Anything weaker refuses to claim membership:
    ///
    /// * an `Unverified` view is **not** `Found` — its membership was never made durable;
    /// * unresolved absence is **not** `Absent` — coverage is what makes absence meaningful.
    ///
    /// # Errors
    ///
    /// Never `Err`: absence and incompleteness are answers.
    #[must_use]
    pub fn outcome(&self, principal_id: &str, request_id: &str) -> OutcomeLookup {
        use crate::view::ViewDurability;

        // G0 gate. Attempted-authority undecided is checked FIRST: even if a slot survived, authority is what makes an
        // answer meaningful, and asserting membership from a handle that does not know its own CURRENT would be a lie.
        if self.authority_undecided {
            return OutcomeLookup::Incomplete(OutcomeIncomplete::NoCurrentView);
        }
        // A durable basis is required before either `Found` or `Absent` may be claimed.
        let basis = match self.current_view.as_ref() {
            Some(view) if view.durability() == ViewDurability::Durable => view.basis(),
            Some(_) => {
                return OutcomeLookup::Incomplete(OutcomeIncomplete::NotDurable);
            }
            None => return OutcomeLookup::Incomplete(OutcomeIncomplete::NoCurrentView),
        };
        if !self.coverage_complete {
            return OutcomeLookup::Incomplete(OutcomeIncomplete::CoverageIncomplete);
        }
        match self.fold.outcome(principal_id, request_id) {
            Some(recorded) => OutcomeLookup::Found {
                outcome: recorded.event.clone(),
                original_version: recorded.commit_version,
                basis,
            },
            None => OutcomeLookup::Absent { basis },
        }
    }

    /// Read a complete typed current after-image on the published durable basis.
    ///
    /// The same authority/durability/coverage gates as outcome lookup apply to
    /// both presence and absence. No filesystem work or mutation occurs. Every
    /// searched retained row uses the caller's ledger; the returned borrow
    /// prevents a concurrent commit through this handle.
    ///
    /// # Errors
    /// Invalid record identity or the caller's canonical work/stop refusal.
    pub fn record(
        &self,
        key: sage_db_types::persist::RecordKey,
        ledger: &mut dyn SharedLedger,
    ) -> Result<crate::view::RecordLookup<'_>, crate::view::RecordReadError> {
        use crate::view::{RecordLookup, RecordReadError, ViewDurability};
        ledger
            .checkpoint()
            .map_err(|e| RecordReadError::Refused(Refusal::Stopped(e)))?;
        if !key.id_is_in_range() {
            return Err(RecordReadError::InvalidKey(key));
        }
        if self.authority_undecided {
            return Ok(RecordLookup::Incomplete(OutcomeIncomplete::NoCurrentView));
        }
        let basis = match self.current_view.as_ref() {
            Some(view) if view.durability() == ViewDurability::Durable => view.basis(),
            Some(_) => return Ok(RecordLookup::Incomplete(OutcomeIncomplete::NotDurable)),
            None => return Ok(RecordLookup::Incomplete(OutcomeIncomplete::NoCurrentView)),
        };
        if !self.coverage_complete {
            return Ok(RecordLookup::Incomplete(
                OutcomeIncomplete::CoverageIncomplete,
            ));
        }
        match self.fold.identified_current_bounded(key, ledger)? {
            Some(record) => Ok(RecordLookup::Found { record, basis }),
            None => Ok(RecordLookup::Absent { basis }),
        }
    }

    /// Share a complete current after-image through its EXISTING retained owner.
    /// This uses the same bounded physical lookup as record(); no deep copy or
    /// new allocation occurs, and dropping the attempt or Store cannot uncharge
    /// a still-held image. The output is tied to one actual durable basis.
    ///
    /// # Errors
    /// Invalid numeric key, real caller refusal or a retained-index defect.
    pub fn record_owned(
        &self,
        key: sage_db_types::persist::RecordKey,
        ledger: &mut dyn SharedLedger,
    ) -> Result<crate::view::RecordImageLookup, crate::view::RecordReadError> {
        use crate::view::{RecordImageLookup, RecordReadError};
        ledger
            .checkpoint()
            .map_err(|e| RecordReadError::Refused(Refusal::Stopped(e)))?;
        if !key.id_is_in_range() {
            return Err(RecordReadError::InvalidKey(key));
        }
        let basis = match self.durable_read_basis() {
            Ok(basis) => basis,
            Err(reason) => return Ok(RecordImageLookup::Incomplete(reason)),
        };
        match self.fold.identified_image_bounded(key, ledger)? {
            Some(image) => Ok(RecordImageLookup::Found { image, basis }),
            None => Ok(RecordImageLookup::Absent { basis }),
        }
    }

    /// Visit complete current after-images with numeric record identities on ONE
    /// durable basis. Retired/retracted records are included; a consumer applies
    /// its own activity predicate. Natural-key metadata (rules, models and KB
    /// metadata) is outside this explicitly identified-record API.
    ///
    /// The immutable Store borrow prevents publication changes throughout the
    /// visitor. This method allocates no table or record copy. Every traversal
    /// and retained-owner comparison uses the SAME caller ledger. Working output
    /// built by the visitor must be discarded if the scan fails.
    ///
    /// # Errors
    /// Incomplete authority, genuine caller refusal/index defects, or the exact
    /// visitor error. Never a successful partial traversal.
    pub fn scan_identified_records<'a, E>(
        &'a self,
        ledger: &mut dyn SharedLedger,
        visitor: impl FnMut(&'a crate::fold::FoldedRecord, &mut dyn SharedLedger) -> Result<(), E>,
    ) -> Result<crate::view::ViewBasis, crate::view::RecordScanError<E>> {
        use crate::view::{RecordReadError, RecordScanError};
        ledger
            .checkpoint()
            .map_err(|e| RecordScanError::Read(RecordReadError::Refused(Refusal::Stopped(e))))?;
        let basis = self
            .durable_read_basis()
            .map_err(RecordScanError::Incomplete)?;
        self.fold.visit_identified_current(ledger, visitor)?;
        Ok(basis)
    }

    /// Visit **every** current record, identified or natural, on one durable basis.
    ///
    /// This is a *physical administrative* read surface, not an ordinary query and not
    /// an export. It serves all thirteen record families the fold can hold — including
    /// retired and retracted after-images — and deliberately excludes non-record
    /// bookkeeping (request outcomes, high-water marks, import stage rows, rule
    /// history): those are not records and have no current after-image.
    ///
    /// # Preconditions this method does not enforce
    ///
    /// Authorization is the caller's. The ordinary kernel import guard is deliberately
    /// unchanged, so an administrator must authenticate before using this surface and
    /// must not treat a completed walk as proof that an import finished — presence of
    /// records says nothing about import completion.
    ///
    /// # Sharing
    ///
    /// This and [`Self::scan_identified_records`] both go through one fold traversal
    /// (`crate::fold::StateFold::visit_current`); only the identity selection differs,
    /// so the numeric API keeps its exact previous semantics while the two algorithms
    /// cannot drift. Variable-length model keys are compared through a charged,
    /// interruptible walk, so no key is cloned and no comparison is free.
    ///
    /// # Errors
    ///
    /// Incomplete authority, a genuine caller refusal/stop, an internal index defect
    /// (a current row with no retained owner is named, never skipped), or the exact
    /// visitor error. A scan never reports a successful partial traversal: the caller
    /// must discard whatever it built if this returns `Err`.
    pub fn scan_current_records<'a, E>(
        &'a self,
        ledger: &mut dyn SharedLedger,
        visitor: impl FnMut(&'a crate::fold::FoldedRecord, &mut dyn SharedLedger) -> Result<(), E>,
    ) -> Result<crate::view::ViewBasis, crate::view::RecordScanError<E>> {
        use crate::view::{RecordReadError, RecordScanError};
        ledger
            .checkpoint()
            .map_err(|e| RecordScanError::Read(RecordReadError::Refused(Refusal::Stopped(e))))?;
        let basis = self
            .durable_read_basis()
            .map_err(RecordScanError::Incomplete)?;
        self.fold.visit_all_current(ledger, visitor)?;
        Ok(basis)
    }

    fn durable_read_basis(&self) -> Result<crate::view::ViewBasis, OutcomeIncomplete> {
        use crate::view::ViewDurability;
        if self.authority_undecided {
            return Err(OutcomeIncomplete::NoCurrentView);
        }
        let basis = match self.current_view.as_ref() {
            Some(view) if view.durability() == ViewDurability::Durable => view.basis(),
            Some(_) => return Err(OutcomeIncomplete::NotDurable),
            None => return Err(OutcomeIncomplete::NoCurrentView),
        };
        if !self.coverage_complete {
            return Err(OutcomeIncomplete::CoverageIncomplete);
        }
        Ok(basis)
    }

    /// Inspect persisted import completion using the caller's ledger.
    /// Ordinary kernel operations must refuse while any session is unfinished;
    /// administrative verification and resume retain access to the physical Store.
    /// This method creates no copy and never infers completion from an empty or
    /// interrupted traversal.
    ///
    /// # Errors
    /// Incomplete authority or the real caller work/stop refusal.
    pub fn import_readiness(
        &self,
        ledger: &mut dyn SharedLedger,
    ) -> Result<crate::view::ImportReadiness<'_>, crate::view::CommitReadError> {
        use crate::view::{CommitReadError, ImportReadiness};
        ledger
            .checkpoint()
            .map_err(|e| CommitReadError::Refused(Refusal::Stopped(e)))?;
        let basis = self
            .durable_read_basis()
            .map_err(CommitReadError::Incomplete)?;
        for (_, session) in self.fold.imports() {
            ledger
                .try_charge_entries(1)
                .map_err(|e| CommitReadError::Refused(Refusal::Budget(e)))?;
            ledger
                .checkpoint()
                .map_err(|e| CommitReadError::Refused(Refusal::Stopped(e)))?;
            if !session.record.import_complete {
                return Ok(ImportReadiness::Incomplete {
                    basis,
                    session: &session.record,
                });
            }
        }
        ledger
            .checkpoint()
            .map_err(|e| CommitReadError::Refused(Refusal::Stopped(e)))?;
        Ok(ImportReadiness::Ready { basis })
    }

    /// Read the canonical KB metadata at one definitive durable basis.
    ///
    /// The returned borrow prevents updates through this handle. Applications can
    /// copy the metadata revision and optional legacy Frontier reference, then
    /// read that reference through [`Self::read_committed_blob`] under the same
    /// exclusive Store handle.
    ///
    /// # Errors
    /// An incomplete publication, a caller refusal or a retained-index defect.
    pub fn kb_metadata(
        &self,
        ledger: &mut dyn SharedLedger,
    ) -> Result<(&KbMetadataRecord, crate::view::ViewBasis), crate::view::KbMetadataReadError> {
        use crate::fold::{FixedReadError, FoldedRecord, NaturalKey, RecordIdentity};
        use crate::view::KbMetadataReadError;
        ledger
            .checkpoint()
            .map_err(|e| KbMetadataReadError::Refused(Refusal::Stopped(e)))?;
        let basis = self
            .durable_read_basis()
            .map_err(KbMetadataReadError::Incomplete)?;
        let identity = RecordIdentity::Natural {
            key: NaturalKey::KbMetadata,
        };
        let record = self
            .fold
            .fixed_current_bounded(&identity, ledger)
            .map_err(|error| match error {
                FixedReadError::Refused(refusal) => KbMetadataReadError::Refused(refusal),
                FixedReadError::MissingOwner { revision } => {
                    KbMetadataReadError::IndexInvariant { revision }
                }
            })?
            .ok_or(KbMetadataReadError::Missing)?;
        let FoldedRecord::KbMetadata(metadata) = record else {
            return Err(KbMetadataReadError::WrongRecord);
        };
        if metadata.kb_uuid != basis.kb_uuid {
            return Err(KbMetadataReadError::WrongRecord);
        }
        Ok((metadata, basis))
    }

    /// Read the source package reference when one completed import owns it.
    ///
    /// The reference and absence are selected from the definitive durable fold.
    /// A database with zero or multiple sessions has no unique source package.
    ///
    /// # Errors
    /// Incomplete authority or the caller ledger's exact work or stop refusal.
    pub fn import_package_ref(
        &self,
        ledger: &mut dyn SharedLedger,
    ) -> Result<Option<([u8; 32], crate::view::ViewBasis)>, crate::view::CommitReadError> {
        use crate::view::CommitReadError;
        ledger
            .checkpoint()
            .map_err(|error| CommitReadError::Refused(Refusal::Stopped(error)))?;
        let basis = self
            .durable_read_basis()
            .map_err(CommitReadError::Incomplete)?;
        let [(_, session)] = self.fold.imports() else {
            return Ok(None);
        };
        ledger
            .try_charge_entries(1)
            .map_err(|error| CommitReadError::Refused(Refusal::Budget(error)))?;
        Ok(session
            .record
            .import_complete
            .then_some((session.record.plan_ref, basis)))
    }

    /// Read scalar commit metadata from one actual durable publication.
    /// No caller/compiled rule bytes or guessed blob lengths are substituted.
    /// This read does not bypass the write fence or allocate an identifier.
    ///
    /// # Errors
    /// Incomplete authority, canonical caller refusal or an internal retained-index defect.
    pub fn commit_basis(
        &self,
        space: sage_db_types::persist::IdSpace,
        ledger: &mut dyn SharedLedger,
    ) -> Result<crate::view::CommitBasis, crate::view::CommitReadError> {
        use crate::fold::{FixedReadError, FoldedRecord, NaturalKey, RecordIdentity};
        use crate::view::{CommitBasis, CommitReadError};
        ledger
            .checkpoint()
            .map_err(|e| CommitReadError::Refused(Refusal::Stopped(e)))?;
        let basis = self
            .durable_read_basis()
            .map_err(CommitReadError::Incomplete)?;
        let identity = RecordIdentity::Natural {
            key: NaturalKey::RuleConfig,
        };
        let record = self
            .fold
            .fixed_current_bounded(&identity, ledger)
            .map_err(|e| match e {
                FixedReadError::Refused(r) => CommitReadError::Refused(r),
                FixedReadError::MissingOwner { revision } => {
                    CommitReadError::IndexInvariant { revision }
                }
            })?
            .ok_or(CommitReadError::MissingRules)?;
        let FoldedRecord::RuleConfig(record) = record else {
            return Err(CommitReadError::WrongRulesRecord);
        };
        let rules_len = self
            .fold
            .committed_blob_len_bounded(&record.rules_ref, ledger)
            .map_err(CommitReadError::Refused)?
            .ok_or(CommitReadError::MissingRulesBlob)?;
        Ok(CommitBasis {
            basis,
            rules: sage_db_types::prepared::RulesBinding {
                profile: record.profile,
                rules_digest: record.rules_digest,
                rules_ref: record.rules_ref,
            },
            rules_len,
            // The fixed allocation array's None means no allocation in that space.
            // A business allocator must still verify that its next identity is absent.
            allocated_high_water: self.fold.high_water(space).unwrap_or(0),
        })
    }

    /// Read actual bytes reachable through the current committed-reference union.
    ///
    /// Verifies the returned bytes' raw content hash. The opaque owner retains a
    /// real reservation even after the read attempt facade drops. Both presence
    /// and union absence use one durable publication; this never scans orphans.
    /// Confirmed missing/changed committed content invalidates normal serving.
    ///
    /// # Errors
    /// Incomplete authority, unreferenced digest, configured ceiling, real caller
    /// refusal, exact filesystem error, representation defect, or confirmed damage.
    pub fn read_committed_blob(
        &mut self,
        digest: [u8; 32],
        max_len: u64,
        ledger: &mut dyn crate::lease::LeaseLedger,
    ) -> Result<crate::view::CommittedBlob, crate::view::BlobReadError> {
        use crate::view::{BlobCorruption, BlobReadError, BlobReadStep, CommittedBlob};
        let result = (|| {
            fn step(ledger: &mut dyn SharedLedger) -> Result<(), BlobReadError> {
                ledger
                    .checkpoint()
                    .map_err(|e| BlobReadError::Refused(Refusal::Stopped(e)))?;
                ledger
                    .try_charge_entries(1)
                    .map_err(|e| BlobReadError::Refused(Refusal::Budget(e)))
            }
            ledger
                .checkpoint()
                .map_err(|e| BlobReadError::Refused(Refusal::Stopped(e)))?;
            let basis = self
                .durable_read_basis()
                .map_err(BlobReadError::Incomplete)?;
            let expected = self
                .fold
                .committed_blob_len_bounded(&digest, ledger)
                .map_err(BlobReadError::Refused)?
                .ok_or(BlobReadError::NotReferenced(digest))?;
            let name = EntryName::Blob(digest);
            step(ledger)?;
            let size = self.fs.size(&self.families.blobs, name).map_err(|error| {
                if error.kind() == FsErrorKind::NotFound {
                    BlobReadError::Corrupt(BlobCorruption::Missing)
                } else {
                    BlobReadError::Filesystem {
                        step: BlobReadStep::Size,
                        error,
                    }
                }
            })?;
            if size != expected {
                return Err(BlobReadError::Corrupt(BlobCorruption::Length {
                    expected,
                    actual: size,
                }));
            }
            if size > max_len {
                return Err(BlobReadError::Ceiling {
                    size,
                    ceiling: max_len,
                });
            }
            let len = usize::try_from(size).map_err(|_| BlobReadError::ExtentOverflow)?;
            core::alloc::Layout::array::<u8>(len).map_err(|_| BlobReadError::ExtentOverflow)?;
            // Reserve before allocating, keeping the owner alive beyond facade drop.
            let retained = ledger
                .reserve_owned(size)
                .map_err(|e| BlobReadError::Refused(Refusal::Budget(e)))?;
            ledger
                .checkpoint()
                .map_err(|e| BlobReadError::Refused(Refusal::Stopped(e)))?;
            let mut bytes = Vec::new();
            bytes
                .try_reserve_exact(len)
                .map_err(|_| BlobReadError::Refused(Refusal::Allocation { requested: len }))?;
            if bytes.capacity() != len {
                return Err(BlobReadError::UnexpectedCapacity {
                    requested: len,
                    actual: bytes.capacity(),
                });
            }
            bytes.resize(len, 0);
            step(ledger)?;
            let actual = self
                .fs
                .read_exact_into(&self.families.blobs, name, &mut bytes)
                .map_err(|error| {
                    if error.kind() == FsErrorKind::NotFound {
                        BlobReadError::Corrupt(BlobCorruption::Missing)
                    } else {
                        BlobReadError::Filesystem {
                            step: BlobReadStep::Read,
                            error,
                        }
                    }
                })?;
            if actual != len {
                return Err(BlobReadError::ReadLengthMismatch {
                    expected: len,
                    actual,
                });
            }
            let actual_hash = sage_db_format::digest::raw_sha256_bounded(&bytes, ledger).map_err(
                |e| match e {
                    sage_db_format::digest::BoundedHashError::Budget(e) => {
                        BlobReadError::Refused(Refusal::Budget(e))
                    }
                    sage_db_format::digest::BoundedHashError::Stopped(e) => {
                        BlobReadError::Refused(Refusal::Stopped(e))
                    }
                },
            )?;
            if actual_hash != digest {
                return Err(BlobReadError::Corrupt(BlobCorruption::HashMismatch));
            }
            Ok(CommittedBlob {
                bytes,
                retained,
                basis,
            })
        })();
        if matches!(&result, Err(BlobReadError::Corrupt(_))) {
            self.mark_terminal_corrupt();
        }
        result
    }

    /// The ONE current view accessor.
    ///
    /// Returns a tagged, immutable view of the root this handle currently serves. `None` means there is no current view:
    /// nothing replayed yet, replay incomplete, CURRENT unreadable, or attempted authority undecided.
    ///
    /// The legacy accessors stay Durable-only, so an `Unverified` view is reachable here and nowhere else.
    #[must_use]
    pub fn view(&self) -> Option<crate::view::TaggedView> {
        if self.authority_undecided {
            return None;
        }
        self.current_view.clone()
    }

    /// Whether ambiguous I/O has fenced writes, including orphan-phase health failures.
    #[must_use]
    pub const fn is_fenced(&self) -> bool {
        self.fenced
    }

    /// Whether the outcome index covers the current committed history.
    ///
    /// An undecided authority may name a commit this index has not replayed, so it cannot claim complete coverage.
    /// An orphan-phase health fence leaves the authority unchanged and does not invalidate that coverage.
    #[must_use]
    pub const fn coverage_complete(&self) -> bool {
        self.coverage_complete && !self.authority_undecided
    }

    /// The KB identity this handle is bound to.
    #[must_use]
    pub const fn kb_uuid(&self) -> KbUuid {
        self.kb_uuid
    }

    /// The canonical state entries this handle retained, for inspection and digest recomputation.
    pub fn retained_entries(&self) -> impl Iterator<Item = &StateEntry> {
        // The CURRENT set: one entry per key. A `flat_map` over history would expose BOTH the old and the new
        // revision of any record a later transaction revised — an ambiguous view, and a second authority beside the
        // set the digest was computed from.
        // DURABLE-ONLY, same basis as `published_pin`: an `Unverified` current set must never look like the durable current
        // view. Reading the tagged slot (rather than the raw pin) is what keeps those two from disagreeing.
        if self.authority_undecided {
            return [].as_slice().iter();
        }
        self.current_view
            .as_ref()
            .filter(|view| view.durability() == crate::view::ViewDurability::Durable)
            .map_or([].as_slice(), crate::view::TaggedView::entries)
            .iter()
    }

    /// Commit the **Genesis** transaction of a root proven fresh under a continuously held lock.
    ///
    /// Commit the **Genesis** transaction. Crate-private: reachable ONLY through the bootstrap handoff in
    /// [`crate::init::bootstrap`].
    ///
    /// Genesis is not a client request — it has no `(principal_id, request_id)` outcome to look up (§7) — so the client
    /// idempotency guard does not apply. Everything else is the ordinary protocol: the candidate is validated through the
    /// accepted format layer and published through the same durable path.
    ///
    /// Visibility is the mechanism, not a version check: an ordinary writable handle cannot call this at all, so the
    /// bypass cannot be selected by an arbitrary caller on a general handle.
    ///
    /// # Errors
    ///
    /// Any ordinary commit refusal. A candidate that is not a legal version-1 Genesis is refused, not repaired.
    pub(crate) fn commit_genesis(
        &mut self,
        genesis: PreparedTransaction,
        limits: &Limits,
        ledger: &mut dyn crate::lease::LeaseLedger,
    ) -> HandleOutcome {
        // Genesis is version 1 with no predecessor; publishing anything else at version 1 would be a protocol violation.
        if genesis.version != 1 || genesis.prev_version.is_some() {
            return HandleOutcome::NotCommitted {
                reason: HandleRefusal::WrongKb {
                    expected: self.kb_uuid,
                    found: genesis.kb_uuid,
                },
            };
        }
        if self.head.is_some() {
            // Genesis is only legal on a root with no committed history.
            return HandleOutcome::NotCommitted {
                reason: HandleRefusal::ExistingRootUnsupported,
            };
        }
        self.commit_inner(genesis, limits, ledger, /* client_request = */ false)
    }

    /// Commit one candidate.
    ///
    /// Order, and it is total — no step may be reordered without breaking idempotency:
    ///
    /// 1. fence check;
    /// 2. outcome lookup for `(principal_id, request_id)` **before** any version check, so an exact retry of an
    ///    already-committed request returns the stored outcome instead of a version conflict;
    /// 3. version continuity;
    /// 4. derive facts from the candidate (projection + assemblers + chain);
    /// 5. commit through the persistence sequence.
    ///
    /// # Errors
    ///
    /// Never `Err`: a refusal and an ambiguous attempt are answers, not errors.
    pub fn commit(
        &mut self,
        prepared: PreparedTransaction,
        limits: &Limits,
        ledger: &mut dyn crate::lease::LeaseLedger,
    ) -> HandleOutcome {
        self.commit_inner(prepared, limits, ledger, /* client_request = */ true)
    }

    /// The one commit body. `client_request` selects whether the client idempotency lookup applies: it does for every
    /// request a client makes, and does not for Genesis, which has no `(principal_id, request_id)` outcome to find.
    fn commit_inner(
        &mut self,
        prepared: PreparedTransaction,
        limits: &Limits,
        ledger: &mut dyn crate::lease::LeaseLedger,
        client_request: bool,
    ) -> HandleOutcome {
        if self.fenced {
            return HandleOutcome::NotCommitted {
                reason: HandleRefusal::Fenced,
            };
        }

        // Step 2: idempotency first. A stale exact retry must answer "committed, here is the result", never
        // "version conflict" — the client's write already happened. Genesis is not a client request and has no such
        // lookup (§7), so it skips this step entirely rather than being made to satisfy it.
        if client_request && let Some(existing) = self.lookup_outcome(&prepared) {
            return existing;
        }

        // Step 3: continuity.
        // The candidate must belong to this KB. A transaction for another KB would publish under this root.
        if prepared.kb_uuid != self.kb_uuid {
            return HandleOutcome::NotCommitted {
                reason: HandleRefusal::WrongKb {
                    expected: self.kb_uuid,
                    found: prepared.kb_uuid,
                },
            };
        }
        // Optimistic concurrency: when the caller states an expectation, it must be the current head. Checked
        // BEFORE continuity so a stale expectation is reported as such rather than as a version conflict.
        if let Some(expect) = prepared.expect_version
            && Some(expect) != self.head
        {
            return HandleOutcome::NotCommitted {
                reason: HandleRefusal::ExpectVersionMismatch {
                    expected: expect,
                    found: self.head,
                },
            };
        }

        // Checked: `head + 1` overflows at u64::MAX, and a wrapped version would compare equal to 0.
        let expected = match self.head {
            None => 1,
            Some(head) => match head.checked_add(1) {
                Some(next) => next,
                None => {
                    return HandleOutcome::NotCommitted {
                        reason: HandleRefusal::VersionOverflow { head },
                    };
                }
            },
        };
        if prepared.version != expected {
            return HandleOutcome::NotCommitted {
                reason: HandleRefusal::VersionConflict {
                    expected,
                    found: prepared.version,
                },
            };
        }

        // Consume the ORIGINAL carrier once after the refusal-only guards. The header owns its existing strings and
        // extensions; derivation borrows those values and persistence later consumes the very same header and parts.
        let (header, parts) = crate::prepared::split_carrier(prepared);

        // Step 4: build the NEXT state — current fold + this candidate on a scratch copy — and derive the facts
        // from it. Nothing published is mutated here; a failure below discards the scratch entirely.
        let (facts, next_fold, current_set, tx_image) =
            match self.derive_next(&header, &parts, limits, ledger) {
                Ok(derived) => derived,
                Err(refusal) => {
                    return HandleOutcome::NotCommitted {
                        reason: HandleRefusal::Derivation(refusal),
                    };
                }
            };

        let prepared_history = match crate::history::HistoryFrame::from_image(
            header.version,
            header.tx_id,
            &tx_image,
            self.history_frames.clone(),
            limits,
            ledger,
        ) {
            Ok(frame) => frame,
            Err(error) => {
                return HandleOutcome::NotCommitted {
                    reason: HandleRefusal::History(error),
                };
            }
        };
        drop(tx_image);

        // The last-committed metadata is inline; no history Vec or unused watermark copy is allocated here.
        // Prepare the pin owner BEFORE persistence, from the SAME canonical set the digest is computed from: one
        // current set, not a second owned copy. `prepare` transfers the already-admitted retained extent and admits only
        // the new owner block, so a refusal here costs no durability and leaves no state changed.
        let pin_snapshot = sage_db_types::graph::SnapshotId {
            kb_uuid: self.kb_uuid,
            version: sage_db_types::identity::KbVersion::new(header.version),
            rules: sage_db_types::relation::RuleSet::from_trusted_parts(
                facts.rules.profile,
                sage_db_types::digest::RulesDigest::new(sage_db_types::digest::Digest::from_bytes(
                    facts.rules.rules_digest,
                )),
            ),
            graph_digest: sage_db_types::digest::GraphDigest::new(
                sage_db_types::digest::Digest::from_bytes(facts.graph_digest),
            ),
        };
        // The pin retains the SAME buffer the digest was computed from: `into_entries` is a byte-exact move of the
        // `RecordSet`'s vector (no clone, no realloc, no re-sort), so the published view is the digested set itself.
        let pin_entries = current_set.into_entries();
        let prepared_pin = match crate::pin::prepare(ledger, pin_snapshot, pin_entries) {
            Ok(pin) => pin,
            // Each failure keeps its own class and quantity. Nothing is collapsed into a fabricated
            // `Allocation { requested: 0 }`: a layout error attempted no allocation, a transfer shortfall consulted no
            // allowance, and a real allocator refusal carries the extent it actually asked for.
            Err(error) => {
                let reason = match error {
                    crate::pin::PinError::Budget(error) => HandleRefusal::Derivation(
                        CommitRefusal::Refused(sage_db_types::framing::Refusal::Budget(error)),
                    ),
                    crate::pin::PinError::Stopped(stop) => HandleRefusal::Derivation(
                        CommitRefusal::Refused(sage_db_types::framing::Refusal::Stopped(stop)),
                    ),
                    crate::pin::PinError::Transfer(error) => HandleRefusal::PinTransfer(error),
                    crate::pin::PinError::Layout => HandleRefusal::PinLayout,
                    crate::pin::PinError::ExtentOverflow => HandleRefusal::ExtentOverflow,
                    crate::pin::PinError::Allocate { requested } => HandleRefusal::Derivation(
                        CommitRefusal::Refused(sage_db_types::framing::Refusal::Allocation {
                            requested: requested as usize,
                        }),
                    ),
                };
                return HandleOutcome::NotCommitted { reason };
            }
        };

        // The surviving fold container takes ownership of its own retained buffers at the publication boundary: the fork
        // built new tables and keys on this attempt's counter, and those outlive the facade. Per-record tokens cover record
        // bodies only, so without this the container's data would stay served after its credit was released.
        let mut next_fold = next_fold;
        match next_fold.adopt_container(ledger) {
            crate::fold::FoldOutcome::Ok(()) => {}
            crate::fold::FoldOutcome::Refused(refusal) => {
                return HandleOutcome::NotCommitted {
                    reason: HandleRefusal::Derivation(CommitRefusal::Refused(refusal)),
                };
            }
            crate::fold::FoldOutcome::Defect(defect) => {
                return HandleOutcome::NotCommitted {
                    reason: HandleRefusal::Derivation(CommitRefusal::from(defect)),
                };
            }
        }

        // Persist. The facts come from the candidate, not from the caller.
        let target = CommitTarget {
            fs: &self.fs,
            root: &self.root,
            staging: &self.families.staging,
            blobs: &self.families.blobs,
            transactions: &self.families.transactions,
            manifests: &self.families.manifests,
        };
        // Derivation borrowed this header/parts; publication consumes the same owned values without rebuilding either.
        let persistence = commit_parts(&target, header, parts, &facts, limits, ledger);
        self.last_persist_stop = persistence.deferred_stop();
        match persistence {
            CommitOutcome::Committed {
                version,
                recorded,
                manifest_name,
                manifest_retained,
                manifest_sha256,
                ..
            } => {
                // THE ONE SWAP: moves only. Capacity and vectors were prepared above, before persistence.
                //
                // The outcome is NOT copied into the row: `next_fold` already holds every committed outcome (that is
                // what the idempotency lookup reads), so a second owned copy would be both an allocation after the
                // fence and two authorities for one fact.
                self.head = Some(version);
                self.fold = next_fold;
                self.history_frames = Some(prepared_history);
                // THE ONE MOVE: the prepared pin becomes the published owner. The digest was computed from this same
                // buffer, and readers are served from it, so there is exactly ONE current owner and no post-fence
                // allocation. A pre-publication refusal dropped the preparation with no state change.
                self.pin = Some(prepared_pin);
                // The pin identity is built from the version, the rules in force and the derived graph digest — the
                // same facts the manifest carries, so the pin and the persisted artifact describe one version.
                // Only what a caller actually reads. `row_entries`/`row_watermarks` are no longer cached in RAM: the
                // current set lives in the pin, and the committed files remain on disk. Nothing authoritative is dropped.
                self.last_committed = Some(Committed {
                    chain_digest: facts.chain_digest,
                    manifest_name,
                    _manifest_retained: manifest_retained,
                    manifest_sha256,
                });

                // Coverage is only complete once this handle has indexed everything it committed.
                self.coverage_complete = true;
                // ONE publication basis: a durable commit publishes the tagged view from the pin it just swapped in. Without
                // this, a fresh bootstrap+commit had a pin but `view()` == None — a real defect the milestone review found.
                if let Some(pin) = self.pin.clone() {
                    self.current_view = Some(crate::view::TaggedView::new(
                        pin,
                        crate::view::ViewDurability::Durable,
                        crate::view::ViewBasis {
                            kb_uuid: self.kb_uuid,
                            version: sage_db_types::identity::KbVersion::new(version),
                            manifest_sha256,
                        },
                    ));
                }
                HandleOutcome::Committed { version, recorded }
            }
            CommitOutcome::NotCommitted { reason, .. } => {
                // K12b: ambiguous I/O in the ORPHAN phase (before authority moved) fences writes, but the previously
                // durable current view remains available — nobody disputes what CURRENT names. This is a DIFFERENT
                // reason from attempted-authority `Unknown`, which hides the current view, so the two must not collapse
                // into one condition.
                if let CommitRefusal::Filesystem { error, .. } = &reason
                    && error.effect() == crate::fs::Effect::Unknown
                {
                    self.fenced = true;
                    return HandleOutcome::NotCommitted {
                        reason: HandleRefusal::Derivation(reason),
                    };
                }
                HandleOutcome::NotCommitted {
                    reason: HandleRefusal::Derivation(reason),
                }
            }
            // Ambiguity fences the handle: it must not keep accepting writes while the root's meaning is undecided.
            CommitOutcome::Unknown { .. } => {
                // Writes are fenced AND the current view becomes absent: two distinct consequences of one undecided outcome.
                // `fenced` alone was not enough — A showed a fresh getter still returning Some(old).
                self.fenced = true;
                self.authority_undecided = true;
                // ONE publication basis: an undecided authority has NO current view, and that must be true of the stored slot
                // too — otherwise a later reader of the slot (the outcome query) would still answer as if authority held.
                self.current_view = None;
                self.coverage_complete = false;
                HandleOutcome::Unknown
            }
        }
    }

    /// Look up an idempotency key, returning the stored answer when one exists.
    fn lookup_outcome(&self, prepared: &PreparedTransaction) -> Option<HandleOutcome> {
        // "Absent" requires affirmative coverage. The previous guard (`!coverage_complete && !history.is_empty()`)
        // was false for the very case it was written for: a fresh handle has empty history, so it skipped this
        // branch and fell through as absent.
        if !self.coverage_complete {
            return Some(HandleOutcome::NotCommitted {
                reason: HandleRefusal::OutcomeIndexIncomplete,
            });
        }
        // The fold is the ONE authority for committed outcomes: it holds every one, keyed by the idempotency pair,
        // so a stored answer is read from there rather than from a second copy in `history`.
        let stored = self
            .fold
            .outcome(&prepared.principal_id, &prepared.request_id)?;
        if stored.payload_digest == prepared.payload_digest {
            return Some(HandleOutcome::AlreadyCommitted {
                version: stored.commit_version,
                // The stored outcome is ALREADY admitted and owned by the fold: sharing it is a refcount bump, not a
                // payload copy and not a new admission. That is the exact-retry requirement.
                recorded: RecordedOutcome::Outcome(stored.event.clone()),
            });
        }
        // Same key, different content: an id reuse, never a retry.
        Some(HandleOutcome::NotCommitted {
            reason: HandleRefusal::IdempotencyMismatch,
        })
    }

    /// Build the NEXT state: the committed state plus this candidate, applied to a scratch copy.
    ///
    /// Nothing published is mutated during derivation. The candidate is folded into a **clone** of the current
    /// fold and the facts are computed from that next state, so a failure leaves the published state untouched and
    /// discards the scratch.
    ///
    /// This is the one flow: snapshot current + physical fold -> apply candidate -> facts from NEXT -> prebuild ->
    /// persist -> one swap.
    fn derive_next(
        &self,
        header: &sage_db_format::encode::TxHeaderToEncode,
        parts: &crate::prepared::CarrierParts,
        limits: &Limits,
        ledger: &mut dyn crate::lease::LeaseLedger,
    ) -> Result<(ManifestFacts, crate::fold::StateFold, RecordSet, Vec<u8>), CommitRefusal> {
        // Canonical state entries for THIS candidate, from the real projection over raw event fields. Retained
        // rather than re-encoded, so optional-field presence survives.
        let mut new_entries: Vec<StateEntry> = Vec::new();
        let event_view = match sage_db_format::prepared::view_for_encode(
            header,
            &parts.events,
            limits,
            ledger,
        ) {
            FormatOutcome::Ok(view) => view,
            FormatOutcome::Format(error) => return Err(CommitRefusal::Format(error)),
            FormatOutcome::Refused(refusal) => return Err(CommitRefusal::Refused(refusal)),
        };
        for event in event_view.events() {
            match project_event(event.tag, &event.fields, &mut new_entries, ledger) {
                FormatOutcome::Ok(_) => {}
                // The four-class verdict is preserved, not collapsed: a caller needs to know whether the bytes were
                // structurally wrong versus refused for a reason.
                FormatOutcome::Format(error) => return Err(CommitRefusal::Format(error)),
                FormatOutcome::Refused(refusal) => return Err(CommitRefusal::Refused(refusal)),
            }
        }

        // Scratch fold: the committed state, then this candidate applied. The published fold is not touched.
        //
        // BLOCKED, deliberately and visibly: `try_fork` reserves its tables fallibly but then deep-copies every owned
        // `String`/`Vec` in all thirteen record families with `clone_from`, which allocates infallibly and unadmitted.
        // Precharging those bytes would not make the copies fallible — a budget meter is not allocation fallibility —
        // so proceeding would claim resource closure the commit path does not have. Refusing here keeps the gap
        // detectable instead of letting a scalar-only fork fixture stand in for it.
        //
        // Removal condition: the fold payload path becomes shared-immutable (refcount bump, no payload copy) or an
        // actual admitted, fallible per-field copy. Neither exists yet.
        let mut next_fold = match self.fold.try_fork(ledger) {
            crate::fold::FoldOutcome::Ok(fork) => fork,
            crate::fold::FoldOutcome::Refused(refusal) => {
                return Err(CommitRefusal::Refused(refusal));
            }
            crate::fold::FoldOutcome::Defect(defect) => return Err(CommitRefusal::from(defect)),
        };
        let references = canonical_refs_with_lengths(event_view.events(), &parts.blobs, ledger)?;
        let actions = fold_actions(&parts.events, references, limits, ledger)?;
        let version = crate::fold::FoldedVersion {
            version: header.version,
            tx_id: header.tx_id,
            event_count: u32::try_from(parts.events.len()).unwrap_or(u32::MAX),
            rules_profile: parts.rules.profile.as_u16(),
            rules_digest: parts.rules.rules_digest,
        };
        match next_fold.apply_owning(version, self.kb_uuid, actions, ledger) {
            crate::fold::FoldOutcome::Ok(()) => {}
            crate::fold::FoldOutcome::Refused(refusal) => {
                return Err(CommitRefusal::Refused(refusal));
            }
            crate::fold::FoldOutcome::Defect(defect) => return Err(CommitRefusal::from(defect)),
        }

        // State digest from the union of previously committed entries and this candidate's, sorted as
        // `RecordSet::from_sorted` requires. A per-candidate set would drop every earlier record.
        // Merge the PUBLISHED current view with this candidate's entries — one entry per canonical key, latest revision
        // wins. Walking every `history[].entries` prefix would rebuild the same state from the whole committed chain and
        // would grow with history depth; the published current set is already that merge for everything committed.
        //
        // Copies are unavoidable here: an unchanged old entry must be physically duplicated into the next owned set,
        // because the old pin keeps its own buffer immutable. So the copy is ADMITTED and FALLIBLE rather than an
        // infallible `StateEntry::clone`.
        let mut by_key: Vec<StateEntry> = Vec::new();
        // `pending` holds references only, so it is reserved FALLIBLY for the exact count before it is filled. Pushing
        // first and admitting later would allocate the reference array before any charge.
        let expected = self.pin.as_ref().map_or(0, |pin| pin.entries().len()) + new_entries.len();
        let pending_slots = (expected as u64)
            .checked_mul(core::mem::size_of::<&StateEntry>() as u64)
            .ok_or(CommitRefusal::ExtentOverflow)?;
        if let Err(error) = ledger.try_charge(pending_slots) {
            return Err(CommitRefusal::Refused(
                sage_db_types::framing::Refusal::Budget(error),
            ));
        }
        if let Err(stop) = ledger.checkpoint() {
            return Err(CommitRefusal::Refused(
                sage_db_types::framing::Refusal::Stopped(stop),
            ));
        }
        let mut pending: Vec<&StateEntry> = Vec::new();
        if pending.try_reserve_exact(expected).is_err() {
            return Err(CommitRefusal::Refused(
                sage_db_types::framing::Refusal::Allocation {
                    requested: pending_slots as usize,
                },
            ));
        }

        // Seed from the published current view, then overlay the candidate.
        if let Some(pin) = self.pin.as_ref() {
            for entry in pin.entries() {
                pending.push(entry);
            }
        }
        for entry in &new_entries {
            pending.push(entry);
        }

        // Admit the slot capacity and the payload bytes the new set will own, before any allocation.
        let entry_slots = deadline_slots(pending.len())?;
        let mut payload_bytes = 0u64;
        for entry in &pending {
            payload_bytes = payload_bytes
                .checked_add(entry.key.len() as u64)
                .and_then(|n| n.checked_add(entry.body.len() as u64))
                .ok_or(CommitRefusal::Encode)?;
        }
        if let Err(error) = ledger.try_charge(entry_slots) {
            return Err(CommitRefusal::Refused(
                sage_db_types::framing::Refusal::Budget(error),
            ));
        }
        if let Err(stop) = ledger.checkpoint() {
            return Err(CommitRefusal::Refused(
                sage_db_types::framing::Refusal::Stopped(stop),
            ));
        }
        if by_key.try_reserve_exact(pending.len()).is_err() {
            return Err(CommitRefusal::Refused(
                sage_db_types::framing::Refusal::Allocation {
                    requested: entry_slots as usize,
                },
            ));
        }

        for entry in pending {
            // Fallible, admitted copy: reserve each new key/body buffer before the copy, so a refusal happens before the
            // bytes are allocated rather than after an infallible clone has already occurred.
            let copy = copy_entry_admitted(entry, ledger)?;
            match by_key.binary_search_by(|existing: &StateEntry| {
                (existing.kind, existing.key.as_slice()).cmp(&(copy.kind, copy.key.as_slice()))
            }) {
                // A later entry supersedes an earlier one for the same canonical key.
                Ok(at) => by_key[at] = copy,
                Err(at) => by_key.insert(at, copy),
            }
        }
        let set = match RecordSet::from_sorted(by_key, ledger) {
            Ok(set) => set,
            // Preserve the ACTUAL rejection class. `Err(_) => Encode` told the caller to fix its bytes when a budget
            // shortfall or a caller stop was the real cause, which are different remedies.
            Err(sage_db_types::persist::RecordSetRejection::Budget(error)) => {
                return Err(CommitRefusal::Refused(
                    sage_db_types::framing::Refusal::Budget(error),
                ));
            }
            Err(sage_db_types::persist::RecordSetRejection::Stopped(stop)) => {
                return Err(CommitRefusal::Refused(
                    sage_db_types::framing::Refusal::Stopped(stop),
                ));
            }
            // An ordering/violation verdict IS about the entries themselves, so `Encode` is truthful here.
            Err(sage_db_types::persist::RecordSetRejection::Order(_)) => {
                return Err(CommitRefusal::Encode);
            }
        };
        let state_digest = match sage_db_format::state::state_digest(&set, ledger) {
            Ok(digest) => digest,
            // A ledger refusal stays a refusal; a structural defect in the candidate is not a budget problem.
            Err(error) => {
                return Err(match error.as_refusal() {
                    Some(refusal) => CommitRefusal::Refused(refusal),
                    None => CommitRefusal::Encode,
                });
            }
        };

        // Graph and bookkeeping come from the NEXT fold, so records this candidate did not touch are included.
        let (terms, assertions) = graph_from_fold(&next_fold, ledger)?;
        let graph_digest = match sage_db_format::graph::graph_digest(&terms, &assertions, ledger) {
            Ok(digest) => digest,
            Err(error) => {
                return Err(match error.as_refusal() {
                    Some(refusal) => CommitRefusal::Refused(refusal),
                    None => CommitRefusal::Encode,
                });
            }
        };
        // `next_fold` already received this candidate through `fold.apply`, so its outcomes and imports INCLUDE
        // the candidate's. Adding them again here doubled the current `RequestOutcomeRecorded` in
        // `bookkeeping_digest`; and taking imports only from the candidate dropped every retained import.
        let (outcomes, imports, counters) = bookkeeping_from_fold(&next_fold, ledger)?;
        let bookkeeping_digest = match sage_db_format::bookkeeping::bookkeeping_digest(
            &outcomes, &imports, &counters, ledger,
        ) {
            Ok(digest) => digest,
            Err(error) => {
                return Err(match error.as_refusal() {
                    Some(refusal) => CommitRefusal::Refused(refusal),
                    None => CommitRefusal::Encode,
                });
            }
        };

        let prev_chain = self.fold_chain_digest();
        let (tx_digest, tx_image) = tx_digest_of(header, &parts.events, limits, ledger)?;
        let chain_digest = sage_db_format::digest::chain_digest(prev_chain, tx_digest);

        let facts = ManifestFacts {
            state_digest,
            graph_digest,
            chain_digest,
            bookkeeping_digest,
            // Admitted and owned here, before persistence: the predecessor name is a real buffer this commit publishes,
            // so its allocation is charged like every other retained buffer rather than appearing as a bare `clone()`.
            prev_manifest_name: match self.last_committed.as_ref() {
                Some(previous) => {
                    let bytes = previous.manifest_name.len() as u64;
                    if let Err(error) = ledger.try_charge(bytes) {
                        return Err(CommitRefusal::Refused(
                            sage_db_types::framing::Refusal::Budget(error),
                        ));
                    }
                    if let Err(stop) = ledger.checkpoint() {
                        return Err(CommitRefusal::Refused(
                            sage_db_types::framing::Refusal::Stopped(stop),
                        ));
                    }
                    let mut name = String::new();
                    if name
                        .try_reserve_exact(previous.manifest_name.len())
                        .is_err()
                    {
                        return Err(CommitRefusal::Refused(
                            sage_db_types::framing::Refusal::Allocation {
                                requested: previous.manifest_name.len(),
                            },
                        ));
                    }
                    name.push_str(&previous.manifest_name);
                    Some(name)
                }
                None => None,
            },
            prev_manifest_sha256: self.last_committed.as_ref().map(|c| c.manifest_sha256),
            // The rules the candidate was evaluated with, carried through to the pin identity. `RulesBinding` is a
            // profile plus two 32-byte arrays — fixed size, no heap — so this copy allocates nothing.
            // Fixed-size binding (profile + two 32-byte arrays): no heap, so this is not an owning clone.
            rules: parts.rules.clone(),
        };
        Ok((facts, next_fold, set, tx_image))
    }

    /// The chain digest of the last committed version, or zero before genesis.
    fn fold_chain_digest(&self) -> [u8; 32] {
        self.last_committed
            .as_ref()
            .map_or([0u8; 32], |committed| committed.chain_digest)
    }
}

/// Every canonical blob reference this candidate commits, paired with the length its declaration verified.
///
/// Enumeration is format's predicate; the length comes from the matching declaration, so the fold records a verified
/// extent rather than a guess.
fn canonical_refs_with_lengths(
    events: &[sage_db_format::encode::EventToEncode<'_>],
    declarations: &[sage_db_types::prepared::BlobDeclaration],
    ledger: &mut dyn SharedLedger,
) -> Result<Vec<([u8; 32], u64)>, CommitRefusal> {
    /// Pairs each reference with its declared length as enumeration reaches it.
    struct Pair<'a> {
        declarations: &'a [sage_db_types::prepared::BlobDeclaration],
        found: Vec<([u8; 32], u64)>,
        missing: Option<[u8; 32]>,
    }

    impl sage_db_types::framing::BlobRefSink for Pair<'_> {
        fn push(
            &mut self,
            _tag: u16,
            sha256: [u8; 32],
            _ledger: &mut dyn SharedLedger,
        ) -> Result<(), Refusal> {
            let mut declaration = None;
            for row in self.declarations {
                _ledger.try_charge_entries(1)?;
                _ledger.checkpoint()?;
                if row.sha256 == sha256 {
                    declaration = Some(row);
                    break;
                }
            }
            match declaration {
                Some(declared) => {
                    for (seen, _) in &self.found {
                        _ledger.try_charge_entries(1)?;
                        _ledger.checkpoint()?;
                        if *seen == sha256 {
                            return Ok(());
                        }
                    }
                    // At most one distinct row per declaration; capacity was reserved before enumeration.
                    self.found.push((sha256, declared.declared_len));
                }
                None if self.missing.is_none() => self.missing = Some(sha256),
                None => {}
            }
            Ok(())
        }
    }

    let mut pairs = Pair {
        declarations,
        found: Vec::new(),
        missing: None,
    };
    let slots = (declarations.len() as u64)
        .checked_mul(core::mem::size_of::<([u8; 32], u64)>() as u64)
        .ok_or(CommitRefusal::ExtentOverflow)?;
    ledger
        .try_charge(slots)
        .map_err(|error| CommitRefusal::Refused(Refusal::Budget(error)))?;
    ledger
        .checkpoint()
        .map_err(|stop| CommitRefusal::Refused(Refusal::Stopped(stop)))?;
    pairs
        .found
        .try_reserve_exact(declarations.len())
        .map_err(|_| {
            CommitRefusal::Refused(Refusal::Allocation {
                requested: slots as usize,
            })
        })?;
    for event in events {
        if let Err(refusal) =
            sage_db_format::dispatch::for_each_blob_ref_fields(&event.fields, &mut pairs, ledger)
        {
            return Err(CommitRefusal::Refused(refusal));
        }
    }
    if let Some(sha256) = pairs.missing {
        return Err(CommitRefusal::UndeclaredRef { sha256 });
    }
    Ok(pairs.found)
}

/// Translate ONE owned dispatched payload into the fold actions it contributes.
///
/// This is the SINGLE payload→action mapping. Commit calls it after decoding an `OwnedEvent`; replay calls it after matching
/// `read_dispatched`'s owned `Record` variant. Neither path duplicates the mapping, and neither recovers the payload through
/// `payload()` — that accessor borrows (`Option<&_>`), so it cannot hand over ownership. The enum is matched by value.
///
/// Collects canonically enumerated references during the raw projection pass, admitting each element.
struct RefSink<'a> {
    out: &'a mut Vec<([u8; 32], u64)>,
}

impl sage_db_types::framing::BlobRefSink for RefSink<'_> {
    fn push(
        &mut self,
        _tag: u16,
        sha256: [u8; 32],
        ledger: &mut dyn SharedLedger,
    ) -> Result<(), Refusal> {
        ledger.try_charge_entries(1)?;
        ledger.checkpoint()?;
        // The caller reserved one slot per raw field, an upper bound on callbacks.
        debug_assert!(self.out.len() < self.out.capacity());
        self.out.push((sha256, 0));
        Ok(())
    }
}

fn push_payload_actions(
    payload: sage_db_format::dispatch::DispatchedPayload,
    actions: &mut Vec<crate::fold::FoldEvent>,
    ledger: &mut dyn SharedLedger,
) -> Result<(), Refusal> {
    use crate::fold::{FoldEvent, FoldedRecord};
    use sage_db_format::dispatch::DispatchedPayload;

    match payload {
        DispatchedPayload::Term(record) => {
            actions.push(FoldEvent::Record(FoldedRecord::Term(record)))
        }
        DispatchedPayload::Assertion(record) => {
            actions.push(FoldEvent::Record(FoldedRecord::Assertion(record)))
        }
        DispatchedPayload::Claim(record) => {
            actions.push(FoldEvent::Record(FoldedRecord::Claim(record)))
        }
        DispatchedPayload::Evidence(record) => {
            actions.push(FoldEvent::Record(FoldedRecord::Evidence(record)))
        }
        DispatchedPayload::EvidenceLink(record) => {
            actions.push(FoldEvent::Record(FoldedRecord::EvidenceLink(record)))
        }
        DispatchedPayload::Vector(record) => {
            actions.push(FoldEvent::Record(FoldedRecord::Vector(record)))
        }
        DispatchedPayload::Proposal(record) => {
            actions.push(FoldEvent::Record(FoldedRecord::Proposal(record)))
        }
        DispatchedPayload::FrontierEntry(record) => {
            actions.push(FoldEvent::Record(FoldedRecord::FrontierEntry(record)))
        }
        DispatchedPayload::SessionCheckpoint(record) => {
            actions.push(FoldEvent::Record(FoldedRecord::SessionCheckpoint(record)))
        }
        DispatchedPayload::KbMetadata(record) => {
            actions.push(FoldEvent::Record(FoldedRecord::KbMetadata(record)))
        }
        DispatchedPayload::Model(record) => {
            actions.push(FoldEvent::Record(FoldedRecord::Model(record)))
        }
        DispatchedPayload::RuleProfile(record) => {
            actions.push(FoldEvent::Record(FoldedRecord::RuleConfig(record)))
        }
        DispatchedPayload::Asset(record) => {
            actions.push(FoldEvent::Record(FoldedRecord::Asset(record)))
        }
        DispatchedPayload::IdAllocation(record) => actions.push(FoldEvent::IdAllocation(record)),
        DispatchedPayload::RequestOutcome(record) => actions.push(FoldEvent::Outcome(record)),
        DispatchedPayload::ImportBookkeeping(record) => {
            actions.push(FoldEvent::ImportSession(record))
        }
        // One wire event establishes two physical singleton after-images.
        DispatchedPayload::Genesis(record) => {
            // By VALUE: the decoded groups are moved into the two records rather than duplicated. Copying them
            // was an infallible owning clone on a path that already owns them.
            let second_reason = match record.reason.as_deref() {
                Some(reason) => Some(crate::names::copy_value_bounded(reason, ledger)?),
                None => None,
            };
            let mut metadata = genesis_metadata_record(record.created_at, record.metadata);
            let mut rules = genesis_rules_record(record.created_at, record.rules);
            metadata.audit.reason = record.reason;
            rules.audit.reason = second_reason;
            actions.push(FoldEvent::Record(FoldedRecord::KbMetadata(metadata)));
            actions.push(FoldEvent::Record(FoldedRecord::RuleConfig(rules)));
        }
    }
    Ok(())
}

/// Turn a candidate's owned events into the fold's actions, by decoding each through the real dispatch.
///
/// # Errors
///
/// A format verdict or refusal from decoding an event.
fn fold_actions(
    events: &[sage_db_types::prepared::OwnedEvent],
    references: Vec<([u8; 32], u64)>,
    limits: &Limits,
    ledger: &mut dyn SharedLedger,
) -> Result<Vec<crate::fold::FoldEvent>, CommitRefusal> {
    use crate::fold::FoldEvent;

    let action_count = events
        .len()
        .checked_add(references.len())
        .and_then(|count| count.checked_add(events.iter().filter(|event| event.tag == 224).count()))
        .ok_or(CommitRefusal::ExtentOverflow)?;
    let action_bytes = (action_count as u64)
        .checked_mul(core::mem::size_of::<FoldEvent>() as u64)
        .ok_or(CommitRefusal::ExtentOverflow)?;
    ledger
        .try_charge_entries(action_count as u64)
        .map_err(|error| CommitRefusal::Refused(Refusal::Budget(error)))?;
    ledger
        .try_charge(action_bytes)
        .map_err(|error| CommitRefusal::Refused(Refusal::Budget(error)))?;
    ledger
        .checkpoint()
        .map_err(|stop| CommitRefusal::Refused(Refusal::Stopped(stop)))?;
    let mut actions = Vec::new();
    actions.try_reserve_exact(action_count).map_err(|_| {
        CommitRefusal::Refused(Refusal::Allocation {
            requested: action_bytes as usize,
        })
    })?;
    // Canonical references this candidate commits, with the length its declaration verified. Emitted as fold actions
    // so the fold's committed-ref union is real; a transient containment check alone leaves it empty.
    for (sha256, declared_len) in references {
        actions.push(FoldEvent::CommittedBlobRef {
            sha256,
            declared_len,
        });
    }
    for event in events {
        match sage_db_format::prepared::decode_owned_event(event, limits, ledger) {
            FormatOutcome::Ok(payload) => push_payload_actions(payload, &mut actions, ledger)
                .map_err(CommitRefusal::Refused)?,
            // Preserve the actual four-class verdict: a caller must be able to tell a structural rejection from a
            // generic encode failure.
            FormatOutcome::Format(error) => return Err(CommitRefusal::Format(error)),
            FormatOutcome::Refused(refusal) => return Err(CommitRefusal::Refused(refusal)),
        }
    }
    Ok(actions)
}

/// The KB-metadata record a Genesis establishes.
///
/// Genesis carries a FIELD GROUP, so building the record is a physical projection of those fields into the shape the
/// fold keys by — not a re-encoding of wire bytes.
fn genesis_metadata_record(
    created_at: i64,
    metadata: sage_db_types::persist::GenesisMetadata,
) -> KbMetadataRecord {
    // Moved, not cloned: the decoded group is already owned here, so each field transfers into the record.
    KbMetadataRecord {
        revision: sage_db_types::identity::RecordRevision::FIRST,
        kb_uuid: metadata.kb_uuid,
        slug: metadata.slug,
        name: metadata.name,
        description: metadata.description,
        domains: metadata.domains,
        legacy_frontier_ref: None,
        audit: sage_db_types::persist::OptionalActorAudit::absent_actor(created_at),
        extensions: metadata.extensions,
        changed_fields: None,
    }
}

/// The rule-config record a Genesis establishes.
fn genesis_rules_record(
    created_at: i64,
    rules: sage_db_types::persist::GenesisRules,
) -> sage_db_types::persist::RuleConfigRecord {
    sage_db_types::persist::RuleConfigRecord {
        revision: sage_db_types::identity::RecordRevision::FIRST,
        profile: rules.profile,
        from_profile: None,
        rules_digest: rules.rules_digest,
        rules_ref: rules.rules_ref,
        dry_run_report_ref: None,
        audit: sage_db_types::persist::OptionalActorAudit::absent_actor(created_at),
        extensions: rules.extensions,
        changed_fields: None,
    }
}

/// Copy a `RequestOutcomeEvent`, admitting each owned String before it is allocated.
///
/// The outer array is admitted by the caller; this handles the interior, which a `Clone` would allocate infallibly.
fn copy_event_admitted(
    source: &RequestOutcomeEvent,
    ledger: &mut dyn SharedLedger,
) -> Result<RequestOutcomeEvent, CommitRefusal> {
    let string = |text: &str, ledger: &mut dyn SharedLedger| -> Result<String, CommitRefusal> {
        let bytes = text.len() as u64;
        if let Err(error) = ledger.try_charge(bytes) {
            return Err(CommitRefusal::Refused(
                sage_db_types::framing::Refusal::Budget(error),
            ));
        }
        if let Err(stop) = ledger.checkpoint() {
            return Err(CommitRefusal::Refused(
                sage_db_types::framing::Refusal::Stopped(stop),
            ));
        }
        let mut out = String::new();
        if out.try_reserve_exact(text.len()).is_err() {
            return Err(CommitRefusal::Refused(
                sage_db_types::framing::Refusal::Allocation {
                    requested: text.len(),
                },
            ));
        }
        out.push_str(text);
        Ok(out)
    };
    let opt = |value: &Option<String>, ledger: &mut dyn SharedLedger| match value {
        Some(text) => string(text, ledger).map(Some),
        None => Ok(None),
    };
    // Built directly from scalars and the admitted strings: `audit.clone()` allocated every owned field infallibly before
    // the fallible rebuild, which is the same hidden-clone defect this helper exists to remove.
    let audit = sage_db_types::persist::CreationAudit {
        recorded_by: opt(&source.audit.recorded_by, ledger)?,
        actor_origin: source.audit.actor_origin,
        reason: opt(&source.audit.reason, ledger)?,
        created_at: source.audit.created_at,
    };
    Ok(RequestOutcomeEvent {
        principal_id: string(&source.principal_id, ledger)?,
        request_id: string(&source.request_id, ledger)?,
        payload_digest: source.payload_digest,
        result_ref: source.result_ref,
        commit_version: source.commit_version,
        audit,
        // Extensions are the remaining owned group; copied entry by entry with its own admission.
        extensions: copy_extensions_admitted(&source.extensions, ledger)?,
    })
}

/// Copy an `Extensions` table, admitting its slot array and each entry's key/value buffers.
fn copy_extensions_admitted(
    source: &sage_db_types::persist::Extensions,
    ledger: &mut dyn SharedLedger,
) -> Result<sage_db_types::persist::Extensions, CommitRefusal> {
    let entries = source.entries();
    if entries.is_empty() {
        return Ok(sage_db_types::persist::Extensions::new());
    }
    let slots =
        (entries.len() as u64).saturating_mul(core::mem::size_of::<(String, Vec<u8>)>() as u64);
    if let Err(error) = ledger.try_charge(slots) {
        return Err(CommitRefusal::Refused(
            sage_db_types::framing::Refusal::Budget(error),
        ));
    }
    let mut rebuilt: Vec<(String, Vec<u8>)> = Vec::new();
    if rebuilt.try_reserve_exact(entries.len()).is_err() {
        return Err(CommitRefusal::Refused(
            sage_db_types::framing::Refusal::Allocation {
                requested: slots as usize,
            },
        ));
    }
    for (key, value) in entries {
        let key_bytes = key.len() as u64;
        if let Err(error) = ledger.try_charge(key_bytes) {
            return Err(CommitRefusal::Refused(
                sage_db_types::framing::Refusal::Budget(error),
            ));
        }
        let mut copied_key = String::new();
        if copied_key.try_reserve_exact(key.len()).is_err() {
            return Err(CommitRefusal::Refused(
                sage_db_types::framing::Refusal::Allocation {
                    requested: key.len(),
                },
            ));
        }
        copied_key.push_str(key);
        let value_bytes = value.len() as u64;
        if let Err(error) = ledger.try_charge(value_bytes) {
            return Err(CommitRefusal::Refused(
                sage_db_types::framing::Refusal::Budget(error),
            ));
        }
        let mut copied_value = Vec::new();
        if copied_value.try_reserve_exact(value.len()).is_err() {
            return Err(CommitRefusal::Refused(
                sage_db_types::framing::Refusal::Allocation {
                    requested: value.len(),
                },
            ));
        }
        copied_value.extend_from_slice(value);
        rebuilt.push((copied_key, copied_value));
    }
    match sage_db_types::persist::Extensions::from_canonical(rebuilt) {
        Ok(ext) => Ok(ext),
        // The source was canonical, so a defect in the copy must not be disguised as budget or allocation.
        Err(_) => Err(CommitRefusal::CopyNotCanonical),
    }
}

/// Copy an `ImportSessionRecord`, admitting every owned buffer.
fn copy_import_record_admitted(
    source: &ImportSessionRecord,
    ledger: &mut dyn SharedLedger,
) -> Result<ImportSessionRecord, CommitRefusal> {
    let mut expected_counts: Vec<sage_db_types::persist::CountPair> = Vec::new();
    let counts_bytes = (source.expected_counts.len() as u64)
        .saturating_mul(core::mem::size_of::<sage_db_types::persist::CountPair>() as u64);
    if counts_bytes > 0 {
        if let Err(error) = ledger.try_charge(counts_bytes) {
            return Err(CommitRefusal::Refused(
                sage_db_types::framing::Refusal::Budget(error),
            ));
        }
        if expected_counts
            .try_reserve_exact(source.expected_counts.len())
            .is_err()
        {
            return Err(CommitRefusal::Refused(
                sage_db_types::framing::Refusal::Allocation {
                    requested: counts_bytes as usize,
                },
            ));
        }
        expected_counts.extend(source.expected_counts.iter().copied());
    }
    let string = |text: &str, ledger: &mut dyn SharedLedger| -> Result<String, CommitRefusal> {
        let bytes = text.len() as u64;
        if let Err(error) = ledger.try_charge(bytes) {
            return Err(CommitRefusal::Refused(
                sage_db_types::framing::Refusal::Budget(error),
            ));
        }
        let mut out = String::new();
        if out.try_reserve_exact(text.len()).is_err() {
            return Err(CommitRefusal::Refused(
                sage_db_types::framing::Refusal::Allocation {
                    requested: text.len(),
                },
            ));
        }
        out.push_str(text);
        Ok(out)
    };
    let opt = |value: &Option<String>, ledger: &mut dyn SharedLedger| match value {
        Some(text) => string(text, ledger).map(Some),
        None => Ok(None),
    };
    Ok(ImportSessionRecord {
        import_session_id: string(&source.import_session_id, ledger)?,
        stage: source.stage,
        source_digest: source.source_digest,
        plan_ref: source.plan_ref,
        expected_counts,
        import_complete: source.import_complete,
        audit: sage_db_types::persist::CreationAudit {
            recorded_by: opt(&source.audit.recorded_by, ledger)?,
            actor_origin: source.audit.actor_origin,
            reason: opt(&source.audit.reason, ledger)?,
            created_at: source.audit.created_at,
        },
        extensions: copy_extensions_admitted(&source.extensions, ledger)?,
    })
}

/// Slot bytes for `count` entries, with checked arithmetic.
fn deadline_slots(count: usize) -> Result<u64, CommitRefusal> {
    (count as u64)
        .checked_mul(core::mem::size_of::<StateEntry>() as u64)
        .ok_or(CommitRefusal::ExtentOverflow)
}

/// Copy one entry into the next owned set, admitting and reserving its key/body buffers first.
///
/// The copy is necessary — the old pin keeps its buffer immutable — so it is made fallible and admitted rather than left as
/// an infallible `clone()`. Nothing is allocated before its extent is charged.
fn copy_entry_admitted(
    entry: &StateEntry,
    ledger: &mut dyn SharedLedger,
) -> Result<StateEntry, CommitRefusal> {
    let key_len = entry.key.len();
    let body_len = entry.body.len();
    let bytes = (key_len as u64)
        .checked_add(body_len as u64)
        .ok_or(CommitRefusal::ExtentOverflow)?;
    if let Err(error) = ledger.try_charge(bytes) {
        return Err(CommitRefusal::Refused(
            sage_db_types::framing::Refusal::Budget(error),
        ));
    }
    if let Err(stop) = ledger.checkpoint() {
        return Err(CommitRefusal::Refused(
            sage_db_types::framing::Refusal::Stopped(stop),
        ));
    }
    let mut key = Vec::new();
    if key.try_reserve_exact(key_len).is_err() {
        return Err(CommitRefusal::Refused(
            sage_db_types::framing::Refusal::Allocation { requested: key_len },
        ));
    }
    key.extend_from_slice(&entry.key);
    let mut body = Vec::new();
    if body.try_reserve_exact(body_len).is_err() {
        return Err(CommitRefusal::Refused(
            sage_db_types::framing::Refusal::Allocation {
                requested: body_len,
            },
        ));
    }
    body.extend_from_slice(&entry.body);
    Ok(StateEntry {
        kind: entry.kind,
        key,
        revision: entry.revision,
        body,
    })
}

/// The transaction-domain digest over the real encoded body, computed before persistence.
fn tx_digest_of(
    header: &crate::prepared::TxHeaderToEncode,
    events: &[sage_db_types::prepared::OwnedEvent],
    limits: &Limits,
    ledger: &mut dyn SharedLedger,
) -> Result<([u8; 32], Vec<u8>), CommitRefusal> {
    // File identity and transaction identity have different scopes. The encoder's second return value is the RAW file
    // hash used by manifest.tx_file_sha256; it is not the transaction-domain digest used by the manifest chain.
    let (image, _file_sha256) =
        crate::prepared::encode_tx_image_borrowed(header, events, limits, ledger)?;
    let envelope = match sage_db_format::transaction::read_envelope(&image, limits, ledger) {
        FormatOutcome::Ok(envelope) => envelope,
        FormatOutcome::Format(error) => return Err(CommitRefusal::Format(error)),
        FormatOutcome::Refused(refusal) => return Err(CommitRefusal::Refused(refusal)),
    };
    // storage-digests4.1: the actual uncompressed body includes the complete header (including extensions) and ordered
    // events, but none of the file header, block headers or trailer. Never reconstruct this from a reduced read header.
    let digest = sage_db_format::digest::digest_bounded(
        sage_db_format::digest::Domain::Tx,
        &envelope.body,
        ledger,
    )
    .map_err(|error| match error {
        sage_db_format::digest::BoundedHashError::Budget(error) => {
            CommitRefusal::Refused(Refusal::Budget(error))
        }
        sage_db_format::digest::BoundedHashError::Stopped(stop) => {
            CommitRefusal::Refused(Refusal::Stopped(stop))
        }
    })?;
    drop(envelope);
    Ok((digest, image))
}

/// Graph inputs, read from the fold's CURRENT records.
///
/// The fold is the single authority: it already holds every committed record and its current revision, so this
/// cannot omit a record the candidate did not touch. Selecting typed fields from `FoldedRecord` is a physical
/// projection, not a re-encoding.
fn graph_from_fold(
    fold: &crate::fold::StateFold,
    ledger: &mut dyn SharedLedger,
) -> Result<(Vec<TermMeta>, Vec<AssertionRecord>), CommitRefusal> {
    use crate::fold::FoldedRecord;
    let mut term_count = 0usize;
    let mut assertion_count = 0usize;
    for record in fold.current_records() {
        ledger
            .checkpoint()
            .map_err(|stop| CommitRefusal::Refused(Refusal::Stopped(stop)))?;
        match record {
            FoldedRecord::Term(term) if !term.retired => term_count += 1,
            FoldedRecord::Assertion(assertion) if !assertion.retracted => assertion_count += 1,
            _ => {}
        }
    }
    let mut terms = reserved_slots::<TermMeta>(term_count, ledger)?;
    let mut assertions = reserved_slots::<AssertionRecord>(assertion_count, ledger)?;
    for record in fold.current_records() {
        ledger
            .checkpoint()
            .map_err(|stop| CommitRefusal::Refused(Refusal::Stopped(stop)))?;
        match record {
            FoldedRecord::Term(term) if !term.retired => terms.push(TermMeta {
                id: TermId::new(term.key.id() as u32),
                layer: term.layer,
            }),
            FoldedRecord::Assertion(assertion) if !assertion.retracted => {
                let id = AssertionId::new(assertion.key.id());
                assertions.push(AssertionRecord {
                    id,
                    revision: assertion.revision,
                    from: TermId::new(assertion.from.id() as u32),
                    relation: assertion.relation,
                    target: TermId::new(assertion.target.id() as u32),
                    confidence: assertion.confidence,
                });
            }
            _ => {}
        }
    }
    Ok((terms, assertions))
}

/// Allocate a concrete slot array before filling it; nested buffers are handled by their actual builders.
fn reserved_slots<T>(count: usize, ledger: &mut dyn SharedLedger) -> Result<Vec<T>, CommitRefusal> {
    let bytes = (count as u64)
        .checked_mul(core::mem::size_of::<T>() as u64)
        .ok_or(CommitRefusal::ExtentOverflow)?;
    ledger
        .try_charge(bytes)
        .map_err(|error| CommitRefusal::Refused(Refusal::Budget(error)))?;
    ledger
        .checkpoint()
        .map_err(|stop| CommitRefusal::Refused(Refusal::Stopped(stop)))?;
    let mut values = Vec::new();
    values.try_reserve_exact(count).map_err(|_| {
        CommitRefusal::Refused(Refusal::Allocation {
            requested: bytes as usize,
        })
    })?;
    Ok(values)
}

/// Bookkeeping inputs: outcomes and imports from committed history plus retained high water marks.
/// The rows `bookkeeping_digest` needs, by value because it takes contiguous slices.
type BookkeepingInputs = (
    Vec<RequestOutcomeEvent>,
    Vec<ImportSessionRecord>,
    Vec<Watermark>,
);

/// Bookkeeping inputs: cumulative outcomes from the fold, plus one watermark per advanced space.
///
/// Watermarks come from the fold's per-space maxima — one entry per space, never a concatenation of per-version
/// copies, which would give the same space several rows.
fn bookkeeping_from_fold(
    fold: &crate::fold::StateFold,
    ledger: &mut dyn SharedLedger,
) -> Result<BookkeepingInputs, CommitRefusal> {
    // `bookkeeping_digest` takes contiguous slices, so the caller must own these rows: the copy is unavoidable here. It is
    // made FALLIBLE and admitted rather than an infallible `collect`, because a clone that aborts mid-digest is exactly
    // what the resource discipline is for.
    let outcomes = fold.outcomes();
    let mut outcome_rows: Vec<RequestOutcomeEvent> = Vec::new();
    let outcome_slots = (outcomes.len() as u64)
        .checked_mul(core::mem::size_of::<RequestOutcomeEvent>() as u64)
        .ok_or(CommitRefusal::ExtentOverflow)?;
    // Admit the outer array AND each nested copy: `try_reserved` alone leaves the interior `Clone` infallible.
    if let Err(error) = ledger.try_charge(outcome_slots) {
        return Err(CommitRefusal::Refused(
            sage_db_types::framing::Refusal::Budget(error),
        ));
    }
    if let Err(stop) = ledger.checkpoint() {
        return Err(CommitRefusal::Refused(
            sage_db_types::framing::Refusal::Stopped(stop),
        ));
    }
    if outcome_rows.try_reserve_exact(outcomes.len()).is_err() {
        return Err(CommitRefusal::Refused(
            sage_db_types::framing::Refusal::Allocation {
                requested: outcome_slots as usize,
            },
        ));
    }
    for (_, recorded) in outcomes {
        // The event's owned Strings are copied through admitted builders, not by an infallible `clone()`.
        outcome_rows.push(copy_event_admitted(recorded.event.event(), ledger)?);
    }

    let imports = fold.imports();
    let mut import_rows: Vec<ImportSessionRecord> = Vec::new();
    let import_slots = (imports.len() as u64)
        .checked_mul(core::mem::size_of::<ImportSessionRecord>() as u64)
        .ok_or(CommitRefusal::ExtentOverflow)?;
    if let Err(error) = ledger.try_charge(import_slots) {
        return Err(CommitRefusal::Refused(
            sage_db_types::framing::Refusal::Budget(error),
        ));
    }
    if let Err(stop) = ledger.checkpoint() {
        return Err(CommitRefusal::Refused(
            sage_db_types::framing::Refusal::Stopped(stop),
        ));
    }
    if import_rows.try_reserve_exact(imports.len()).is_err() {
        return Err(CommitRefusal::Refused(
            sage_db_types::framing::Refusal::Allocation {
                requested: import_slots as usize,
            },
        ));
    }
    for (_, session) in imports {
        import_rows.push(copy_import_record_admitted(&session.record, ledger)?);
    }

    let counter_count = fold.high_water_all().count();
    let counter_slots = (counter_count as u64)
        .checked_mul(core::mem::size_of::<Watermark>() as u64)
        .ok_or(CommitRefusal::ExtentOverflow)?;
    if let Err(error) = ledger.try_charge(counter_slots) {
        return Err(CommitRefusal::Refused(
            sage_db_types::framing::Refusal::Budget(error),
        ));
    }
    let mut counters: Vec<Watermark> = Vec::new();
    if counters.try_reserve_exact(counter_count).is_err() {
        return Err(CommitRefusal::Refused(
            sage_db_types::framing::Refusal::Allocation {
                requested: counter_slots as usize,
            },
        ));
    }
    counters.extend(
        fold.high_water_all()
            .map(|(space, hwm)| Watermark { space, hwm }),
    );
    Ok((outcome_rows, import_rows, counters))
}

/// Whether a filesystem error is the "not found" case a `CURRENT.tmp` cleanup tolerates.
#[must_use]
pub fn is_not_found(error: &FsError) -> bool {
    error.kind() == FsErrorKind::NotFound
}

/// A refusal that surfaced from the ledger rather than from a verdict.
#[must_use]
pub fn is_refusal<T>(outcome: &FormatOutcome<T>) -> Option<Refusal> {
    outcome.as_refusal()
}

/// The reader type this module uses to view raw event fields.
const _: Option<Reader<'_>> = None;

/// Build the legal version-1 Genesis candidate for `kb_uuid`.
///
/// Kept here, beside the commit path, because the shape is protocol, not fixture data: Genesis is one tag-224 event followed
/// by one tag-176 `IdAllocation` per id space, all high-water marks zero, and a version-1 `NonRequest` canon with the
/// all-zero payload sentinel. The rules binding is derived from the caller's declared rules blob rather than invented, and
/// the blob is declared so the transaction's own rules reference resolves.
///
/// # Errors
///
/// [`CommitRefusal`] when the rules blob cannot be digested under budget.
pub fn genesis_candidate(
    kb_uuid: KbUuid,
    rules: &[u8],
    ledger: &mut dyn SharedLedger,
) -> Result<PreparedTransaction, CommitRefusal> {
    genesis_candidate_with_profile(
        kb_uuid,
        sage_db_types::relation::RuleProfile::FormalV1,
        rules,
        ledger,
    )
}

/// Prepare Genesis using an explicitly selected supported rule profile.
///
/// Both the Genesis record and transaction binding declare this same profile. The ordinary
/// commit verifier still authenticates the actual rule blob and rejects a profile mismatch;
/// selecting a profile never changes, rewrites or blesses the supplied bytes.
/// [`genesis_candidate`] keeps its existing formal-v1 default.
///
/// # Errors
/// The same canonical preparation/refusal classes as [`genesis_candidate`].
pub fn genesis_candidate_with_profile(
    kb_uuid: KbUuid,
    profile: sage_db_types::relation::RuleProfile,
    rules: &[u8],
    ledger: &mut dyn SharedLedger,
) -> Result<PreparedTransaction, CommitRefusal> {
    use sage_db_types::prepared::{BlobDeclaration, BlobSource, OwnedEvent, RulesBinding};

    let rules_ref = match sage_db_format::digest::raw_sha256_bounded(rules, ledger) {
        Ok(digest) => digest,
        Err(sage_db_format::digest::BoundedHashError::Budget(error)) => {
            return Err(CommitRefusal::Refused(
                sage_db_types::framing::Refusal::Budget(error),
            ));
        }
        Err(sage_db_format::digest::BoundedHashError::Stopped(stop)) => {
            return Err(CommitRefusal::Refused(
                sage_db_types::framing::Refusal::Stopped(stop),
            ));
        }
    };
    let rules_digest = match sage_db_format::digest::digest_bounded(
        sage_db_format::digest::Domain::Rules,
        rules,
        ledger,
    ) {
        Ok(digest) => digest,
        Err(sage_db_format::digest::BoundedHashError::Budget(error)) => {
            return Err(CommitRefusal::Refused(
                sage_db_types::framing::Refusal::Budget(error),
            ));
        }
        Err(sage_db_format::digest::BoundedHashError::Stopped(stop)) => {
            return Err(CommitRefusal::Refused(
                sage_db_types::framing::Refusal::Stopped(stop),
            ));
        }
    };

    fn bytes(source: &[u8], ledger: &mut dyn SharedLedger) -> Result<Vec<u8>, CommitRefusal> {
        let mut target = reserved_slots::<u8>(source.len(), ledger)?;
        target.extend_from_slice(source);
        Ok(target)
    }
    fn string(source: &str, ledger: &mut dyn SharedLedger) -> Result<String, CommitRefusal> {
        ledger
            .try_charge(source.len() as u64)
            .map_err(|error| CommitRefusal::Refused(Refusal::Budget(error)))?;
        ledger
            .checkpoint()
            .map_err(|stop| CommitRefusal::Refused(Refusal::Stopped(stop)))?;
        let mut target = String::new();
        target.try_reserve_exact(source.len()).map_err(|_| {
            CommitRefusal::Refused(Refusal::Allocation {
                requested: source.len(),
            })
        })?;
        target.push_str(source);
        Ok(target)
    }
    fn text_field(source: &str, ledger: &mut dyn SharedLedger) -> Result<Vec<u8>, CommitRefusal> {
        let size = source
            .len()
            .checked_add(4)
            .ok_or(CommitRefusal::ExtentOverflow)?;
        let mut target = reserved_slots::<u8>(size, ledger)?;
        target.extend_from_slice(&(source.len() as u32).to_le_bytes());
        target.extend_from_slice(source.as_bytes());
        Ok(target)
    }
    let mut fields = reserved_slots::<(u16, Vec<u8>)>(10, ledger)?;
    fields.push((2, bytes(&1u64.to_le_bytes(), ledger)?));
    fields.push((8, bytes(&0i64.to_le_bytes(), ledger)?));
    fields.push((144, bytes(kb_uuid.as_bytes(), ledger)?));
    fields.push((145, text_field("kb", ledger)?));
    fields.push((146, text_field("Knowledge Base", ledger)?));
    fields.push((147, text_field("created by bootstrap", ledger)?));
    let mut domains = reserved_slots::<u8>(4 + 4 + "default".len(), ledger)?;
    domains.extend_from_slice(&1u32.to_le_bytes());
    domains.extend_from_slice(&("default".len() as u32).to_le_bytes());
    domains.extend_from_slice(b"default");
    fields.push((148, domains));
    fields.push((163, bytes(&rules_digest, ledger)?));
    fields.push((166, bytes(&rules_ref, ledger)?));
    fields.push((196, bytes(&profile.as_u16().to_le_bytes(), ledger)?));
    let mut events = reserved_slots::<OwnedEvent>(10, ledger)?;
    events.push(OwnedEvent { tag: 224, fields });
    for space in 0u16..9 {
        let mut fields = reserved_slots::<(u16, Vec<u8>)>(2, ledger)?;
        fields.push((164, bytes(&space.to_le_bytes(), ledger)?));
        fields.push((165, bytes(&0u128.to_le_bytes(), ledger)?));
        events.push(OwnedEvent { tag: 176, fields });
    }
    let mut blobs = reserved_slots::<BlobDeclaration>(1, ledger)?;
    blobs.push(BlobDeclaration {
        sha256: rules_ref,
        declared_len: rules.len() as u64,
        source: BlobSource::Provide(bytes(rules, ledger)?),
    });
    Ok(PreparedTransaction {
        version: 1,
        prev_version: None,
        tx_id: [0x01; 16],
        kb_uuid,
        created_at: 0,
        principal_id: string("system", ledger)?,
        request_id: string("genesis", ledger)?,
        payload_digest: [0u8; 32],
        expect_version: None,
        extensions: Default::default(),
        events,
        blobs,
        rules: RulesBinding {
            profile,
            rules_digest,
            rules_ref,
        },
    })
}

// ---------------------------------------------------------------------------
// P1-5: replay, sync and resolution for an existing root
// ---------------------------------------------------------------------------

/// Replay an observed root from Genesis, publish a tagged view, then try to make it durable.
///
/// Called by [`crate::open::open_existing`] after acquisition and observation. Kept here because it needs the `Store`'s own
/// physical derivation (`fold_actions` + `apply_owning`), which is the same path commit uses — so revision continuity is
/// enforced by one checker, not two.
pub(crate) fn resolve_observed<F: FileSystem>(
    fs: F,
    root: F::Dir,
    families: crate::commit::Families<F>,
    lock: F::Lock,
    observed: crate::open::ObservedRoot,
    limits: &Limits,
    ledger: &mut dyn crate::lease::LeaseLedger,
) -> Result<crate::open::Opened<F>, crate::open::OpenRefusal> {
    use crate::open::Opened;

    let kb_uuid = observed.current.kb_uuid;
    let mut store = Store::open_reopening(fs, root, families, lock, kb_uuid);

    // Replay into a fold first: failure BEFORE materialization completes offers no handle and no view at all.
    let (fold, current_set, history_frames) =
        replay_into_fold(&store.fs, &store.families, &observed.chain, limits, ledger)?;
    let mut observed = observed;
    observed.referenced_blobs = committed_blob_union(&fold, ledger)?;
    store.publish_materialized(fold, current_set, &observed, history_frames, ledger)?;

    // Publish the tagged view BEFORE sync: the data is verified and replayed, so it is readable, but not yet durable.
    let basis = observed.basis();

    // Ordered whole-chain sync, even when CURRENT names an older root than files present.
    match sync_observed(&store, &observed, ledger) {
        Ok(()) => {
            // Promotion is one coherent state change: the same observation becomes Durable.
            store.promote_view_durable(basis)?;
            Ok(Opened::Ready(store))
        }
        // Verified but unsynced: readable, fenced, tagged Unverified. The view stays valid.
        Err((step, cause)) => {
            // Replay COMPLETED; only durability is missing. The view stays valid and tagged Unverified.
            store.fenced = true;
            Ok(Opened::ReadOnly { store, step, cause })
        }
    }
}

impl<F: FileSystem> Store<F> {
    /// Promote only the completed observation; readiness and its guards change together.
    fn promote_view_durable(
        &mut self,
        basis: crate::view::ViewBasis,
    ) -> Result<(), crate::open::OpenRefusal> {
        let current = self
            .current_view
            .as_ref()
            .filter(|view| view.basis() == basis)
            .ok_or(crate::open::OpenRefusal::PublicationBasisMismatch)?;
        self.current_view = Some(crate::view::TaggedView::new(
            current.snapshot_owned(),
            crate::view::ViewDurability::Durable,
            basis,
        ));
        self.coverage_complete = true;
        self.fenced = false;
        self.authority_undecided = false;
        Ok(())
    }
}

/// Replay the observed chain from Genesis, building the fold, the current pin and the durable-current slot.
///
/// Every version goes through `fold_actions` + `apply_owning`, so revision continuity is the SAME physical checker commit
/// uses. Each version's decoded payloads come from the canonical reader (`read_envelope` for the body, then `read_dispatched`
/// per event, which returns owned payloads by value) — never from a reduced header and never by cloning a borrow.
fn replay_into_fold<F: FileSystem>(
    fs: &F,
    families: &crate::commit::Families<F>,
    chain: &[crate::open::ChainRow],
    limits: &Limits,
    ledger: &mut dyn crate::lease::LeaseLedger,
) -> Result<
    (
        crate::fold::StateFold,
        Vec<StateEntry>,
        Option<crate::history::HistoryFrame>,
    ),
    crate::open::OpenRefusal,
> {
    use crate::open::{CorruptReason, OpenRefusal};
    use sage_db_types::framing::{FormatOutcome, Refusal};

    let head = chain
        .first()
        .ok_or(OpenRefusal::InvalidObservation)?
        .version;
    let mut fold = crate::fold::StateFold::new();
    let mut previous_chain = [0u8; 32];
    let mut history_frames = None;
    let mut tx_buf: Vec<u8> = Vec::new();
    // The canonical current set, merged per version with the same kind+key / latest semantics commit uses.
    let mut current_set: Vec<StateEntry> = Vec::new();
    // One version's freshly projected entries, merged then dropped.
    let mut version_entries: Vec<StateEntry> = Vec::new();
    // Canonical references carried by THIS version's committed events, admitted through the pass's own ledger.
    let mut version_refs: Vec<([u8; 32], u64)> = Vec::new();

    for row in chain.iter().rev() {
        // CORRECTION 1: the manifest NAMES the exact transaction. Read that named file; never enumerate the family to
        // rediscover which committed content belongs to this version.
        let tx_name = row.tx_name;
        crate::open::read_whole_strict(
            fs,
            &families.transactions,
            tx_name,
            &mut tx_buf,
            limits.max_tx_file_bytes,
            ledger,
        )?;

        // The recorded raw FILE hash is a file-identity claim: verify it against the bytes just read.
        let file_sha = match sage_db_format::digest::raw_sha256_bounded(&tx_buf, ledger) {
            Ok(digest) => digest,
            Err(error) => return Err(crate::open::refusal_from_hash(error)),
        };
        if file_sha != row.tx_file_sha256 {
            return Err(OpenRefusal::Corrupt(CorruptReason::TxFileHashMismatch));
        }

        // CORRECTION 3: parse/validate the transaction HEADER before dispatching events. The envelope body begins with its
        // header field set, so the header must be consumed first and its real facts kept.
        let envelope = match sage_db_format::transaction::read_envelope(&tx_buf, limits, ledger) {
            FormatOutcome::Ok(envelope) => envelope,
            FormatOutcome::Format(error) => return Err(OpenRefusal::Format(error)),
            FormatOutcome::Refused(refusal) => return Err(OpenRefusal::Refused(refusal)),
        };
        // The transaction id embedded in the file name, for the header's own self-check.
        let tx_id_bytes = match tx_name {
            crate::names::EntryName::Transaction(id) => *id.as_bytes(),
            _ => return Err(OpenRefusal::InvalidObservation),
        };

        // PASS 1 — full canonical composition. A no-op visitor keeps every partial callback as scratch; this pass exists to
        // decide composition (header, dense sequence, membership) with the canonical checker, not to retain payloads.
        let summary = match sage_db_format::transaction::read_transaction(
            &tx_buf,
            limits,
            Some(tx_id_bytes),
            &mut sage_db_format::transaction::DiscardEvents,
            ledger,
        ) {
            FormatOutcome::Ok(summary) => summary,
            // Preserve the canonical verdict classes rather than relabelling them as byte corruption.
            FormatOutcome::Format(error) => return Err(OpenRefusal::Format(error)),
            FormatOutcome::Refused(refusal) => return Err(OpenRefusal::Refused(refusal)),
        };
        let header = summary.header;
        if header.rules_profile != row.rules_profile {
            return Err(OpenRefusal::Corrupt(CorruptReason::HeaderManifestMismatch));
        }
        if header.prev_version != row.version - 1 {
            return Err(OpenRefusal::Corrupt(CorruptReason::VersionDiscontinuity));
        }
        // Cross-check the manifest's own claims against the validated header facts.
        if header.version != row.version {
            return Err(OpenRefusal::Corrupt(CorruptReason::VersionDiscontinuity));
        }
        if header.kb_uuid != row.kb_uuid {
            return Err(OpenRefusal::Corrupt(CorruptReason::HeaderManifestMismatch));
        }
        if header.rules_digest != row.rules_digest {
            return Err(OpenRefusal::Corrupt(CorruptReason::HeaderManifestMismatch));
        }
        // Version continuity through the chain's own predecessor claim: a genesis row has no predecessor and must declare 0.
        if row.prev_manifest_sha256.is_none() && header.version != 1 {
            return Err(OpenRefusal::Corrupt(CorruptReason::VersionDiscontinuity));
        }
        // created_at must agree on the SAME 64 wire bits: the header reads a u64, the manifest and normative tag 8 are i64,
        // so a lossy conversion (or one that rejects legitimately negative timestamps) would invent a mismatch.
        if header.created_at.to_be_bytes() != row.created_at.to_be_bytes() {
            return Err(OpenRefusal::Corrupt(CorruptReason::HeaderManifestMismatch));
        }

        // The chain digest must join this row to its predecessor: a false link is corruption even when the tail is
        // internally consistent, so this comparison cannot be skipped.
        {
            let tx_domain = match sage_db_format::digest::digest_bounded(
                sage_db_format::digest::Domain::Tx,
                &envelope.body,
                ledger,
            ) {
                Ok(digest) => digest,
                Err(error) => return Err(crate::open::refusal_from_hash(error)),
            };
            let expected_chain = sage_db_format::digest::chain_digest(previous_chain, tx_domain);
            if expected_chain != row.chain_digest {
                return Err(OpenRefusal::Corrupt(CorruptReason::HistoricalLinkFalse));
            }
            previous_chain = expected_chain;
        }

        // --- RAW PROJECTION PASS (the projector commit uses) ---
        //
        // Sequenced BEFORE the owned-dispatch pass, over the same envelope body. Each pass pays its own real reader costs
        // through the same ledger; nothing is prepaid twice and no moved payload is re-admitted.
        version_entries.clear();
        version_refs.clear();
        {
            let mut proj_reader = sage_db_format::wire::Reader::new(&envelope.body);
            // The body begins with the transaction header field set; consume it first.
            sage_db_format::event::read_header(&mut proj_reader, ledger)
                .map_err(crate::open::from_read_error)?;
            while !proj_reader.is_empty() {
                let raw = match sage_db_format::event::read_event(&mut proj_reader, ledger) {
                    Ok(raw) => raw,
                    Err(error) => return Err(crate::open::from_read_error(error)),
                };
                // THE PRODUCER FIX: enumerate this actual committed event's canonical references here, with the same ledger
                // that pays for the projection. Without this the committed union stays empty and the "sync every referenced
                // blob" loop iterates nothing — which is exactly the missing-historical-blob failure.
                {
                    let needed = version_refs
                        .len()
                        .checked_add(raw.fields().len())
                        .ok_or(OpenRefusal::ExtentOverflow)?;
                    crate::open::reserve_slots(&mut version_refs, needed, ledger)?;
                    let mut sink = RefSink {
                        out: &mut version_refs,
                    };
                    if let Err(refusal) = sage_db_format::dispatch::for_each_blob_ref_fields(
                        raw.fields(),
                        &mut sink,
                        ledger,
                    ) {
                        return Err(OpenRefusal::Refused(refusal));
                    }
                }
                match project_event(raw.tag(), raw.fields(), &mut version_entries, ledger) {
                    FormatOutcome::Ok(_) => {}
                    // Preserve the four-class verdict rather than collapsing it.
                    FormatOutcome::Format(error) => return Err(OpenRefusal::Format(error)),
                    FormatOutcome::Refused(refusal) => {
                        return Err(OpenRefusal::Refused(refusal));
                    }
                }
            }
        }
        // The canonical reader already validated composition. Prepare the owned physical action table.
        let mut actions: Vec<crate::fold::FoldEvent> = Vec::new();
        // The refs just enumerated from the raw pass become the fold's committed-reference actions, so the union is derived
        // from actual canonical event references across the whole chain.
        for (sha256, declared_len) in &mut version_refs {
            *declared_len = verify_blob(fs, families, *sha256, ledger)?;
        }
        let capacity = usize::try_from(summary.events_seen)
            .map_err(|_| OpenRefusal::ExtentOverflow)?
            .checked_mul(2)
            .and_then(|n| n.checked_add(version_refs.len()))
            .ok_or(OpenRefusal::ExtentOverflow)?;
        crate::open::reserve_slots(&mut actions, capacity, ledger)?;
        for (sha256, declared_len) in version_refs.drain(..) {
            actions.push(crate::fold::FoldEvent::CommittedBlobRef {
                sha256,
                declared_len,
            });
        }
        let mut wire_events: u32 = 0;
        // PASS 2 — owned dispatch, sequentially, handing each OWNED payload to the ONE bridge. Each real decode pays this
        // same ledger, so the second pass is admitted work rather than a free re-read.
        {
            let mut owned_reader = sage_db_format::wire::Reader::new(&envelope.body);
            // The header field set was already validated by pass 1, so any failure here keeps its OWN canonical class:
            // the format crate decides decode/schema/budget/stop/allocation, and this path only forwards that verdict.
            sage_db_format::event::read_header(&mut owned_reader, ledger)
                .map_err(crate::open::from_read_error)?;
            while !owned_reader.is_empty() {
                match sage_db_format::dispatch::read_dispatched(&mut owned_reader, ledger) {
                    FormatOutcome::Ok(sage_db_format::dispatch::DispatchedEvent::Record {
                        payload,
                        ..
                    }) => {
                        push_payload_actions(payload, &mut actions, ledger)
                            .map_err(OpenRefusal::Refused)?;
                        wire_events = wire_events.saturating_add(1);
                    }
                    FormatOutcome::Ok(sage_db_format::dispatch::DispatchedEvent::Validated {
                        family,
                        ..
                    }) => {
                        // An identified family whose record needs a caller-supplied key. Its references were enumerated in
                        // pass 0, and dropping it silently would drop committed content — so this stays the unsupported shape
                        // rather than a silently empty fold.
                        return Err(OpenRefusal::Unsupported(
                            crate::open::UnsupportedReason::UnmaterializedEventFamily(family),
                        ));
                    }
                    FormatOutcome::Format(error) => return Err(OpenRefusal::Format(error)),
                    FormatOutcome::Refused(refusal) => {
                        return Err(OpenRefusal::Refused(refusal));
                    }
                }
            }
        }

        // The header the fold records comes from the canonical read, so rules profile and created_at are the real ones.
        let version_row = crate::fold::FoldedVersion {
            version: row.version,
            // The real transaction id from the parsed header, joined against the file name it came from.
            tx_id: header.tx_id,
            // WIRE event count, not the expanded physical action count.
            event_count: wire_events,
            // The VALIDATED profile, not the manifest's copy alone: they were just cross-checked, and this is the fact the
            // snapshot must carry.
            rules_profile: header.rules_profile,
            rules_digest: row.rules_digest,
        };
        let kb = row.kb_uuid;
        match fold.apply_owning(version_row, kb, actions, ledger) {
            crate::fold::FoldOutcome::Ok(()) => {}
            crate::fold::FoldOutcome::Refused(refusal) => {
                return Err(OpenRefusal::Refused(refusal));
            }
            // Strict revision continuity lives in apply_owning -> insert_record.
            crate::fold::FoldOutcome::Defect(error) => {
                return Err(crate::open::fold_failure(error));
            }
        }

        verify_fold_rules(fs, families, &fold, row, limits, ledger)?;

        // Merge this version into the accumulated set, then check the digest against THIS manifest.
        merge_current_set(&mut current_set, &version_entries, ledger)?;
        let set = match RecordSet::from_sorted(current_set, ledger) {
            Ok(set) => set,
            Err(sage_db_types::persist::RecordSetRejection::Budget(error)) => {
                return Err(OpenRefusal::Refused(Refusal::Budget(error)));
            }
            Err(sage_db_types::persist::RecordSetRejection::Stopped(stop)) => {
                return Err(OpenRefusal::Refused(Refusal::Stopped(stop)));
            }
            Err(error @ sage_db_types::persist::RecordSetRejection::Order(_)) => {
                return Err(OpenRefusal::RecordSet(error));
            }
        };
        match sage_db_format::state::state_digest(&set, ledger) {
            Ok(digest) if digest == row.state_digest => {}
            Ok(_) => return Err(OpenRefusal::Corrupt(CorruptReason::StateDigestMismatch)),
            Err(error) => {
                return Err(crate::open::assembly_failure(error));
            }
        }

        current_set = set.into_entries();

        // GRAPH: recomputed from THIS version's fold through the same physical helper commit uses.
        let (terms, assertions) = match graph_from_fold(&fold, ledger) {
            Ok(pair) => pair,
            Err(refusal) => {
                return Err(crate::open::derivation_failure(refusal));
            }
        };
        match sage_db_format::graph::graph_digest(&terms, &assertions, ledger) {
            Ok(digest) if digest == row.graph_digest => {}
            Ok(_) => return Err(OpenRefusal::Corrupt(CorruptReason::GraphDigestMismatch)),
            Err(error) => {
                return Err(crate::open::assembly_failure(error));
            }
        }

        // BOOKKEEPING: outcomes/imports/counters from the same fold, compared with this manifest.
        let (outcomes, imports, counters) = match bookkeeping_from_fold(&fold, ledger) {
            Ok(rows) => rows,
            Err(refusal) => {
                return Err(crate::open::derivation_failure(refusal));
            }
        };
        match sage_db_format::bookkeeping::bookkeeping_digest(
            &outcomes, &imports, &counters, ledger,
        ) {
            Ok(digest) if digest == row.bookkeeping_digest => {}
            Ok(_) => {
                return Err(OpenRefusal::Corrupt(
                    CorruptReason::BookkeepingDigestMismatch,
                ));
            }
            Err(error) => {
                return Err(crate::open::assembly_failure(error));
            }
        }
        history_frames = Some(crate::history::HistoryFrame::from_image(
            row.version,
            header.tx_id,
            &tx_buf,
            history_frames,
            limits,
            ledger,
        )?);
    }

    if fold.head_version() != Some(head) {
        return Err(OpenRefusal::Corrupt(CorruptReason::VersionDiscontinuity));
    }
    Ok((fold, current_set, history_frames))
}

/// Map an open refusal onto the resolve outcome that means the same thing.
fn map_open_refusal(error: crate::open::OpenRefusal) -> crate::open::Resolution {
    use crate::open::{OpenRefusal, Resolution};
    match error {
        OpenRefusal::Corrupt(r) => Resolution::Corrupt(r),
        OpenRefusal::Unsupported(r) => Resolution::Unsupported(r),
        OpenRefusal::Filesystem { step, error } => Resolution::Filesystem { step, error },
        OpenRefusal::Refused(r) => Resolution::Incomplete(r),
        OpenRefusal::Format(e) => Resolution::Format(e),
        other => Resolution::Failed(other),
    }
}

/// The blob reference union the replayed fold actually committed.
///
/// The fold accumulates `CommittedBlobRef` per version, so this is the complete historical union rather than a tail-only or
/// current-only view — which is exactly what the durability sync must cover.
/// The complete committed blob union, admitted element-by-element and deduplicated.
///
/// The fold accumulates `CommittedBlobRef` per version, so this is the whole-chain union. Every slot is admitted through the
/// ledger BEFORE the table grows, and the result is checked by the same union the sync loop iterates — an unadmitted `collect`
/// would make the later "sync every referenced file" pass silently unbudgeted.
fn committed_blob_union(
    fold: &crate::fold::StateFold,
    ledger: &mut dyn crate::lease::LeaseLedger,
) -> Result<Vec<[u8; 32]>, crate::open::OpenRefusal> {
    let mut union = Vec::new();
    crate::open::reserve_slots(&mut union, fold.blobs().len(), ledger)?;
    for (digest, _) in fold.blobs() {
        ledger
            .try_charge_entries(1)
            .map_err(|e| crate::open::OpenRefusal::Refused(Refusal::Budget(e)))?;
        ledger
            .checkpoint()
            .map_err(|s| crate::open::OpenRefusal::Refused(Refusal::Stopped(s)))?;
        union.push(*digest);
    }
    Ok(union)
}

/// Validate the selected ancestor using the open/commit replay engine and derive
/// ALL historical blob references, including superseded record content.
pub(crate) fn backup_blob_union<F: FileSystem>(
    fs: &F,
    families: &crate::commit::Families<F>,
    chain: &[crate::open::ChainRow],
    limits: &Limits,
    ledger: &mut dyn crate::lease::LeaseLedger,
) -> Result<Vec<[u8; 32]>, crate::open::OpenRefusal> {
    let (fold, _current, _history) = replay_into_fold(fs, families, chain, limits, ledger)?;
    committed_blob_union(&fold, ledger)
}

/// Merge one version's freshly projected entries into the accumulated canonical current set.
///
/// Same semantics as commit's merge: one entry per `(kind, key)`, and a later revision supersedes an earlier one. A version that
/// does not touch a record leaves that record's existing row in place, which is what "CURRENT state, not this transaction's
/// projection" means. Copies here are real (the old row and the new row both exist for an instant), so each is admitted and
/// fallibly reserved rather than cloned infallibly.
fn merge_current_set(
    current: &mut Vec<StateEntry>,
    entries: &[StateEntry],
    ledger: &mut dyn crate::lease::LeaseLedger,
) -> Result<(), crate::open::OpenRefusal> {
    use crate::open::OpenRefusal;
    for entry in entries {
        ledger
            .try_charge_entries(1)
            .map_err(|e| OpenRefusal::Refused(Refusal::Budget(e)))?;
        ledger
            .checkpoint()
            .map_err(|s| OpenRefusal::Refused(Refusal::Stopped(s)))?;
        let row = copy_entry_admitted(entry, ledger).map_err(crate::open::derivation_failure)?;
        match current
            .binary_search_by(|e| (e.kind, e.key.as_slice()).cmp(&(row.kind, row.key.as_slice())))
        {
            Ok(at) => current[at] = row,
            Err(at) => {
                let needed = current
                    .len()
                    .checked_add(1)
                    .ok_or(OpenRefusal::ExtentOverflow)?;
                crate::open::reserve_slots(current, needed, ledger)?;
                current.insert(at, row);
            }
        }
    }
    Ok(())
}

/// Make the observed root durable: sync its whole chain in the contract's fixed order.
///
/// The order matters and is not negotiable: CURRENT file → root directory → all manifests (tail first) → manifests directory →
/// all transactions → transactions directory → all referenced blobs → blobs directory. It runs for the **observed** root even
/// when newer-looking files exist, because durability evidence is about what CURRENT names.
///
/// Every file uses the existing-file sync primitive; directory sync is a separate namespace proof.
fn sync_observed<F: FileSystem>(
    store: &Store<F>,
    observed: &crate::open::ObservedRoot,
    ledger: &mut dyn crate::lease::LeaseLedger,
) -> Result<(), (crate::open::OpenStep, crate::open::SyncFailure)> {
    use crate::fs::Family;
    sync_entry_step(
        &store.fs,
        &store.root,
        None,
        EntryName::Root(crate::names::RootName::Current),
        ledger,
    )?;
    sync_dir_step(&store.fs, &store.root, None, ledger)?;
    for row in &observed.chain {
        sync_entry_step(
            &store.fs,
            &store.families.manifests,
            Some(Family::Manifests),
            row.manifest_name,
            ledger,
        )?;
    }
    sync_dir_step(
        &store.fs,
        &store.families.manifests,
        Some(Family::Manifests),
        ledger,
    )?;
    for row in &observed.chain {
        let name = row.tx_name;
        sync_entry_step(
            &store.fs,
            &store.families.transactions,
            Some(Family::Transactions),
            name,
            ledger,
        )?;
    }
    sync_dir_step(
        &store.fs,
        &store.families.transactions,
        Some(Family::Transactions),
        ledger,
    )?;
    for digest in &observed.referenced_blobs {
        sync_entry_step(
            &store.fs,
            &store.families.blobs,
            Some(Family::Blobs),
            EntryName::Blob(*digest),
            ledger,
        )?;
    }
    sync_dir_step(
        &store.fs,
        &store.families.blobs,
        Some(Family::Blobs),
        ledger,
    )
}

fn sync_gate(
    step: crate::open::OpenStep,
    ledger: &mut dyn crate::lease::LeaseLedger,
) -> Result<(), (crate::open::OpenStep, crate::open::SyncFailure)> {
    ledger
        .try_charge_entries(1)
        .map_err(|e| (step, crate::open::SyncFailure::Refused(Refusal::Budget(e))))?;
    sync_checkpoint(step, ledger)
}

fn sync_checkpoint(
    step: crate::open::OpenStep,
    ledger: &mut dyn crate::lease::LeaseLedger,
) -> Result<(), (crate::open::OpenStep, crate::open::SyncFailure)> {
    ledger
        .checkpoint()
        .map_err(|s| (step, crate::open::SyncFailure::Refused(Refusal::Stopped(s))))
}

fn sync_error(error: FsError) -> crate::open::SyncFailure {
    if let Some(budget) = error.budget() {
        return crate::open::SyncFailure::Refused(Refusal::Budget(budget));
    }
    if let Some(stop) = error.control() {
        return crate::open::SyncFailure::Refused(Refusal::Stopped(stop));
    }
    crate::open::SyncFailure::Filesystem(error)
}

fn sync_entry_step<F: FileSystem>(
    fs: &F,
    dir: &F::Dir,
    family: Option<crate::fs::Family>,
    name: EntryName,
    ledger: &mut dyn crate::lease::LeaseLedger,
) -> Result<(), (crate::open::OpenStep, crate::open::SyncFailure)> {
    let step = crate::open::OpenStep::SyncFile { family, name };
    sync_gate(step, ledger)?;
    fs.sync_entry(dir, name)
        .map_err(|e| (step, sync_error(e)))?;
    sync_checkpoint(step, ledger)
}

fn sync_dir_step<F: FileSystem>(
    fs: &F,
    dir: &F::Dir,
    family: Option<crate::fs::Family>,
    ledger: &mut dyn crate::lease::LeaseLedger,
) -> Result<(), (crate::open::OpenStep, crate::open::SyncFailure)> {
    let step = crate::open::OpenStep::SyncDirectory(family);
    sync_gate(step, ledger)?;
    fs.sync_dir(dir).map_err(|e| (step, sync_error(e)))?;
    sync_checkpoint(step, ledger)
}

/// Verify the named immutable bytes, including references only present in old committed transactions.
pub(crate) fn verify_blob<F: FileSystem>(
    fs: &F,
    families: &crate::commit::Families<F>,
    digest: [u8; 32],
    ledger: &mut dyn crate::lease::LeaseLedger,
) -> Result<u64, crate::open::OpenRefusal> {
    use crate::open::{CorruptReason, OpenRefusal, OpenStep};
    ledger
        .try_charge_entries(1)
        .map_err(|e| OpenRefusal::Refused(Refusal::Budget(e)))?;
    ledger
        .checkpoint()
        .map_err(|s| OpenRefusal::Refused(Refusal::Stopped(s)))?;
    let entry = EntryName::Blob(digest);
    let size = match fs.size(&families.blobs, entry) {
        Ok(size) => size,
        Err(error) if error.kind() == FsErrorKind::NotFound => {
            return Err(OpenRefusal::Corrupt(CorruptReason::MissingBlob));
        }
        Err(error) => return Err(crate::open::fs_failure(OpenStep::Hash, error)),
    };
    let report = fs
        .hash_bounded(&families.blobs, entry, size, ledger)
        .map_err(|e| crate::open::fs_failure(OpenStep::Hash, e))?;
    if report.sha256 != digest || report.len != size {
        return Err(OpenRefusal::Corrupt(CorruptReason::BlobMismatch));
    }
    Ok(size)
}

fn verify_fold_rules<F: FileSystem>(
    fs: &F,
    families: &crate::commit::Families<F>,
    fold: &crate::fold::StateFold,
    row: &crate::open::ChainRow,
    limits: &Limits,
    ledger: &mut dyn crate::lease::LeaseLedger,
) -> Result<(), crate::open::OpenRefusal> {
    use crate::open::{CorruptReason, OpenRefusal, OpenStep};
    let mut rules = None;
    for record in fold.current_records() {
        ledger
            .try_charge_entries(1)
            .map_err(|e| OpenRefusal::Refused(Refusal::Budget(e)))?;
        ledger
            .checkpoint()
            .map_err(|s| OpenRefusal::Refused(Refusal::Stopped(s)))?;
        if let crate::fold::FoldedRecord::RuleConfig(config) = record {
            rules = Some(config);
            break;
        }
    }
    let config = rules.ok_or(OpenRefusal::MissingRulesState)?;
    if config.profile.as_u16() != row.rules_profile || config.rules_digest != row.rules_digest {
        return Err(OpenRefusal::Corrupt(CorruptReason::RulesBindingMismatch));
    }
    let size = verify_blob(fs, families, config.rules_ref, ledger)?;
    let mut bytes = Vec::new();
    crate::open::read_whole(
        fs,
        &families.blobs,
        EntryName::Blob(config.rules_ref),
        &mut bytes,
        OpenStep::Hash,
        size,
        ledger,
    )?;
    let declared = sage_db_format::rules_read::DeclaredRules {
        rules_ref: config.rules_ref,
        rules_digest: row.rules_digest,
        profile: config.profile,
    };
    match sage_db_format::rules_read::verify_rules(&bytes, &declared, limits, ledger) {
        sage_db_types::framing::FormatOutcome::Ok(_) => Ok(()),
        sage_db_types::framing::FormatOutcome::Format(e) => Err(OpenRefusal::Format(e)),
        sage_db_types::framing::FormatOutcome::Refused(r) => Err(OpenRefusal::Refused(r)),
    }
}

/// Reuse the physical payload mapping; never clone a borrowed decoded record.
pub(crate) fn history_image(
    payload: sage_db_format::dispatch::DispatchedPayload,
    identity: &crate::fold::RecordIdentity,
    revision: u64,
    fold: &crate::fold::StateFold,
    ledger: &mut dyn crate::lease::LeaseLedger,
) -> Result<Option<crate::history::HistoryImage>, crate::open::OpenRefusal> {
    let mut actions = reserved_slots(2, ledger).map_err(crate::open::derivation_failure)?;
    push_payload_actions(payload, &mut actions, ledger)
        .map_err(crate::open::OpenRefusal::Refused)?;
    for action in actions {
        if let crate::fold::FoldEvent::Record(record) = action {
            if record.revision().get() != revision {
                continue;
            }
            if &record
                .identity(ledger)
                .map_err(crate::open::OpenRefusal::Refused)?
                != identity
            {
                continue;
            }
            let shared = fold
                .revision_owner(identity, revision)
                .ok_or(crate::open::OpenRefusal::HistoryFrameMismatch)?;
            if shared.get() != &record {
                return Err(crate::open::OpenRefusal::HistoryFrameMismatch);
            }
            return Ok(Some(crate::history::HistoryImage::new(shared)));
        }
    }
    Ok(None)
}

/// Verify only references actually present in this matched wire event.
pub(crate) fn verify_event_refs<F: FileSystem>(
    fs: &F,
    families: &crate::commit::Families<F>,
    fields: &[sage_db_format::event::Field<'_>],
    ledger: &mut dyn crate::lease::LeaseLedger,
) -> Result<(), crate::open::OpenRefusal> {
    let mut references = Vec::new();
    crate::open::reserve_slots(&mut references, fields.len(), ledger)?;
    let mut sink = RefSink {
        out: &mut references,
    };
    sage_db_format::dispatch::for_each_blob_ref_fields(fields, &mut sink, ledger)
        .map_err(crate::open::OpenRefusal::Refused)?;
    for (digest, _) in references {
        verify_blob(fs, families, digest, ledger)?;
    }
    Ok(())
}

#[cfg(test)]
mod p15_physical_projection_tests {
    use super::*;
    #[test]
    fn genesis_reason_is_preserved_in_both_physical_after_images() {
        let control = sage_db_types::query::RunToCompletion;
        let mut ledger = crate::lease::AttemptLedger::open(1 << 24, 1 << 20, &control).unwrap();
        let rules = include_bytes!("../../format/tests/fixtures/rules-verify/rules.formal.bin");
        let mut carrier =
            genesis_candidate(KbUuid::from_bytes([0x19; 16]), rules, &mut ledger).unwrap();
        let reason = "original genesis reason";
        let mut value = (reason.len() as u32).to_le_bytes().to_vec();
        value.extend_from_slice(reason.as_bytes());
        carrier.events[0].fields.push((7, value));
        carrier.events[0].fields.sort_by_key(|(tag, _)| *tag);
        let actions = fold_actions(&carrier.events, Vec::new(), &Limits::V1, &mut ledger).unwrap();
        let mut records = 0;
        for action in actions {
            match action {
                crate::fold::FoldEvent::Record(crate::fold::FoldedRecord::KbMetadata(record)) => {
                    assert_eq!(record.audit.reason.as_deref(), Some(reason));
                    records += 1;
                }
                crate::fold::FoldEvent::Record(crate::fold::FoldedRecord::RuleConfig(record)) => {
                    assert_eq!(record.audit.reason.as_deref(), Some(reason));
                    records += 1;
                }
                _ => {}
            }
        }
        assert_eq!(
            records, 2,
            "one actual Genesis event establishes both singleton records"
        );
    }
}
