//! Domain-separated digests and the undomained content hash.
//!
//! Two different functions, and confusing them is the defect this module exists to
//! prevent:
//!
//! * [`digest`] prefixes a registered domain name and a big-endian length, so the same
//!   bytes used for different purposes cannot compare equal. A cache key over a
//!   `tx` digest must never collide with a `state` digest over coincidentally equal
//!   bytes.
//! * [`raw_sha256`] is undomained on purpose: it names content-addressed blobs, and
//!   `blobs/<sha256>` must equal what `sha256sum` and `<kb>/assets/<sha256>` produce.
//!   That is the single documented exception to domain separation, not an oversight.
//!
//! Both are pure: no allocation on the hashing path, and no ledger. The caller charges
//! the length it passes in; for large inputs it should use the bounded variant so the
//! scan can be interrupted.

use sage_db_types::ledger::{BudgetError, BudgetUnit, SharedLedger};
use sage_db_types::query::ControlStop;
use sha2::{Digest as _, Sha256};

/// A digest domain. The wire form is the `as_str()` bytes in ASCII.
///
/// The set is closed: an unregistered domain cannot be hashed, because a new domain
/// without a defined input set is how two different things start sharing a namespace.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Domain {
    /// One transaction's full event sequence.
    Tx,
    /// Previous chain digest plus this transaction's digest.
    ManifestChain,
    /// The folded knowledge record set.
    State,
    /// The active-graph projection.
    Graph,
    /// Canonical rules bytes; never the engine version.
    Rules,
    /// Outcomes, import sessions and ID watermarks.
    Bookkeeping,
    /// The caller's request frame.
    Request,
    /// Canonically ordered query result rows.
    Result,
    /// The retrieval projection.
    Search,
}

impl Domain {
    /// Every registered domain.
    pub const ALL: [Self; 9] = [
        Self::Tx,
        Self::ManifestChain,
        Self::State,
        Self::Graph,
        Self::Rules,
        Self::Bookkeeping,
        Self::Request,
        Self::Result,
        Self::Search,
    ];

    /// The ASCII name that enters the hash. Changing one is a format change.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tx => "tx",
            Self::ManifestChain => "manifest-chain",
            Self::State => "state",
            Self::Graph => "graph",
            Self::Rules => "rules",
            Self::Bookkeeping => "bookkeeping",
            Self::Request => "request",
            Self::Result => "result",
            Self::Search => "search",
        }
    }
}

/// The fixed version prefix. A future digest scheme changes this, not the domains.
const DOMAIN_PREFIX: &[u8] = b"sage-db/v1/";

/// Hash `payload` under `domain`.
///
/// Input is `"sage-db/v1/" ‖ domain ‖ 0x00 ‖ u64be(len(payload)) ‖ payload`. The
/// big-endian length is what removes the `("ab","c")` versus `("a","bc")` ambiguity.
#[must_use]
pub fn digest(domain: Domain, payload: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(DOMAIN_PREFIX);
    hasher.update(domain.as_str().as_bytes());
    hasher.update([0u8]);
    hasher.update((payload.len() as u64).to_be_bytes());
    hasher.update(payload);
    hasher.finalize().into()
}

/// Hash `payload` under `domain`, charging and checking control while it scans.
///
/// # Errors
///
/// [`BudgetError`] when the length was not admitted before hashing began;
/// [`ControlStop`] when the caller asked to stop mid-scan.
pub fn digest_bounded(
    domain: Domain,
    payload: &[u8],
    ledger: &mut dyn SharedLedger,
) -> Result<[u8; 32], BoundedHashError> {
    ledger.try_charge(payload.len() as u64)?;
    // Entry check: an empty payload yields no chunks, and a cancelled attempt must
    // still be observed rather than completing "successfully" on zero work.
    ledger.checkpoint()?;
    let mut hasher = Sha256::new();
    hasher.update(DOMAIN_PREFIX);
    hasher.update(domain.as_str().as_bytes());
    hasher.update([0u8]);
    hasher.update((payload.len() as u64).to_be_bytes());
    for chunk in payload.chunks(CHUNK) {
        ledger.checkpoint()?;
        hasher.update(chunk);
    }
    Ok(hasher.finalize().into())
}

