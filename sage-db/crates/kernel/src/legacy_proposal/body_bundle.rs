//! A v4 request frame followed by the hashed proposal body in the one required blob owner.

use sage_db_store::typed;
use sage_db_types::ledger::SharedLedger;

use crate::claims::{CommandError, admitted_vec, extent, map_format};

use super::malformed;

const TAG: &[u8; 8] = b"SGPBODY4";
const PREFIX: usize = TAG.len() + 4;
const BODY_REF_BYTES: usize = 8 + 32;

/// Maximum raw UTF-8 extent of a large compatibility body from the public write gate.
pub const MAX_COMPAT_PROPOSED_BODY_BYTES: usize = 256 * 1024;
const MAX_UTF16_UNITS: usize = 128 * 1024;

fn check_extent(body: &str, ledger: &mut dyn SharedLedger) -> Result<(), CommandError> {
    if body.len() > MAX_COMPAT_PROPOSED_BODY_BYTES {
        return Err(malformed());
    }
    for (index, _) in body.encode_utf16().enumerate() {
        if index == MAX_UTF16_UNITS {
            return Err(malformed());
        }
        ledger.try_charge_entries(1)?;
        if index.is_multiple_of(1024) {
            ledger.checkpoint()?;
        }
    }
    Ok(())
}

pub(super) fn bind(
    body: &str,
    metadata: &[u8],
    ledger: &mut dyn SharedLedger,
) -> Result<Vec<u8>, CommandError> {
    check_extent(body, ledger)?;
    let size = extent(body.len())?;
    let sha256 = map_format(typed::raw_hash(body.as_bytes(), ledger))?;
    let total = BODY_REF_BYTES
        .checked_add(metadata.len())
        .ok_or_else(malformed)?;
    let mut op = admitted_vec(total, ledger)?;
    op.extend_from_slice(&size.to_le_bytes());
    op.extend_from_slice(&sha256);
    op.extend_from_slice(metadata);
    Ok(op)
}

pub(super) fn open<'a>(
    op: &'a [u8],
    raw: &'a [u8],
    ledger: &mut dyn SharedLedger,
) -> Result<(&'a [u8], &'a str, [u8; 32]), CommandError> {
    let (size, tail) = op.split_first_chunk::<8>().ok_or_else(malformed)?;
    let (sha, metadata) = tail.split_first_chunk::<32>().ok_or_else(malformed)?;
    if u64::from_le_bytes(*size) != extent(raw.len())? || raw.len() > MAX_COMPAT_PROPOSED_BODY_BYTES
    {
        return Err(malformed());
    }
    let text = core::str::from_utf8(raw).map_err(|_| malformed())?;
    check_extent(text, ledger)?;
    if map_format(typed::raw_hash(raw, ledger))? != *sha {
        return Err(malformed());
    }
    Ok((metadata, text, *sha))
}

pub(crate) fn pack(
    frame: &[u8],
    body: &str,
    ledger: &mut dyn SharedLedger,
) -> Result<Vec<u8>, CommandError> {
    let frame_len = u32::try_from(frame.len()).map_err(|_| malformed())?;
    let total = PREFIX
        .checked_add(frame.len())
        .and_then(|n| n.checked_add(body.len()))
        .ok_or_else(malformed)?;
    // This format deliberately stays inside the existing leased proposal-read ceiling.
    if extent(total)? > typed::MAX_FRAME_BYTES {
        return Err(malformed());
    }
    let mut owner = admitted_vec(total, ledger)?;
    owner.extend_from_slice(TAG);
    owner.extend_from_slice(&frame_len.to_le_bytes());
    owner.extend_from_slice(frame);
    owner.extend_from_slice(body.as_bytes());
    Ok(owner)
}

pub(super) fn split(owner: &[u8]) -> Result<(&[u8], Option<&[u8]>), CommandError> {
    let Some(tail) = owner.strip_prefix(TAG) else {
        return Ok((owner, None));
    };
    let (width, tail) = tail.split_first_chunk::<4>().ok_or_else(malformed)?;
    let width = usize::try_from(u32::from_le_bytes(*width)).map_err(|_| malformed())?;
    let (frame, raw) = tail.split_at_checked(width).ok_or_else(malformed)?;
    Ok((frame, Some(raw)))
}
