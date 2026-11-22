//! Bounded selection of an original file from the committed v1 import package.

use sage_db_store::fs::FileSystem;
use sage_db_store::handle::Store;
use sage_db_store::lease::AttemptLedger;
use sage_db_store::view::{CommittedBlob, ViewBasis};
use sage_db_types::framing::Refusal;
use sage_db_types::identity::RecordRevision;
use sage_db_types::ledger::SharedLedger;
use sage_db_types::persist::TermRecord;
use serde::de::{self, DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::admission::{Admission, AdmissionContext, AdmissionError};

use crate::http::Failure;
use crate::operator_read::{Decimal, Digest};

const MAX_PACKAGE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = 64 * 1024;

pub(crate) fn package<F: FileSystem>(
    store: &mut Store<F>,
    term: &TermRecord,
    basis: ViewBasis,
    ledger: &mut AttemptLedger<'_>,
) -> Result<Option<CommittedBlob>, Failure> {
    if term.revision != RecordRevision::FIRST
        || term.legacy_source_path.is_none()
        || term.legacy_source_file_digest.is_none()
    {
        return Ok(None);
    }
    import_package(store, basis, ledger)
}

pub(crate) fn import_package(
    store: &mut Store<impl FileSystem>,
    basis: ViewBasis,
    ledger: &mut AttemptLedger<'_>,
) -> Result<Option<CommittedBlob>, Failure> {
    let Some((digest, import_basis)) = store
        .import_package_ref(ledger)
        .map_err(|error| crate::runtime::kernel_error(error.into()))?
    else {
        return Ok(None);
    };
    if import_basis != basis {
        return Err(Failure::internal());
    }
    let blob = store
        .read_committed_blob(digest, MAX_PACKAGE_BYTES, ledger)
        .map_err(crate::runtime::blob_error)?;
    if blob.basis() != basis {
        return Err(Failure::internal());
    }
    Ok(Some(blob))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Original<'a> {
    bytes_base64: &'a str,
    length: Decimal<u64>,
    sha256: Digest,
}

pub(crate) fn extract<'a>(
    package: &'a CommittedBlob,
    term: &TermRecord,
    ledger: &mut dyn SharedLedger,
) -> Result<Original<'a>, Failure> {
    let path = term
        .legacy_source_path
        .as_deref()
        .ok_or_else(Failure::internal)?;
    let digest = term
        .legacy_source_file_digest
        .ok_or_else(Failure::internal)?;
    let input = package.as_bytes();
    let [selection] = select::<1>(input, Selector::Path(path), ledger)?;
    let selected = selection.ok_or_else(Failure::internal)?;
    if selected.digest != digest {
        return Err(Failure::internal());
    }
    selected.original(input)
}

#[derive(Serialize)]
pub(crate) struct Example<'a> {
    path: &'a str,
    #[serde(flatten)]
    original: Original<'a>,
}

pub(crate) fn examples<'a>(
    package: &'a CommittedBlob,
    ledger: &mut dyn SharedLedger,
) -> Result<Vec<Example<'a>>, Failure> {
    let input = package.as_bytes();
    let entries = select::<32>(input, Selector::Examples, ledger)?;
    let mut result = crate::activation::reserve(entries.iter().flatten().count(), ledger)?;
    for entry in entries.into_iter().flatten() {
        let path = entry
            .path
            .as_ref()
            .and_then(|span| span.get(input))
            .ok_or_else(Failure::internal)?;
        let original = entry.original(input)?;
        crate::activation::push(&mut result, Example { path, original }, ledger)?;
    }
    Ok(result)
}

fn select<const N: usize>(
    input: &[u8],
    selector: Selector<'_>,
    ledger: &mut dyn SharedLedger,
) -> Result<[Option<Located>; N], Failure> {
    let mut policy = Policy {
        ledger,
        refused: None,
    };
    let selected = {
        let context = AdmissionContext::new(&mut policy);
        let mut json = serde_json::Deserializer::from_slice_with_admission(input, &context);
        Selection::<N>(selector)
            .deserialize(&mut json)
            .and_then(|entries| {
                json.end()?;
                let mut selected = std::array::from_fn(|_| None);
                for (i, entry) in entries.into_iter().enumerate() {
                    if let Some(entry) = entry {
                        selected[i] = Some(Located::from_entry(input, entry).ok_or_else(|| {
                            <serde_json::Error as de::Error>::custom("archive field not borrowed")
                        })?);
                    }
                }
                Ok(selected)
            })
    };
    selected.map_err(|error| policy.failure(error))
}