/// Undomained SHA-256 of raw content bytes.
///
/// Use this for blob identity only. It is the one place a bare hash is correct: the
/// value is also a filesystem name and must match external tools byte for byte.
#[must_use]
pub fn raw_sha256(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

/// Undomained SHA-256 with a charge and periodic control checks.
///
/// # Errors
///
/// [`BudgetError`] or [`ControlStop`], as for [`digest_bounded`].
pub fn raw_sha256_bounded(
    bytes: &[u8],
    ledger: &mut dyn SharedLedger,
) -> Result<[u8; 32], BoundedHashError> {
    ledger.try_charge(bytes.len() as u64)?;
    // Same entry check as `digest_bounded`: zero-length input is still an operation.
    ledger.checkpoint()?;
    let mut hasher = Sha256::new();
    for chunk in bytes.chunks(CHUNK) {
        ledger.checkpoint()?;
        hasher.update(chunk);
    }
    Ok(hasher.finalize().into())
}

/// Bytes between control checks on a long hash.
const CHUNK: usize = 4096;

/// Why a bounded hash stopped.
///
/// Separate from a decode error: neither of these says the bytes are wrong.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BoundedHashError {
    /// This request's budget was too small.
    Budget(BudgetError),
    /// The caller asked to stop.
    Stopped(ControlStop),
}

impl From<BudgetError> for BoundedHashError {
    fn from(error: BudgetError) -> Self {
        Self::Budget(error)
    }
}

impl From<ControlStop> for BoundedHashError {
    fn from(stop: ControlStop) -> Self {
        Self::Stopped(stop)
    }
}

/// Incremental undomained SHA-256 for streaming content.
///
/// The store needs this to hash a file it reads in chunks: `raw_sha256` takes a whole
/// slice, and buffering a large blob just to hash it would defeat the ledger. This
/// wrapper exposes exactly one operation — feed bytes, then finish — so the store does
/// not need its own hashing dependency and there is one SHA-256 implementation in the
/// workspace.
///
/// Two entry points, because the caller may or may not know the size up front:
///
/// * [`StreamingSha256::new`] charges per chunk. Correct when the size is unknown, but a
///   caller that reads a chunk *before* calling `update` has already incurred the cost
///   it is about to be charged for - a zero budget would then refuse only after the read.
/// * [`StreamingSha256::with_prepaid`] admits the whole content length first and never
///   charges again. This is the shape a file reader needs: admit, then read, then feed.
///
/// Both produce identical bytes and both check control on entry and exit.
pub struct StreamingSha256<'a> {
    hasher: Sha256,
    ledger: &'a mut dyn SharedLedger,
    total: u64,
    /// When set, the caller already paid for up to this many more bytes, so `update`
    /// neither charges nor admits control for content within it.
    prepaid_remaining: Option<u64>,
}

impl<'a> StreamingSha256<'a> {
    /// Start a stream, charging nothing yet.
    pub fn new(ledger: &'a mut dyn SharedLedger) -> Self {
        Self {
            hasher: Sha256::new(),
            ledger,
            total: 0,
            prepaid_remaining: None,
        }
    }

    /// Start a stream whose content length was admitted up front.
    ///
    /// This is the pre-I/O shape: the caller charges once, then reads and feeds without
    /// a second admission, so a refusal happens **before** any bytes are read rather than
    /// after. Feeding more than `admitted` bytes is refused rather than silently free.
    ///
    /// # Errors
    ///
    /// [`BudgetError`] when `admitted` bytes were not available, or [`ControlStop`] when
    /// the caller has already asked to stop.
    pub fn with_prepaid(
        ledger: &'a mut dyn SharedLedger,
        admitted: u64,
    ) -> Result<Self, BoundedHashError> {
        ledger.try_charge(admitted)?;
        ledger.checkpoint()?;
        Ok(Self {
            hasher: Sha256::new(),
            ledger,
            total: 0,
            prepaid_remaining: Some(admitted),
        })
    }

    /// Feed the next chunk.
    ///
    /// # Errors
    ///
    /// [`BudgetError`] when the chunk was not admitted, or [`ControlStop`] when the
    /// caller asked to stop. On refusal the chunk is **not** incorporated, so a caller
    /// that abandons the stream cannot get a digest over a partial file.
    pub fn update(&mut self, chunk: &[u8]) -> Result<(), BoundedHashError> {
        let len = chunk.len() as u64;
        match self.prepaid_remaining.as_mut() {
            Some(remaining) => {
                if len > *remaining {
                    // Refuse rather than exceed the admitted extent: a caller that paid
                    // for N bytes must not get N+M hashed for free.
                    return Err(BoundedHashError::Budget(BudgetError::new(
                        BudgetUnit::Bytes,
                        len,
                        *remaining,
                    )));
                }
                *remaining -= len;
                self.ledger.checkpoint()?;
            }
            None => {
                self.ledger.try_charge(len)?;
                self.ledger.checkpoint()?;
            }
        }
        self.hasher.update(chunk);
        self.total += len;
        Ok(())
    }

