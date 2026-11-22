//! Recorded-response assembly.
//!
//! # What this is
//!
//! One layer that turns a kernel [`HandleOutcome`] (or a stored outcome lookup) into the
//! **generated** protocol response DTOs. It reads the canonical result blob the kernel recorded,
//! parses it with the canonical borrowed decoder, cross-checks all three version sources, and
//! fills the DTO's full affected/counter lists.
//!
//! # Ownership and accounting
//!
//! Every buffer this module produces is **admitted to the caller's ledger before it is
//! allocated**, and the resulting credit is adopted into a [`Response`] that owns it:
//!
//! 1. measure the exact extent (table slot bytes plus each string's byte length, computed from
//!    the values themselves — a decimal is at most 39 bytes, a digest 64, a uuid 32);
//! 2. `try_charge` that extent, then `adopt_retained` it into one [`OwnedReservation`];
//! 3. only then allocate, fallibly, at the measured sizes.
//!
//! A refused allocation is [`Refusal::Allocation`] carrying the exact extent that failed, so it
//! is never confused with a budget refusal. Attempt capacity that is never adopted is released
//! when the attempt drops; admitted credit held by a [`Response`] is released when **it** drops.
//!
//! Strings are built from stack-formatted decimal/hex/UUID buffers; there is no `to_string`, no
//! `to_owned`, and no `with_capacity` on this path.
//!
//! # What this is not
//!
//! * **No transport.** No HTTP, RPC, framing, or serialisation. The DTOs are returned
//!   populated; encoding them on a wire is later work.
//! * **No second decoder.** `sage_db_store::typed::read_result_payload` parses the blob.
//! * **No second authority for identity.** Proposal id, version and counters come from the
//!   *recorded* outcome, never from the request.
//! * **No re-authorization.** [`assemble_outcome`] takes an already-performed [`OutcomeLookup`].
//!
//! # Preconditions on the outcome path
//!
//! [`assemble_outcome`] performs a real `read_committed_blob`, which opens the root the store
//! already holds. It does **not** re-run the import gate: the caller is expected to have
//! authenticated the request and taken the lookup through the store's own import-and-durability
//! gate. That is a deliberate contract — one authority for authentication, one for the durable
//! basis — stated here rather than duplicated.

use sage_db_protocol::generated::{
    AlreadyCommitted, CommandError, Committed, Counter, ErrorCode, OutcomeAbsent, OutcomeFound,
    OutcomeIncomplete, ProposalStatus as WireProposalStatus, RecordKey as WireKey, RecordedCommit,
    RevPair as WireRevPair, ReviewCommitted, SubmitCommitted, Unknown, ViewBasis as WireViewBasis,
    WriteOutcome, committed, write_outcome,
};
use sage_db_store::commit::RecordedOutcome;
use sage_db_store::fold::RecordIdentity;
use sage_db_store::handle::{HandleOutcome, Store};
use sage_db_store::history::{HistoryBudget, HistoryIncomplete, HistoryOutcome};
use sage_db_store::lease::{LeaseLedger, OwnedReservation};
use sage_db_store::open::{OpenRefusal, OutcomeIncomplete as StoreIncomplete, OutcomeLookup};
use sage_db_store::typed;
use sage_db_store::view::BlobReadError;
use sage_db_types::framing::{Limits, Refusal};
use sage_db_types::identity::RecordRevision;
use sage_db_types::persist::{ProposalStatus, RecordKey, RecordKind, RequestOutcomeEvent};

/// Which write method produced an outcome.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WriteFamily {
    /// A submission: the success detail names the created proposal.
    Submit,
    /// A review: the success detail names the proposal and its persisted status.
    Review,
    /// A commit command: no method-specific detail applies.
    Commit,
}

/// Why a response could not be assembled.
///
/// Every variant states what was actually observed; none invents a version, a status or a
/// successful outcome.
#[derive(Debug)]
pub enum ResponseError {
    /// The caller's ledger refused budget, or a stop arrived.
    Refused(Refusal),
    /// An output buffer could not be allocated at its admitted extent.
    ///
    /// Distinct from [`Self::Refused`]: the admission succeeded and the **allocator** failed,
    /// so this is the only variant that means an allocator was actually consulted.
    Allocation {
        /// Bytes the allocation needed.
        requested: usize,
    },
    /// A measured extent cannot be represented on this platform.
    ///
    /// Pure arithmetic: no allocator was consulted, so this is never [`Self::Allocation`].
    ExtentOverflow {
        /// Number of elements whose extent could not be expressed.
        elements: usize,
    },
    /// Retained credit did not match what this attempt had admitted.
    ///
    /// The store documents this as neither budget nor allocation: no allowance would produce
    /// credit that was never admitted, and no allocator was consulted. Reporting it as
    /// `Allocation` would contradict the definition it came from.
    Transfer,
    /// A canonical reader declined an index it had already counted.
    ///
    /// The reader validates its own counts, so this cannot happen on well-formed bytes; it is a
    /// local invariant defect and must never be allowed to truncate a result silently.
    ShortCanonicalRead {
        /// Which table.
        table: &'static str,
        /// The zero-based index the reader declined.
        index: u32,
    },
    /// The outcome event is missing where one is required (genesis recorded no outcome).
    NoRecordedOutcome,
    /// The recorded result blob could not be read within the caller's ceiling.
    Blob(BlobReadError),
    /// The recorded result blob did not parse as a canonical result payload.
    Result(typed::ResultReadError),
    /// The three version sources disagree.
    ///
    /// All three are durable facts; a disagreement is an inconsistency, not caller input.
    VersionMismatch {
        /// The version the commit machinery reported (or the lookup's original version).
        outcome: u64,
        /// The version inside the outcome event.
        recorded: u64,
        /// The version inside the result payload the event points at.
        result: u64,
    },
    /// A revision in the recorded result is not a valid revision.
    InvalidRevision {
        /// The offending revision value.
        value: u64,
    },
    /// The reviewed proposal's status could not be established within the caller's bounds.
    StatusUnknown,
    /// The historical search covered the basis and found nothing there.
    HistoryAbsent,
    /// The historical search could not finish.
    HistoryIncomplete(HistoryIncomplete),
    /// The historical read failed definitively.
    ///
    /// A real I/O, format or corruption verdict. It is **not** absence: folding it into
    /// [`Self::HistoryAbsent`] would tell the caller the original status is simply not recorded,
    /// sending an operator to the wrong remedy.
    HistoryError(OpenRefusal),
    /// The historical record is not a proposal, so it cannot carry a review status.
    HistoryWrongFamily,
}