struct Span(std::ops::Range<usize>);

impl Span {
    fn locate(input: &[u8], value: &str) -> Option<Self> {
        // Keep the offset of a selected borrowed field after the many other
        // package fields and the admitted parser have been dropped.
        let start = value.as_ptr().addr().checked_sub(input.as_ptr().addr())?;
        Some(Self(start..start.checked_add(value.len())?))
    }

    fn get<'a>(&self, input: &'a [u8]) -> Option<&'a str> {
        std::str::from_utf8(input.get(self.0.clone())?).ok()
    }
}

struct Located {
    path: Option<Span>,
    base64: Span,
    digest: [u8; 32],
    length: u64,
}

impl Located {
    fn from_entry(input: &[u8], entry: Entry<'_>) -> Option<Self> {
        Some(Self {
            path: entry.path.and_then(|path| Span::locate(input, path)),
            base64: Span::locate(input, entry.base64)?,
            digest: crate::hex::<32>(entry.sha256)?,
            length: entry.length,
        })
    }

    fn original<'a>(&self, input: &'a [u8]) -> Result<Original<'a>, Failure> {
        if self.length > MAX_FILE_BYTES {
            return Err(Failure::internal());
        }
        let bytes_base64 = self.base64.get(input).ok_or_else(Failure::internal)?;
        if u64::try_from(bytes_base64.len()).ok() != Some(self.length.div_ceil(3) * 4) {
            return Err(Failure::internal());
        }
        Ok(Original {
            bytes_base64,
            length: Decimal(self.length),
            sha256: Digest(self.digest),
        })
    }
}

struct Policy<'a> {
    ledger: &'a mut dyn SharedLedger,
    refused: Option<Refusal>,
}

impl Policy<'_> {
    fn refuse(&mut self, refusal: Refusal) -> AdmissionError {
        self.refused = Some(refusal);
        AdmissionError::Refused
    }

    fn failure(&self, error: serde_json::Error) -> Failure {
        if let Some(refused) = self.refused {
            return Failure::refused(refused);
        }
        if let Some(AdmissionError::AllocatorFailed { requested }) = error.admission_error() {
            return Failure::refused(Refusal::Allocation { requested });
        }
        Failure::internal()
    }
}

impl Admission for Policy<'_> {
    fn grow(&mut self, additional: usize) -> Result<(), AdmissionError> {
        let bytes = u64::try_from(additional).map_err(|_| AdmissionError::ExtentOverflow {
            elements: additional,
        })?;
        self.ledger
            .try_charge(bytes)
            .map_err(|error| self.refuse(Refusal::Budget(error)))
    }

    fn checkpoint(&mut self, consumed: usize) -> Result<(), AdmissionError> {
        let entries = u64::try_from(consumed)
            .map_err(|_| AdmissionError::ExtentOverflow { elements: consumed })?;
        self.ledger
            .checkpoint()
            .map_err(|error| self.refuse(Refusal::Stopped(error)))?;
        self.ledger
            .try_charge_entries(entries)
            .map_err(|error| self.refuse(Refusal::Budget(error)))
    }
}

#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "snake_case")]
enum PackageField {
    PackageVersion,
    Files,
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "snake_case")]
enum FileField {
    Path,
    Length,
    Sha256,
    BytesBase64,
    #[serde(other)]
    Other,
}

struct Entry<'a> {
    path: Option<&'a str>,
    length: u64,
    sha256: &'a str,
    base64: &'a str,
}

#[derive(Clone, Copy)]
enum Selector<'a> {
    Path(&'a str),
    Examples,
}

impl Selector<'_> {
    fn matches(self, path: &str) -> bool {
        match self {
            Self::Path(wanted) => path == wanted,
            Self::Examples => {
                let Some(file) = path
                    .strip_prefix("proposals/atoms/")
                    .or_else(|| path.strip_prefix("proposals/edges/"))
                else {
                    return false;
                };
                file.len() <= 255
                    && file.starts_with("example-")
                    && file.ends_with(".md")
                    && file
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            }
        }
    }
}

struct Selection<'a, const N: usize>(Selector<'a>);

impl<'de, const N: usize> DeserializeSeed<'de> for Selection<'_, N> {
    type Value = [Option<Entry<'de>>; N];

    fn deserialize<D: de::Deserializer<'de>>(self, decoder: D) -> Result<Self::Value, D::Error> {
        decoder.deserialize_map(self)
    }
}