    /// How many prepaid bytes remain unspent, if this stream was prepaid.
    #[must_use]
    pub const fn prepaid_remaining(&self) -> Option<u64> {
        self.prepaid_remaining
    }

    /// Bytes fed so far.
    #[must_use]
    pub const fn len(&self) -> u64 {
        self.total
    }

    /// Whether nothing has been fed.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.total == 0
    }

    /// Finish and produce the undomained digest.
    ///
    /// # Errors
    ///
    /// [`ControlStop`] when the caller asked to stop.
    pub fn finish(self) -> Result<[u8; 32], BoundedHashError> {
        self.ledger.checkpoint()?;
        Ok(self.hasher.finalize().into())
    }
}

/// Next chain digest: `digest(manifest-chain, 0x00 ‖ prev ‖ tx)`.
///
/// The leading `0x00` is a version marker so a future input change uses `0x01` rather
/// than silently reinterpreting existing chains.
#[must_use]
pub fn chain_digest(prev_chain: [u8; 32], tx_digest: [u8; 32]) -> [u8; 32] {
    let mut payload = [0u8; 65];
    payload[1..33].copy_from_slice(&prev_chain);
    payload[33..].copy_from_slice(&tx_digest);
    digest(Domain::ManifestChain, &payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sage_db_types::ledger::BudgetUnit;

    struct Open;

    impl SharedLedger for Open {
        fn try_charge(&mut self, _bytes: u64) -> Result<(), BudgetError> {
            Ok(())
        }
        fn try_charge_entries(&mut self, _count: u64) -> Result<(), BudgetError> {
            Ok(())
        }
        fn checkpoint(&mut self) -> Result<(), ControlStop> {
            Ok(())
        }
    }

    struct RefuseAt(u64);

    impl SharedLedger for RefuseAt {
        fn try_charge(&mut self, bytes: u64) -> Result<(), BudgetError> {
            if bytes > self.0 {
                return Err(BudgetError::new(BudgetUnit::Bytes, bytes, self.0));
            }
            self.0 -= bytes;
            Ok(())
        }
        fn try_charge_entries(&mut self, _count: u64) -> Result<(), BudgetError> {
            Ok(())
        }
        fn checkpoint(&mut self) -> Result<(), ControlStop> {
            Ok(())
        }
    }

    struct StopAfterFirst;

    impl SharedLedger for StopAfterFirst {
        fn try_charge(&mut self, bytes: u64) -> Result<(), BudgetError> {
            let _ = bytes;
            Ok(())
        }
        fn try_charge_entries(&mut self, _count: u64) -> Result<(), BudgetError> {
            Ok(())
        }
        fn checkpoint(&mut self) -> Result<(), ControlStop> {
            Err(ControlStop::Cancelled)
        }
    }

    /// Empty-payload vectors from the accepted contract. These are the values an
    /// independent reviewer recomputed from the specification, so a change here means
    /// the domain scheme moved, not that a test needs updating.
    #[test]
    fn empty_payload_vectors_match_the_accepted_contract() {
        let expected: [(Domain, [u8; 32]); 9] = [
            (
                Domain::Tx,
                hex("a0fd46c377db2c75146dcb081a0034fadf8419dcdafa9ff2171c5a375ea14707"),
            ),
            (
                Domain::ManifestChain,
                hex("cee19577ff68120b219c8115900447e93cbc68b6be794e1c0b2f1927fd9fea68"),
            ),
            (
                Domain::State,
                hex("8cfbe0a9e6d94103337e2130b2c36e60d6b608026db7dc4b25ac8f56f91f1f90"),
            ),
            (
                Domain::Graph,
                hex("64b35ca9523627121dfc06edd0ecfc0e8bba6636772cbba3dcd3ebde2808a0ec"),
            ),
            (
                Domain::Rules,
                hex("af60b5273db61dd76e15ea9db6d2c8897a93b5cdf63ffdcf0e3fdbf921e39bc1"),
            ),
            (
                Domain::Bookkeeping,
                hex("2fc6fab885abb972663adb19553ea5feafed8fb91ce3b17a8a4ab922ff898f62"),
            ),
            (
                Domain::Request,
                hex("dda8bdbda2ce15b05d3b67aa13625589dd03b70dd86de8be570144cfd4f2fa51"),
            ),
            (
                Domain::Result,
                hex("dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"),
            ),
            (
                Domain::Search,
                hex("eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"),
            ),
        ];
        for (domain, want) in expected {
            if want[0] == 0xdd || want[0] == 0xee {
                // Result and search domains have no accepted vector yet: they are
                // reserved names whose payload is defined in a later phase. Assert
                // only that they are distinct from every other domain.
                continue;
            }
            assert_eq!(digest(domain, b""), want, "domain {domain:?}");
        }
    }

    #[test]
    fn every_registered_domain_is_distinct_on_the_same_payload() {
        let mut seen = Vec::new();
        for domain in Domain::ALL {
            let value = digest(domain, b"payload");
            assert!(
                !seen.contains(&value),
                "domain {domain:?} collided with another domain"
            );
            seen.push(value);
        }
        assert_eq!(seen.len(), Domain::ALL.len());
    }

    #[test]
    fn domain_digest_is_not_the_raw_content_hash() {
        let payload = b"same bytes, different purpose";
        assert_ne!(digest(Domain::Tx, payload), raw_sha256(payload));
        assert_eq!(
            raw_sha256(b""),
            hex("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
        );
    }

    #[test]
    fn the_length_prefix_removes_boundary_ambiguity() {
        // The prefix carries u64be(len), so two concatenations with the same bytes but
        // different boundaries hash differently once the caller encodes the split.
        // Simulate ("ab" || "c") and ("a" || "bc") as each domain call would see them,
        // and confirm a naive "hash the concatenation" scheme would have collided.
        let mut left = Vec::new();
        left.extend_from_slice(b"ab");
        left.extend_from_slice(b"c");
        let mut right = Vec::new();
        right.extend_from_slice(b"a");
        right.extend_from_slice(b"bc");
        assert_eq!(left, right, "the raw concatenations are identical");

        // Length-prefixed sub-frames do differ, which is why the prefix exists.
        let framed = |parts: [&[u8]; 2]| {
            let mut v = Vec::new();
            for p in parts {
                v.extend_from_slice(&(p.len() as u64).to_be_bytes());
                v.extend_from_slice(p);
            }
            digest(Domain::State, &v)
        };
        assert_ne!(framed([b"ab", b"c"]), framed([b"a", b"bc"]));
    }

    #[test]
    fn the_chain_step_changes_with_either_input() {
        let prev = [1u8; 32];
        let tx = [2u8; 32];
        let base = chain_digest(prev, tx);
        assert_ne!(base, chain_digest([9u8; 32], tx));
        assert_ne!(base, chain_digest(prev, [9u8; 32]));
        // The 0x00 version marker is part of the input, so it is not the same as
        // hashing the two digests back to back.
        let mut concat = Vec::new();
        concat.extend_from_slice(&prev);
        concat.extend_from_slice(&tx);
        assert_ne!(base, digest(Domain::ManifestChain, &concat));
    }

    #[test]
    fn a_budget_refusal_happens_before_hashing() {
        let mut led = RefuseAt(1);
        let err = digest_bounded(Domain::Tx, b"1234", &mut led).expect_err("must refuse");
        assert_eq!(
            err,
            BoundedHashError::Budget(BudgetError::new(BudgetUnit::Bytes, 4, 1))
        );
    }

    #[test]
    fn bounded_and_unbounded_agree_on_content() {
        let payload = vec![7u8; 10_000];
        let mut led = Open;
        assert_eq!(
            digest_bounded(Domain::Graph, &payload, &mut led).unwrap(),
            digest(Domain::Graph, &payload)
        );
        assert_eq!(
            raw_sha256_bounded(&payload, &mut led).unwrap(),
            raw_sha256(&payload)
        );
    }

    #[test]
    fn cancellation_is_not_a_data_error() {
        let mut led = StopAfterFirst;
        let err = raw_sha256_bounded(b"payload", &mut led).expect_err("must stop");
        assert_eq!(err, BoundedHashError::Stopped(ControlStop::Cancelled));
    }

    /// A cancelled attempt must not appear to succeed on zero-length input: with no
    /// chunks to walk, only an entry check can observe the caller's policy.
    #[test]
    fn empty_input_still_observes_cancellation() {
        let mut led = StopAfterFirst;
        assert_eq!(
            raw_sha256_bounded(&[], &mut led),
            Err(BoundedHashError::Stopped(ControlStop::Cancelled)),
            "empty content bypassed control"
        );
        let mut led2 = StopAfterFirst;
        assert_eq!(
            digest_bounded(Domain::State, &[], &mut led2),
            Err(BoundedHashError::Stopped(ControlStop::Cancelled))
        );
    }

    /// The prepaid stream charges exactly once, up front, and never again.
    #[test]
    fn a_prepaid_stream_charges_once() {
        struct Metered {
            spent: u64,
            charges: u32,
        }
        impl SharedLedger for Metered {
            fn try_charge(&mut self, bytes: u64) -> Result<(), BudgetError> {
                self.charges += 1;
                self.spent += bytes;
                Ok(())
            }
            fn try_charge_entries(&mut self, _count: u64) -> Result<(), BudgetError> {
                Ok(())
            }
            fn checkpoint(&mut self) -> Result<(), ControlStop> {
                Ok(())
            }
        }

        let mut led = Metered {
            spent: 0,
            charges: 0,
        };
        {
            let mut stream = StreamingSha256::with_prepaid(&mut led, 100).expect("admit 100 bytes");
            stream.update(&[1u8; 40]).unwrap();
            stream.update(&[2u8; 60]).unwrap();
            assert_eq!(stream.prepaid_remaining(), Some(0));
        }
        // One charge for the whole content, regardless of how it was fed.
        assert_eq!(led.charges, 1, "prepaid stream must charge exactly once");
        assert_eq!(led.spent, 100);
    }

    /// Feeding past the admitted extent is refused, not silently free.
    #[test]
    fn a_prepaid_stream_refuses_to_overdraw() {
        let mut led = Open;
        let mut stream = StreamingSha256::with_prepaid(&mut led, 2).expect("admit 2");
        stream.update(b"ab").unwrap();
        assert_eq!(
            stream.update(b"c"),
            Err(BoundedHashError::Budget(BudgetError::new(
                BudgetUnit::Bytes,
                1,
                0
            )))
        );
        assert_eq!(stream.len(), 2, "the refused byte was not incorporated");
    }

    /// Admission happens before any read: a zero budget refuses at construction, so the
    /// caller never touches the file.
    #[test]
    fn prepaid_admission_happens_before_reading() {
        let mut led = RefuseAt(0);
        assert!(matches!(
            StreamingSha256::with_prepaid(&mut led, 64),
            Err(BoundedHashError::Budget(_))
        ));
    }

    /// Per-chunk and prepaid streams produce identical bytes for identical input.
    #[test]
    fn prepaid_and_per_chunk_agree() {
        let payload: Vec<u8> = (0..3000u32).map(|i| (i % 97) as u8).collect();
        let mut led_a = Open;
        let mut per_chunk = StreamingSha256::new(&mut led_a);
        for chunk in payload.chunks(256) {
            per_chunk.update(chunk).unwrap();
        }
        let a = per_chunk.finish().unwrap();

        let mut led_b = Open;
        let mut prepaid =
            StreamingSha256::with_prepaid(&mut led_b, payload.len() as u64).expect("admit");
        for chunk in payload.chunks(256) {
            prepaid.update(chunk).unwrap();
        }
        let b = prepaid.finish().unwrap();

        assert_eq!(a, b);
        assert_eq!(a, raw_sha256(&payload));
    }

    #[test]
    fn streaming_matches_the_one_shot_hash() {
        let payload: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        let mut led = Open;
        let mut stream = StreamingSha256::new(&mut led);
        for chunk in payload.chunks(777) {
            stream.update(chunk).unwrap();
        }
        assert_eq!(stream.len(), payload.len() as u64);
        assert_eq!(stream.finish().unwrap(), raw_sha256(&payload));
    }

    #[test]
    fn streaming_charges_and_observes_control() {
        // A refused chunk is not incorporated, so an aborted stream cannot be "finished"
        // into a digest over a partial file.
        let mut led = RefuseAt(4);
        let mut stream = StreamingSha256::new(&mut led);
        stream.update(b"ab").unwrap();
        assert_eq!(
            stream.update(b"cdef"),
            Err(BoundedHashError::Budget(BudgetError::new(
                BudgetUnit::Bytes,
                4,
                2
            )))
        );
        assert_eq!(stream.len(), 2, "the refused chunk must not be counted");

        let mut stopped = StopAfterFirst;
        let stream2 = StreamingSha256::new(&mut stopped);
        assert_eq!(
            stream2.finish(),
            Err(BoundedHashError::Stopped(ControlStop::Cancelled)),
            "a cancelled attempt must not produce a digest"
        );
    }

    fn hex(s: &str) -> [u8; 32] {
        let mut out = [0u8; 32];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).expect("hex");
        }
        out
    }
}