impl From<Refusal> for ResponseError {
    fn from(refusal: Refusal) -> Self {
        Self::Refused(refusal)
    }
}

/// A retained, fully-populated response and the credit that pays for it.
///
/// The DTO is reachable only by borrow: a caller cannot move buffers out from under the credit.
/// Dropping this releases exactly the admitted bytes.
pub struct Response {
    outcome: WriteOutcome,
    /// The adopted credit for every buffer inside `outcome`.
    ///
    /// A fixed array, not a `Vec`: holding the tokens must not itself allocate. Each occupied
    /// slot owns one measured extent; empty slots hold nothing.
    credits: Credits,
}

/// Up to three adopted extents, held without allocation.
///
/// Three is the maximum any one response builds: the recorded body, the method detail, and the
/// fixed refusal/ambiguity text.
#[derive(Default)]
struct Credits([Option<OwnedReservation>; 3]);

impl Credits {
    fn push(&mut self, token: OwnedReservation) -> Result<(), ResponseError> {
        for slot in &mut self.0 {
            if slot.is_none() {
                *slot = Some(token);
                return Ok(());
            }
        }
        // Unreachable by construction: no path mints more than three.
        Err(ResponseError::Allocation { requested: 0 })
    }

    fn total(&self) -> u64 {
        self.0
            .iter()
            .flatten()
            .map(sage_db_store::lease::OwnedReservation::bytes)
            .sum()
    }
}

impl core::fmt::Debug for Credits {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Credits")
            .field("bytes", &self.total())
            .finish()
    }
}

impl Response {
    /// Borrow the populated protocol outcome.
    #[must_use]
    pub const fn outcome(&self) -> &WriteOutcome {
        &self.outcome
    }

    /// Bytes this response holds admitted for.
    #[must_use]
    pub fn admitted_bytes(&self) -> u64 {
        self.credits.total()
    }
}

impl core::fmt::Debug for Response {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Response")
            .field("admitted_bytes", &self.credits.total())
            .finish_non_exhaustive()
    }
}

/// Settings one assembly call needs.
pub struct ResponseLimits {
    /// Largest result blob the caller will read, in bytes.
    pub max_result_bytes: u64,
    /// The kernel's canonical bounds, for the history scan.
    pub limits: Limits,
    /// Largest number of events the historical status search may visit.
    pub max_history_events: u64,
}

// ---------------------------------------------------------------------------
// Measured extents and admission
// ---------------------------------------------------------------------------

/// A checked byte extent built from real element sizes.
#[derive(Clone, Copy, Default, Debug)]
struct Extent {
    bytes: u64,
}

impl Extent {
    /// Slots for `count` elements of `T`.
    fn slots<T>(&mut self, count: usize) -> Result<(), ResponseError> {
        let each = u64::try_from(core::mem::size_of::<T>())
            .map_err(|_| ResponseError::ExtentOverflow { elements: count })?;
        let total = u64::try_from(count)
            .ok()
            .and_then(|count| count.checked_mul(each))
            .ok_or(ResponseError::ExtentOverflow { elements: count })?;
        self.bytes = self
            .bytes
            .checked_add(total)
            .ok_or(ResponseError::ExtentOverflow { elements: count })?;
        Ok(())
    }

    /// Text of exactly `bytes`.
    fn text(&mut self, bytes: usize) -> Result<(), ResponseError> {
        let added =
            u64::try_from(bytes).map_err(|_| ResponseError::ExtentOverflow { elements: bytes })?;
        self.bytes = self
            .bytes
            .checked_add(added)
            .ok_or(ResponseError::ExtentOverflow { elements: bytes })?;
        Ok(())
    }

    /// Admit this extent and hand back ownership of it.
    ///
    /// Charging adds it to this attempt's untransferred credit; adopting moves exactly that
    /// credit into a token. Nothing is topped up afterwards.
    fn admit(self, ledger: &mut dyn LeaseLedger) -> Result<OwnedReservation, ResponseError> {
        ledger
            .try_charge(self.bytes)
            .map_err(|error| ResponseError::Refused(Refusal::Budget(error)))?;
        // Not budget, not allocation: the extent was never admitted, so no allowance and no
        // allocator produced it. Exhaustively named, with no fallback cast.
        ledger.adopt_retained(self.bytes).map_err(
            |sage_db_store::lease::TransferError::Untransferred { .. }| ResponseError::Transfer,
        )
    }
}

/// Decimal digit count of `u128`, without allocating.
fn decimal_len_u128(mut value: u128) -> usize {
    let mut len = 1;
    while value >= 10 {
        value /= 10;
        len += 1;
    }
    len
}

fn decimal_len_u64(value: u64) -> usize {
    decimal_len_u128(u128::from(value))
}

/// Write a decimal into a stack buffer and borrow it.
fn decimal_u128_into(value: u128, buf: &mut [u8; 40]) -> &str {
    let mut index = 40;
    let mut value = value;
    loop {
        index -= 1;
        buf[index] = b'0' + u8::try_from(value % 10).unwrap_or(0);
        value /= 10;
        if value == 0 {
            break;
        }
    }
    core::str::from_utf8(&buf[index..]).unwrap_or("0")
}

fn decimal_u64_into(value: u64, buf: &mut [u8; 40]) -> &str {
    decimal_u128_into(u128::from(value), buf)
}

/// Write 32 bytes as lowercase hex into a stack buffer and borrow it.
fn hex32_into<'a>(bytes: &[u8; 32], buf: &'a mut [u8; 64]) -> &'a str {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    for (index, byte) in bytes.iter().enumerate() {
        buf[index * 2] = DIGITS[usize::from(byte >> 4)];
        buf[index * 2 + 1] = DIGITS[usize::from(byte & 0x0f)];
    }
    core::str::from_utf8(&buf[..64]).unwrap_or("")
}

/// Write a KB uuid as lowercase hex into a stack buffer and borrow it.
fn uuid_into<'a>(kb: &sage_db_types::identity::KbUuid, buf: &'a mut [u8; 32]) -> &'a str {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    for (index, byte) in kb.as_bytes().iter().enumerate() {
        buf[index * 2] = DIGITS[usize::from(byte >> 4)];
        buf[index * 2 + 1] = DIGITS[usize::from(byte & 0x0f)];
    }
    core::str::from_utf8(&buf[..32]).unwrap_or("")
}