impl<'de, const N: usize> Visitor<'de> for Selection<'_, N> {
    type Value = [Option<Entry<'de>>; N];

    fn expecting(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt.write_str("v1 committed import package")
    }

    fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
        let mut version = None;
        let mut selected = None;
        while let Some(key) = map.next_key()? {
            match key {
                PackageField::PackageVersion if version.is_none() => {
                    version = Some(map.next_value::<u32>()?)
                }
                PackageField::Files if selected.is_none() => {
                    selected = Some(map.next_value_seed(Files::<N>(self.0))?)
                }
                PackageField::Other => {
                    map.next_value::<IgnoredAny>()?;
                }
                _ => return Err(de::Error::custom("duplicate package field")),
            }
        }
        if version != Some(1) {
            return Err(de::Error::custom("unsupported import package version"));
        }
        selected.ok_or_else(|| de::Error::missing_field("files"))
    }
}

struct Files<'a, const N: usize>(Selector<'a>);

impl<'de, const N: usize> DeserializeSeed<'de> for Files<'_, N> {
    type Value = [Option<Entry<'de>>; N];

    fn deserialize<D: de::Deserializer<'de>>(self, decoder: D) -> Result<Self::Value, D::Error> {
        decoder.deserialize_seq(self)
    }
}

impl<'de, const N: usize> Visitor<'de> for Files<'_, N> {
    type Value = [Option<Entry<'de>>; N];

    fn expecting(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt.write_str("import package files")
    }

    fn visit_seq<S: SeqAccess<'de>>(self, mut seq: S) -> Result<Self::Value, S::Error> {
        let mut selected: [Option<Entry<'de>>; N] = std::array::from_fn(|_| None);
        let mut count = 0;
        while let Some(file) = seq.next_element_seed(File(self.0))? {
            if let Some(entry) = file {
                let slot = selected
                    .get_mut(count)
                    .ok_or_else(|| de::Error::custom("too many matching package paths"))?;
                *slot = Some(entry);
                count += 1;
            }
        }
        Ok(selected)
    }
}

struct File<'a>(Selector<'a>);

impl<'de> DeserializeSeed<'de> for File<'_> {
    type Value = Option<Entry<'de>>;

    fn deserialize<D: de::Deserializer<'de>>(self, decoder: D) -> Result<Self::Value, D::Error> {
        decoder.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for File<'_> {
    type Value = Option<Entry<'de>>;

    fn expecting(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt.write_str("import package file")
    }

    fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
        let (mut wanted, mut length, mut sha256, mut base64) = (None, None, None, None);
        while let Some(key) = map.next_key()? {
            match key {
                FileField::Path if wanted.is_none() => {
                    wanted = Some(map.next_value_seed(PathMatch(self.0))?)
                }
                FileField::Length if length.is_none() => length = Some(map.next_value()?),
                FileField::Sha256 if sha256.is_none() => sha256 = Some(map.next_value()?),
                FileField::BytesBase64 if base64.is_none() => base64 = Some(map.next_value()?),
                FileField::Other => {
                    map.next_value::<IgnoredAny>()?;
                }
                _ => return Err(de::Error::custom("duplicate file field")),
            }
        }
        match (wanted, length, sha256, base64) {
            (Some((true, path)), Some(length), Some(sha256), Some(base64)) => Ok(Some(Entry {
                path,
                length,
                sha256,
                base64,
            })),
            (Some((false, _)), Some(_), Some(_), Some(_)) => Ok(None),
            _ => Err(de::Error::custom("incomplete import package file")),
        }
    }
}

struct PathMatch<'a>(Selector<'a>);

impl<'de> DeserializeSeed<'de> for PathMatch<'_> {
    type Value = (bool, Option<&'de str>);

    fn deserialize<D: de::Deserializer<'de>>(self, decoder: D) -> Result<Self::Value, D::Error> {
        decoder.deserialize_str(self)
    }
}

impl<'de> Visitor<'de> for PathMatch<'_> {
    type Value = (bool, Option<&'de str>);

    fn expecting(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt.write_str("package file path")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        let matches = self.0.matches(value);
        if matches && matches!(self.0, Selector::Examples) {
            return Err(de::Error::custom("escaped example path"));
        }
        Ok((matches, None))
    }

    fn visit_borrowed_str<E: de::Error>(self, value: &'de str) -> Result<Self::Value, E> {
        let matches = self.0.matches(value);
        Ok((matches, matches.then_some(value)))
    }
}