/// Append `text` to `out`, reserving its exact length fallibly.
fn push(out: &mut String, text: &str) -> Result<(), ResponseError> {
    out.try_reserve_exact(text.len())
        .map_err(|_| ResponseError::Allocation {
            requested: text.len(),
        })?;
    out.push_str(text);
    Ok(())
}

/// Reserve `count` slots fallibly, reporting the real slot extent on refusal.
///
/// The product is computed **before** the reservation and with checked arithmetic: a saturated
/// figure would be a number that is not the real requirement in exactly the case a caller needs
/// it. If the product is not representable, that is arithmetic, not an allocation failure.
fn reserve_slots<T>(vec: &mut Vec<T>, count: usize) -> Result<(), ResponseError> {
    let elements = u64::try_from(core::mem::size_of::<T>())
        .ok()
        .and_then(|each| u64::try_from(count).ok()?.checked_mul(each))
        .and_then(|bytes| usize::try_from(bytes).ok())
        .ok_or(ResponseError::ExtentOverflow { elements: count })?;
    vec.try_reserve_exact(count)
        .map_err(|_| ResponseError::Allocation {
            requested: elements,
        })
}

// ---------------------------------------------------------------------------
// Version identity: one helper, all three paths
// ---------------------------------------------------------------------------

/// Require the outcome, the event and the result to name the same version.
///
/// The commit machinery reports one version, the outcome event records one, and the result
/// payload it points at states one. All three are durable facts; a disagreement means the
/// response would present a commit under a version its own outcome does not claim.
fn require_same_version(outcome: u64, recorded: u64, result: u64) -> Result<(), ResponseError> {
    if outcome == recorded && recorded == result {
        return Ok(());
    }
    Err(ResponseError::VersionMismatch {
        outcome,
        recorded,
        result,
    })
}

// ---------------------------------------------------------------------------
// The one recorded-body builder
// ---------------------------------------------------------------------------

/// Read, verify and build the canonical `RecordedCommit` from a recorded outcome event.
///
/// Returns the DTO and the credit that owns its buffers. The single builder serves all three
/// write methods **and** the outcome-lookup found path, so they cannot disagree about identity.
fn build_recorded(
    store: &mut Store<impl sage_db_store::fs::FileSystem>,
    event: &RequestOutcomeEvent,
    outcome_version: u64,
    limits: &ResponseLimits,
    ledger: &mut dyn LeaseLedger,
) -> Result<(RecordedCommit, Option<RecordKey>, OwnedReservation), ResponseError> {
    let blob = store
        .read_committed_blob(event.result_ref, limits.max_result_bytes, ledger)
        .map_err(ResponseError::Blob)?;
    let parsed = typed::read_result_payload(blob.as_bytes(), limits.max_result_bytes, ledger)
        .map_err(ResponseError::Result)?;

    // All three sources, through one helper.
    require_same_version(outcome_version, event.commit_version, parsed.version())?;

    // Measure from the parsed counts and computable digit widths — no allocation yet.
    // A count that does not fit `usize` is arithmetic, not an allocator refusal: nothing was
    // reserved, so naming `Allocation` would implicate an allocator that never ran.
    let affected_count =
        usize::try_from(parsed.affected_count()).map_err(|_| ResponseError::ExtentOverflow {
            elements: parsed.affected_count() as usize,
        })?;
    let counter_count =
        usize::try_from(parsed.counter_count()).map_err(|_| ResponseError::ExtentOverflow {
            elements: parsed.counter_count() as usize,
        })?;

    let mut extent = Extent::default();
    extent.slots::<WireRevPair>(affected_count)?;
    extent.slots::<Counter>(counter_count)?;
    // Per affected row: the key id (digits of the id) and the revision (digits).
    for index in 0..parsed.affected_count() {
        let pair = parsed
            .affected_at(index, ledger)
            .map_err(ResponseError::Result)?
            .ok_or(ResponseError::ShortCanonicalRead {
                table: "affected",
                index,
            })?;
        extent.text(decimal_len_u128(pair.key.id()))?;
        extent.text(decimal_len_u64(pair.revision.get()))?;
    }
    for index in 0..parsed.counter_count() {
        let mark = parsed
            .counter_at(index, ledger)
            .map_err(ResponseError::Result)?
            .ok_or(ResponseError::ShortCanonicalRead {
                table: "counters",
                index,
            })?;
        extent.text(decimal_len_u128(mark.hwm))?;
    }
    extent.text(decimal_len_u64(parsed.version()))?;
    extent.text(64)?;

    // Admit, then allocate. Nothing below may top up.
    let credit = extent.admit(ledger)?;

    let mut affected: Vec<WireRevPair> = Vec::new();
    reserve_slots(&mut affected, affected_count)?;
    let mut proposal_key: Option<RecordKey> = None;
    for index in 0..parsed.affected_count() {
        let pair = parsed
            .affected_at(index, ledger)
            .map_err(ResponseError::Result)?
            .ok_or(ResponseError::ShortCanonicalRead {
                table: "affected",
                index,
            })?;
        // Reject a zero revision rather than presenting it: no current record has revision 0.
        if RecordRevision::new(pair.revision.get()).is_none() {
            return Err(ResponseError::InvalidRevision {
                value: pair.revision.get(),
            });
        }
        let mut id_buf = [0u8; 40];
        let mut revision_buf = [0u8; 40];
        let mut id = String::new();
        push(&mut id, decimal_u128_into(pair.key.id(), &mut id_buf))?;
        let mut revision = String::new();
        push(
            &mut revision,
            decimal_u64_into(pair.revision.get(), &mut revision_buf),
        )?;
        if pair.key.kind() == RecordKind::Proposal && proposal_key.is_none() {
            proposal_key = Some(pair.key);
        }
        affected.push(WireRevPair {
            key: Some(WireKey {
                kind: u32::from(pair.key.kind().as_u16()),
                id,
            }),
            revision,
        });
    }

    let mut counters: Vec<Counter> = Vec::new();
    reserve_slots(&mut counters, counter_count)?;
    for index in 0..parsed.counter_count() {
        let mark = parsed
            .counter_at(index, ledger)
            .map_err(ResponseError::Result)?
            .ok_or(ResponseError::ShortCanonicalRead {
                table: "counters",
                index,
            })?;
        let mut hwm_buf = [0u8; 40];
        let mut hwm = String::new();
        push(&mut hwm, decimal_u128_into(mark.hwm, &mut hwm_buf))?;
        counters.push(Counter {
            space: u32::from(mark.space.as_u16()),
            hwm,
        });
    }

    let mut version_buf = [0u8; 40];
    let mut commit_version = String::new();
    push(
        &mut commit_version,
        decimal_u64_into(parsed.version(), &mut version_buf),
    )?;
    let mut result_buf = [0u8; 64];
    let mut result_ref = String::new();
    push(
        &mut result_ref,
        hex32_into(&event.result_ref, &mut result_buf),
    )?;

    Ok((
        RecordedCommit {
            affected,
            counters,
            commit_version,
            result_ref,
        },
        proposal_key,
        credit,
    ))
}

// ---------------------------------------------------------------------------
// Write methods
// ---------------------------------------------------------------------------

/// Assemble the response for a write command.
///
/// `request_id` is the **authenticated** envelope's id. It is the only content an ambiguous
/// outcome carries, so it is required rather than defaulted.
///
/// # Errors
///
/// [`ResponseError`] when the outcome cannot be stated truthfully: a missing outcome event, an
/// admitted allocation that failed, a blob/parse/version inconsistency, or a review status that
/// cannot be established within the caller's bounds.
pub fn assemble_write<F: sage_db_store::fs::FileSystem>(
    store: &mut Store<F>,
    outcome: &HandleOutcome,
    family: WriteFamily,
    request_id: &str,
    limits: &ResponseLimits,
    ledger: &mut dyn LeaseLedger,
) -> Result<Response, ResponseError> {
    match outcome {
        HandleOutcome::Committed { version, recorded } => {
            let event = event_of(recorded)?;
            let (recorded_dto, proposal_key, credit) =
                build_recorded(store, event, *version, limits, ledger)?;
            let status = match family {
                WriteFamily::Review => {
                    Some(original_status(store, event, *version, limits, ledger)?)
                }
                WriteFamily::Submit | WriteFamily::Commit => None,
            };
            let mut credits = Credits::default();
            credits.push(credit)?;
            // The detail names the proposal from the RECORD. Its id text is admitted here
            // rather than cloned out of the recorded row.
            let detail = match family {
                WriteFamily::Submit | WriteFamily::Review => {
                    let key = proposal_key.ok_or(ResponseError::StatusUnknown)?;
                    let mut extent = Extent::default();
                    extent.text(decimal_len_u128(key.id()))?;
                    credits.push(extent.admit(ledger)?)?;
                    let mut id_buf = [0u8; 40];
                    let mut id = String::new();
                    push(&mut id, decimal_u128_into(key.id(), &mut id_buf))?;
                    let wire = WireKey {
                        kind: u32::from(key.kind().as_u16()),
                        id,
                    };
                    Some(match family {
                        WriteFamily::Submit => committed::Detail::Submit(SubmitCommitted {
                            proposal: Some(wire),
                        }),
                        _ => committed::Detail::Review(ReviewCommitted {
                            proposal: Some(wire),
                            status: status.map_or(0, |s| wire_status(s) as i32),
                        }),
                    })
                }
                WriteFamily::Commit => None,
            };
            Ok(Response {
                outcome: WriteOutcome {
                    outcome: Some(write_outcome::Outcome::Committed(Committed {
                        recorded: Some(recorded_dto),
                        detail,
                    })),
                },
                credits,
            })
        }
        HandleOutcome::AlreadyCommitted { version, recorded } => {
            let event = event_of(recorded)?;
            let (recorded_dto, _proposal_key, credit) =
                build_recorded(store, event, *version, limits, ledger)?;
            let status = match family {
                WriteFamily::Review => {
                    Some(original_status(store, event, *version, limits, ledger)?)
                }
                WriteFamily::Submit | WriteFamily::Commit => None,
            };
            let mut credits = Credits::default();
            credits.push(credit)?;
            Ok(Response {
                outcome: WriteOutcome {
                    outcome: Some(write_outcome::Outcome::AlreadyCommitted(AlreadyCommitted {
                        recorded: Some(recorded_dto),
                        original_status: status.map(|s| wire_status(s) as i32),
                    })),
                },
                credits,
            })
        }
        HandleOutcome::Unknown => {
            // The ambiguity IS the request: no version, no result read, no latest fallback.
            // The id is admitted so the wire message can actually be resolved or retried.
            let mut extent = Extent::default();
            extent.text(request_id.len())?;
            let credit = extent.admit(ledger)?;
            let mut id = String::new();
            push(&mut id, request_id)?;
            Ok(Response {
                outcome: WriteOutcome {
                    outcome: Some(write_outcome::Outcome::Unknown(Unknown { request_id: id })),
                },
                credits: {
                    let mut credits = Credits::default();
                    credits.push(credit)?;
                    credits
                },
            })
        }
        HandleOutcome::NotCommitted { reason } => {
            // A refusal carries a fixed phrase, plus figures ONLY when a real `BudgetError`
            // was in hand. Every buffer here is measured before it is admitted.
            let class = refusal_class(reason);
            let mut extent = Extent::default();
            extent.text(class.message.len())?;
            if let Some((_, requested, remaining)) = &class.budget {
                extent.text(decimal_len_u64(*requested))?;
                extent.text(decimal_len_u64(*remaining))?;
            }
            let credit = extent.admit(ledger)?;

            let mut message = String::new();
            push(&mut message, class.message)?;
            let budget = match class.budget {
                None => None,
                Some((units, requested, remaining)) => {
                    let mut requested_buf = [0u8; 40];
                    let mut requested_text = String::new();
                    push(
                        &mut requested_text,
                        decimal_u64_into(requested, &mut requested_buf),
                    )?;
                    let mut remaining_buf = [0u8; 40];
                    let mut remaining_text = String::new();
                    push(
                        &mut remaining_text,
                        decimal_u64_into(remaining, &mut remaining_buf),
                    )?;
                    Some(sage_db_protocol::generated::BudgetDetail {
                        unit: match units {
                            Budgets::Bytes => sage_db_protocol::generated::BudgetUnit::Bytes as i32,
                            Budgets::Entries => {
                                sage_db_protocol::generated::BudgetUnit::Entries as i32
                            }
                        },
                        requested: requested_text,
                        remaining: remaining_text,
                    })
                }
            };
            Ok(Response {
                outcome: WriteOutcome {
                    outcome: Some(write_outcome::Outcome::Error(CommandError {
                        code: class.code as i32,
                        message,
                        budget,
                    })),
                },
                credits: {
                    let mut credits = Credits::default();
                    credits.push(credit)?;
                    credits
                },
            })
        }
    }
}

// ---------------------------------------------------------------------------
// GetRequestOutcome
// ---------------------------------------------------------------------------

/// What a `GetRequestOutcome` lookup produced.
#[derive(Debug)]
pub enum OutcomeResponse {
    /// The request is known on this basis, with its original recorded bodies.
    Found(OutcomeFound),
    /// Complete coverage of this basis, and no such request.
    Absent(OutcomeAbsent),
    /// Cannot answer definitively.
    Incomplete(OutcomeIncomplete),
}

impl OutcomeResponse {
    /// Project into the generated response oneof.
    #[must_use]
    pub fn into_response(self) -> sage_db_protocol::generated::GetRequestOutcomeResponse {
        use sage_db_protocol::generated::get_request_outcome_response::Result as WireResult;
        let result = match self {
            Self::Found(found) => WireResult::Found(found),
            Self::Absent(absent) => WireResult::Absent(absent),
            Self::Incomplete(incomplete) => WireResult::Incomplete(incomplete),
        };
        sage_db_protocol::generated::GetRequestOutcomeResponse {
            result: Some(result),
        }
    }
}

/// The credit behind a [`OutcomeResponse::Found`].
///
/// The other two variants own only short fixed text; this type makes the found path's owner
/// explicit so a caller can observe exactly what it retains.
pub struct FoundResponse {
    /// The populated message.
    response: OutcomeResponse,
    /// The admitted credit for every buffer inside it.
    credits: Credits,
}

impl FoundResponse {
    /// Borrow the populated message.
    #[must_use]
    pub const fn response(&self) -> &OutcomeResponse {
        &self.response
    }

    /// Bytes this response holds admitted for.
    #[must_use]
    pub fn admitted_bytes(&self) -> u64 {
        self.credits.total()
    }
}

impl core::fmt::Debug for FoundResponse {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FoundResponse")
            .field("admitted_bytes", &self.credits.total())
            .finish_non_exhaustive()
    }
}

/// Assemble the response for an outcome lookup already performed by the store.
///
/// Taking the [`OutcomeLookup`] rather than a principal/request pair keeps one authority for
/// lookup: the caller has already authenticated the request and taken the lookup through the
/// store's import-and-durability gate. This function does not re-authorize an anonymous scope.
///
/// # Errors
///
/// [`ResponseError`] when a `Found` answer cannot be stated truthfully — an admitted allocation
/// that failed, a blob/parse/version refusal, or a review status that cannot be established.
pub fn assemble_outcome<F: sage_db_store::fs::FileSystem>(
    store: &mut Store<F>,
    lookup: OutcomeLookup,
    family: WriteFamily,
    limits: &ResponseLimits,
    ledger: &mut dyn LeaseLedger,
) -> Result<FoundResponse, ResponseError> {
    match lookup {
        OutcomeLookup::Found {
            outcome,
            original_version,
            basis,
        } => {
            let event = outcome.event();
            let (recorded_dto, _proposal_key, recorded_credit) =
                build_recorded(store, event, original_version, limits, ledger)?;
            let mut credits = Credits::default();
            credits.push(recorded_credit)?;

            let status = match family {
                WriteFamily::Review => Some(original_status(
                    store,
                    event,
                    original_version,
                    limits,
                    ledger,
                )?),
                WriteFamily::Submit | WriteFamily::Commit => None,
            };

            // The found-specific strings, measured before their own admission.
            let mut extent = Extent::default();
            extent.slots::<WireViewBasis>(1)?;
            extent.text(decimal_len_u64(original_version))?;
            extent.text(64)?; // result_ref
            extent.text(64)?; // request_digest
            extent.text(32)?; // kb uuid
            extent.text(decimal_len_u64(basis.version.get()))?;
            extent.text(64)?; // manifest
            let text_credit = extent.admit(ledger)?;

            let mut version_buf = [0u8; 40];
            let mut original = String::new();
            push(
                &mut original,
                decimal_u64_into(original_version, &mut version_buf),
            )?;
            let mut ref_buf = [0u8; 64];
            let mut result_ref = String::new();
            push(&mut result_ref, hex32_into(&event.result_ref, &mut ref_buf))?;
            let mut digest_buf = [0u8; 64];
            let mut digest = String::new();
            push(
                &mut digest,
                hex32_into(&event.payload_digest, &mut digest_buf),
            )?;

            let mut kb_buf = [0u8; 32];
            let mut kb_text = String::new();
            push(&mut kb_text, uuid_into(&basis.kb_uuid, &mut kb_buf))?;
            let mut basis_version_buf = [0u8; 40];
            let mut basis_version = String::new();
            push(
                &mut basis_version,
                decimal_u64_into(basis.version.get(), &mut basis_version_buf),
            )?;
            let mut manifest_buf = [0u8; 64];
            let mut manifest = String::new();
            push(
                &mut manifest,
                hex32_into(&basis.manifest_sha256, &mut manifest_buf),
            )?;

            let found = OutcomeFound {
                original_version: original,
                result_ref,
                request_digest: digest,
                basis: Some(WireViewBasis {
                    kb_uuid: kb_text,
                    version: basis_version,
                    manifest_sha256: manifest,
                }),
                recorded: Some(recorded_dto),
                original_status: status.map(|s| wire_status(s) as i32),
            };
            credits.push(text_credit)?;
            Ok(FoundResponse {
                response: OutcomeResponse::Found(found),
                credits,
            })
        }
        OutcomeLookup::Absent { basis } => {
            // Absence states its basis; the three basis strings are owned text.
            let mut extent = Extent::default();
            extent.slots::<WireViewBasis>(1)?;
            extent.text(32)?;
            extent.text(decimal_len_u64(basis.version.get()))?;
            extent.text(64)?;
            let credit = extent.admit(ledger)?;

            let mut kb_buf = [0u8; 32];
            let mut kb_text = String::new();
            push(&mut kb_text, uuid_into(&basis.kb_uuid, &mut kb_buf))?;
            let mut version_buf = [0u8; 40];
            let mut version_text = String::new();
            push(
                &mut version_text,
                decimal_u64_into(basis.version.get(), &mut version_buf),
            )?;
            let mut manifest_buf = [0u8; 64];
            let mut manifest = String::new();
            push(
                &mut manifest,
                hex32_into(&basis.manifest_sha256, &mut manifest_buf),
            )?;
            Ok(FoundResponse {
                response: OutcomeResponse::Absent(OutcomeAbsent {
                    basis: Some(WireViewBasis {
                        kb_uuid: kb_text,
                        version: version_text,
                        manifest_sha256: manifest,
                    }),
                }),
                credits: {
                    let mut credits = Credits::default();
                    credits.push(credit)?;
                    credits
                },
            })
        }
        OutcomeLookup::Incomplete(reason) => {
            let text = incomplete_reason(reason);
            let mut extent = Extent::default();
            extent.text(text.len())?;
            let credit = extent.admit(ledger)?;
            let mut owned = String::new();
            push(&mut owned, text)?;
            Ok(FoundResponse {
                response: OutcomeResponse::Incomplete(OutcomeIncomplete { reason: owned }),
                credits: {
                    let mut credits = Credits::default();
                    credits.push(credit)?;
                    credits
                },
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Original review status
// ---------------------------------------------------------------------------

/// Recover the status the ORIGINAL review recorded.
///
/// The identity and revision come from the **recorded result**, so a later review of the same
/// proposal cannot substitute its own status.
fn original_status<F: sage_db_store::fs::FileSystem>(
    store: &mut Store<F>,
    event: &RequestOutcomeEvent,
    outcome_version: u64,
    limits: &ResponseLimits,
    ledger: &mut dyn LeaseLedger,
) -> Result<ProposalStatus, ResponseError> {
    // Read the recorded result to learn which proposal revision this outcome decided.
    let blob = store
        .read_committed_blob(event.result_ref, limits.max_result_bytes, ledger)
        .map_err(ResponseError::Blob)?;
    let parsed = typed::read_result_payload(blob.as_bytes(), limits.max_result_bytes, ledger)
        .map_err(ResponseError::Result)?;
    require_same_version(outcome_version, event.commit_version, parsed.version())?;

    let mut found: Option<(RecordKey, RecordRevision)> = None;
    for index in 0..parsed.affected_count() {
        let pair = parsed
            .affected_at(index, ledger)
            .map_err(ResponseError::Result)?
            .ok_or(ResponseError::ShortCanonicalRead {
                table: "affected",
                index,
            })?;
        if pair.key.kind() == RecordKind::Proposal {
            let revision =
                RecordRevision::new(pair.revision.get()).ok_or(ResponseError::InvalidRevision {
                    value: pair.revision.get(),
                })?;
            found = Some((pair.key, revision));
            break;
        }
    }
    let (key, revision) = found.ok_or(ResponseError::StatusUnknown)?;

    let identity = RecordIdentity::Identified { key };
    let budget = HistoryBudget::new(limits.max_history_events);
    match store.history(&identity, revision.get(), budget, &limits.limits, ledger) {
        HistoryOutcome::Found(hit) => {
            let sage_db_store::fold::FoldedRecord::Proposal(proposal) = hit.image.record() else {
                return Err(ResponseError::HistoryWrongFamily);
            };
            // A different revision would be the LATEST state, which must never be substituted.
            if proposal.revision != revision {
                return Err(ResponseError::HistoryWrongFamily);
            }
            Ok(proposal.status)
        }
        HistoryOutcome::Absent { .. } => Err(ResponseError::HistoryAbsent),
        HistoryOutcome::Incomplete { reason, .. } => Err(ResponseError::HistoryIncomplete(reason)),
        // A definitive failure is NOT absence: the bytes could not be read, which points an
        // operator at corruption or I/O, not at a missing revision.
        HistoryOutcome::Error(refusal) => Err(ResponseError::HistoryError(refusal)),
    }
}

/// The stable reason text for an incomplete lookup.
fn incomplete_reason(reason: StoreIncomplete) -> &'static str {
    match reason {
        StoreIncomplete::NoCurrentView => "no current view",
        StoreIncomplete::NotDurable => "basis not durable",
        StoreIncomplete::CoverageIncomplete => "coverage incomplete",
    }
}

// ---------------------------------------------------------------------------
// Refusal taxonomy
// ---------------------------------------------------------------------------

/// A refusal as the protocol can express it: a code, a fixed phrase, and an optional budget
/// detail that is only ever built from a **real** `BudgetError`.
struct RefusalText {
    code: ErrorCode,
    message: &'static str,
    budget: Option<(Budgets, u64, u64)>,
}

/// The units a `BudgetDetail` can report.
enum Budgets {
    Bytes,
    Entries,
}

impl RefusalText {
    const fn plain(code: ErrorCode, message: &'static str) -> Self {
        Self {
            code,
            message,
            budget: None,
        }
    }
}

/// Classify a store refusal exhaustively.
///
/// Every `HandleRefusal` variant is named: there is deliberately **no wildcard**, so a new
/// variant fails to compile here rather than silently becoming a generic answer.
///
/// Two rules shape the taxonomy:
///
/// * a transient, capacity or permission failure is **never** reported as corruption — the code
///   is the only machine-readable remedy signal on the wire, and `Corrupt` would send an operator
///   to restore a backup when the answer is "free space" or "retry";
/// * `Budget`/`Cancelled`/`Deadline` are claimed only when the canonical payload is actually in
///   hand. A bare `BudgetExceeded` with no `BudgetError` cannot report figures, so it does not
///   claim the budget code.
fn refusal_class(reason: &sage_db_store::handle::HandleRefusal) -> RefusalText {
    use sage_db_store::handle::HandleRefusal as R;
    match reason {
        R::Fenced => {
            RefusalText::plain(ErrorCode::Conflict, "writes are fenced pending resolution")
        }
        R::NotProductEligible => {
            RefusalText::plain(ErrorCode::Unsupported, "the store backend is not eligible")
        }
        R::Filesystem { error, .. } => filesystem_class(error),
        R::ExistingRootUnsupported => RefusalText::plain(
            ErrorCode::Unsupported,
            "the root has history this handle has not indexed",
        ),
        R::AlreadyCommitted { .. } => {
            RefusalText::plain(ErrorCode::Conflict, "the request is already committed")
        }
        R::IdempotencyMismatch => RefusalText::plain(
            ErrorCode::IdempotencyMismatch,
            "the request id is recorded with different content",
        ),
        // Coverage, not damage: absence cannot be concluded, which is a state that resolves.
        R::OutcomeIndexIncomplete => RefusalText::plain(
            ErrorCode::Conflict,
            "the outcome index does not cover committed history",
        ),
        R::WrongKb { .. } => RefusalText::plain(
            ErrorCode::PermissionDenied,
            "the request belongs to another knowledge base",
        ),
        R::ExpectVersionMismatch { .. } => RefusalText::plain(
            ErrorCode::Conflict,
            "the stated version is not the current one",
        ),
        R::VersionOverflow { .. } => {
            RefusalText::plain(ErrorCode::Conflict, "no further version exists")
        }
        R::VersionConflict { .. } => {
            RefusalText::plain(ErrorCode::Conflict, "the version does not follow")
        }
        R::Derivation(refusal) => derivation_class(refusal),
        // An extent that cannot be represented is arithmetic, not allocation: no allocator ran.
        R::ExtentOverflow => RefusalText::plain(
            ErrorCode::InvalidArgument,
            "an extent cannot be represented on this platform",
        ),
        // A layout that cannot be expressed is also arithmetic, not allocation.
        R::PinLayout => RefusalText::plain(
            ErrorCode::InvalidArgument,
            "a retained layout cannot be represented",
        ),
        // A transfer that did not find admission is an accounting inconsistency: no allocator
        // was consulted, and no allowance would have produced the credit.
        R::PinTransfer(_) => {
            RefusalText::plain(ErrorCode::Unspecified, "retained credit was not admitted")
        }
        // A historical read failure keeps its own class rather than collapsing to a generic one.
        R::History(refusal) => open_class(refusal),
    }
}

/// Classify a derivation refusal by its own retained verdict.
fn derivation_class(refusal: &sage_db_store::commit::CommitRefusal) -> RefusalText {
    use sage_db_store::commit::CommitRefusal as C;
    match refusal {
        C::Filesystem { error, .. } => filesystem_class(error),
        // A budget or stop inside derivation keeps its own class, with figures where present.
        C::Budget => RefusalText::plain(ErrorCode::Budget, "the request's budget refused it"),
        C::Stopped => RefusalText::plain(ErrorCode::Cancelled, "the caller cancelled the request"),
        C::Refused(refusal) => ledger_class(refusal),
        // A format verdict carries its own four-way taxonomy.
        C::Format(error) => format_class(error),
        // A transfer that found no admitted credit: accounting, not allocation.
        C::Transfer(_) => {
            RefusalText::plain(ErrorCode::Unspecified, "retained credit was not admitted")
        }
        // An extent that cannot be represented: arithmetic, not allocation.
        C::ExtentOverflow => RefusalText::plain(
            ErrorCode::InvalidArgument,
            "an extent cannot be represented on this platform",
        ),
        // A missing outcome record inside derivation is a local inconsistency.
        C::MissingOutcome => RefusalText::plain(
            ErrorCode::Unspecified,
            "the store could not derive the transaction's outcome",
        ),
        // Structural mismatches between what was declared and what was derived: the request or
        // its declared blobs disagree with what the store produced. Not damage to durable bytes.
        C::Encode
        | C::Fold(_)
        | C::Outcome(_)
        | C::CopyNotCanonical
        | C::UnresolvedFoldCopy
        | C::BlobMismatch { .. }
        | C::DeclaredLengthTooLarge { .. }
        | C::UndeclaredRef { .. } => {
            RefusalText::plain(ErrorCode::InvalidArgument, "the request was refused")
        }
    }
}

/// Classify a seam failure by its actual kind, preserving any payload it carries.
///
/// `FsError` keeps the real `BudgetError` and `ControlStop` when it has them, so a budget or
/// cancellation is reported with its actual figures and its exact stop rather than as damage.
fn filesystem_class(error: &sage_db_store::fs::FsError) -> RefusalText {
    use sage_db_store::fs::FsErrorKind as K;

    // A real budget refusal: report the code AND the figures, because both are in hand.
    if let Some(budget) = error.budget() {
        let units = match budget.unit() {
            sage_db_types::ledger::BudgetUnit::Bytes => Budgets::Bytes,
            sage_db_types::ledger::BudgetUnit::Entries => Budgets::Entries,
        };
        return RefusalText {
            code: ErrorCode::Budget,
            message: "the request's budget refused the operation",
            budget: Some((units, budget.requested(), budget.remaining())),
        };
    }
    // A real stop: cancelled and deadline are different remedies, so they keep their own codes.
    if let Some(stop) = error.control() {
        return match stop {
            sage_db_types::query::ControlStop::Cancelled => {
                RefusalText::plain(ErrorCode::Cancelled, "the caller cancelled the request")
            }
            sage_db_types::query::ControlStop::DeadlineExceeded => {
                RefusalText::plain(ErrorCode::Deadline, "the caller's deadline passed")
            }
        };
    }

    match error.kind() {
        // Capacity: nothing is damaged, and the protocol has no "unavailable" member, so this is
        // an honest unspecified class with a truthful phrase rather than a false corruption.
        K::NoSpace => RefusalText::plain(ErrorCode::Unspecified, "the device has no space left"),
        K::QuotaExceeded => RefusalText::plain(ErrorCode::Unspecified, "a quota would be exceeded"),
        // Retryable: the lock is transient, interruption is transient.
        K::WouldBlock => {
            RefusalText::plain(ErrorCode::Conflict, "an exclusive lock is held elsewhere")
        }
        K::Interrupted => RefusalText::plain(
            ErrorCode::Conflict,
            "the operation was interrupted before completing",
        ),
        K::PermissionDenied => RefusalText::plain(
            ErrorCode::PermissionDenied,
            "the caller may not perform this operation",
        ),
        K::Unsupported => RefusalText::plain(
            ErrorCode::Unsupported,
            "the feature is unavailable on this filesystem",
        ),
        // Structural inconsistency: these genuinely mean the layout does not match the contract.
        K::HashMismatch => RefusalText::plain(
            ErrorCode::Corrupt,
            "content did not match its declared digest or length",
        ),
        K::Symlink => RefusalText::plain(
            ErrorCode::Corrupt,
            "an entry is a symbolic link where the layout requires a real one",
        ),
        K::NotRegular => RefusalText::plain(
            ErrorCode::Corrupt,
            "an entry is not the regular file the layout requires",
        ),
        K::NotADirectory => RefusalText::plain(
            ErrorCode::Corrupt,
            "an entry is not the directory the layout requires",
        ),
        K::IsADirectory => RefusalText::plain(
            ErrorCode::Corrupt,
            "an entry is a directory where a file is required",
        ),
        K::CrossDevice => RefusalText::plain(
            ErrorCode::Corrupt,
            "a family lives on a different device from the root",
        ),
        // Absence is not damage: the layout names an entry that is not there.
        K::NotFound => RefusalText::plain(ErrorCode::Conflict, "an expected entry is absent"),
        K::AlreadyExists => RefusalText::plain(ErrorCode::Conflict, "the entry already exists"),
        K::TooLarge => RefusalText::plain(
            ErrorCode::InvalidArgument,
            "the operation would exceed its limit",
        ),
        // A bare budget kind with no payload: the refusal is real but no figures exist, so the
        // budget code and a `BudgetDetail` are NOT claimed.
        K::BudgetExceeded => {
            RefusalText::plain(ErrorCode::Conflict, "the operation's budget refused it")
        }
        // Any other platform errno: errno alone does not establish corruption.
        K::Io(_) => RefusalText::plain(
            ErrorCode::Unspecified,
            "the operation failed with an I/O error",
        ),
    }
}

/// Classify a historical/open refusal by its own retained verdict.
fn open_class(refusal: &sage_db_store::open::OpenRefusal) -> RefusalText {
    use sage_db_store::open::OpenRefusal as O;
    match refusal {
        // Real structural or verification failures.
        O::Corrupt(_) => RefusalText::plain(ErrorCode::Corrupt, "verification failed"),
        // A format verdict is its own four-class taxonomy; only its corrupt class is damage.
        O::Format(error) => format_class(error),
        O::Filesystem { error, .. } => filesystem_class(error),
        O::Refused(refusal) => ledger_class(refusal),
        O::IdentityMismatch { .. } => RefusalText::plain(
            ErrorCode::PermissionDenied,
            "the root's identity does not match the expected one",
        ),
        O::CrossDevice { .. } => RefusalText::plain(
            ErrorCode::Corrupt,
            "a family lives on a different device from the root",
        ),
        O::ReadLengthMismatch { .. } => RefusalText::plain(
            ErrorCode::Corrupt,
            "an immutable file violated its exact-read contract",
        ),
        // Local defects: the library broke its own invariant. Not a data verdict about bytes.
        O::CanonicalMapperInvariant
        | O::InvalidObservation
        | O::HistoryFrameMismatch
        | O::PublicationBasisMismatch
        | O::BufferLayout
        | O::MissingRulesState
        | O::PinLayout
        | O::HistoryLayout
        | O::ExtentOverflow
        | O::Assembly(_)
        | O::RecordSet(_)
        | O::Fold(_)
        | O::Derivation(_) => RefusalText::plain(
            ErrorCode::Unspecified,
            "the store could not complete the operation",
        ),
        // Neither of these consulted an allocator; both are accounting/eligibility states.
        O::PinTransfer(_) | O::HistoryTransfer(_) => {
            RefusalText::plain(ErrorCode::Unspecified, "retained credit was not admitted")
        }
        // Not damage and not a caller error: the root is simply not initialised, or the backend
        // cannot honour durability.
        O::NotInitialized | O::MissingCurrentOnNonEmpty | O::Unsupported(_) | O::NotEligible(_) => {
            RefusalText::plain(
                ErrorCode::Unsupported,
                "the root is not in a state this build can serve",
            )
        }
    }
}

/// Classify a canonical format verdict.
fn format_class(error: &sage_db_types::framing::FormatError) -> RefusalText {
    use sage_db_types::framing::FormatError as F;
    match error {
        F::Corrupt(_) => RefusalText::plain(ErrorCode::Corrupt, "the bytes are not a legal file"),
        F::UnsupportedFormat(_) => {
            RefusalText::plain(ErrorCode::Unsupported, "the format is not supported")
        }
        F::Limit(_) => RefusalText::plain(
            ErrorCode::InvalidArgument,
            "a declared ceiling would be exceeded",
        ),
        F::Internal(_) => RefusalText::plain(
            ErrorCode::Unspecified,
            "the library broke its own format invariant",
        ),
    }
}

/// Classify a canonical ledger refusal.
fn ledger_class(refusal: &Refusal) -> RefusalText {
    match refusal {
        Refusal::Budget(budget) => {
            let units = match budget.unit() {
                sage_db_types::ledger::BudgetUnit::Bytes => Budgets::Bytes,
                sage_db_types::ledger::BudgetUnit::Entries => Budgets::Entries,
            };
            RefusalText {
                code: ErrorCode::Budget,
                message: "the request's budget refused the operation",
                budget: Some((units, budget.requested(), budget.remaining())),
            }
        }
        Refusal::Stopped(stop) => match stop {
            sage_db_types::query::ControlStop::Cancelled => {
                RefusalText::plain(ErrorCode::Cancelled, "the caller cancelled the request")
            }
            sage_db_types::query::ControlStop::DeadlineExceeded => {
                RefusalText::plain(ErrorCode::Deadline, "the caller's deadline passed")
            }
        },
        // No allocator was consulted; the extent was simply not representable.
        Refusal::Allocation { .. } => RefusalText::plain(
            ErrorCode::InvalidArgument,
            "an extent cannot be represented on this platform",
        ),
    }
}

fn wire_status(status: ProposalStatus) -> WireProposalStatus {
    match status {
        ProposalStatus::Pending => WireProposalStatus::Pending,
        ProposalStatus::Approved => WireProposalStatus::Approved,
        ProposalStatus::Rejected => WireProposalStatus::Rejected,
        ProposalStatus::Conflicted => WireProposalStatus::Conflicted,
    }
}

/// The outcome event inside a recorded outcome, or a stated absence.
fn event_of(recorded: &RecordedOutcome) -> Result<&RequestOutcomeEvent, ResponseError> {
    match recorded {
        RecordedOutcome::Outcome(shared) => Ok(shared.event()),
        RecordedOutcome::GenesisNone => Err(ResponseError::NoRecordedOutcome),
    }
}
