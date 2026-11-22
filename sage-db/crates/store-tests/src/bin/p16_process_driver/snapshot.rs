//! Strict counted/checksummed test transport. It copies model path/byte observations;
//! it is not a Sage format serializer and never repairs or normalizes a path.
use sage_db_store_tests::model::{DurableState, RelPath};
use sage_db_types::persist::StateEntry;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{self, Read, Write},
    path::Path,
};
const MAGIC: &str = "P16SNAP2";
const MAX_BYTES: usize = 64 * 1024 * 1024;
const MAX_ITEMS: usize = 4096;
fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}
pub(crate) fn hex(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "-".into();
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn unhex(s: &str) -> io::Result<Vec<u8>> {
    if s == "-" {
        return Ok(Vec::new());
    }
    if s.is_empty() || !s.len().is_multiple_of(2) || s.len() > MAX_BYTES * 2 {
        return Err(bad("hex width"));
    }
    s.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|p| {
            if !p
                .iter()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c))
            {
                return Err(bad("noncanonical hex"));
            }
            let v = std::str::from_utf8(p).map_err(|_| bad("hex utf8"))?;
            u8::from_str_radix(v, 16).map_err(|_| bad("hex digit"))
        })
        .collect()
}
fn path_from(s: &str) -> io::Result<RelPath> {
    let bytes = unhex(s)?;
    let s = std::str::from_utf8(&bytes).map_err(|_| bad("path utf8"))?;
    if s.is_empty() {
        return Ok(RelPath::root());
    }
    let mut path = RelPath::root();
    for component in s.split('/') {
        if component.is_empty() {
            return Err(bad("empty path component; no normalization"));
        }
        path = RelPath::join(&path, component);
    }
    Ok(path)
}
pub(crate) fn encode(state: &DurableState) -> io::Result<Vec<u8>> {
    if state.dirs.len() > MAX_ITEMS || state.files.len() > MAX_ITEMS {
        return Err(bad("item ceiling"));
    }
    let mut out = format!("{MAGIC}\nDIRS {}\n", state.dirs.len()).into_bytes();
    for dir in &state.dirs {
        writeln!(out, "D {}", hex(dir.as_str().as_bytes()))?;
    }
    writeln!(out, "FILES {}", state.files.len())?;
    for (path, bytes) in &state.files {
        writeln!(
            out,
            "F {} {} {}",
            hex(path.as_str().as_bytes()),
            bytes.len(),
            hex(bytes)
        )?;
    }
    if out.len() > MAX_BYTES {
        return Err(bad("transport ceiling"));
    }
    let hash = sage_db_format::digest::raw_sha256(&out);
    writeln!(out, "END {}", hex(&hash))?;
    Ok(out)
}
fn count(line: &str, tag: &str) -> io::Result<usize> {
    let parts: Vec<_> = line.split(' ').collect();
    if parts.len() != 2 || parts[0] != tag {
        return Err(bad("count header"));
    }
    let n = parts[1].parse::<usize>().map_err(|_| bad("count number"))?;
    if n > MAX_ITEMS {
        return Err(bad("item ceiling"));
    }
    Ok(n)
}
pub(crate) fn decode(bytes: &[u8]) -> io::Result<DurableState> {
    if bytes.len() > MAX_BYTES + 80 || !bytes.ends_with(b"\n") {
        return Err(bad("length/truncated footer"));
    }
    let text = std::str::from_utf8(bytes).map_err(|_| bad("transport utf8"))?;
    let cut = text.rfind("END ").ok_or_else(|| bad("missing footer"))?;
    let footer = &text[cut..];
    let expected = format!(
        "END {}\n",
        hex(&sage_db_format::digest::raw_sha256(&bytes[..cut]))
    );
    if footer != expected {
        return Err(bad("checksum/trailing data"));
    }
    let mut lines = text[..cut].lines();
    if lines.next() != Some(MAGIC) {
        return Err(bad("magic"));
    }
    let nd = count(lines.next().ok_or_else(|| bad("missing dirs"))?, "DIRS")?;
    let mut dirs = BTreeSet::new();
    for _ in 0..nd {
        let line = lines.next().ok_or_else(|| bad("truncated dirs"))?;
        let parts: Vec<_> = line.split(' ').collect();
        if parts.len() != 2 || parts[0] != "D" {
            return Err(bad("dir row"));
        }
        if !dirs.insert(path_from(parts[1])?) {
            return Err(bad("duplicate dir"));
        }
    }
    let nf = count(lines.next().ok_or_else(|| bad("missing files"))?, "FILES")?;
    let mut files = BTreeMap::new();
    for _ in 0..nf {
        let line = lines.next().ok_or_else(|| bad("truncated files"))?;
        let parts: Vec<_> = line.split(' ').collect();
        if parts.len() != 4 || parts[0] != "F" {
            return Err(bad("file row"));
        }
        let path = path_from(parts[1])?;
        let len = parts[2].parse::<usize>().map_err(|_| bad("file length"))?;
        let data = unhex(parts[3])?;
        if data.len() != len {
            return Err(bad("file length differs"));
        }
        if dirs.contains(&path) || files.insert(path, data).is_some() {
            return Err(bad("duplicate/conflicting file"));
        }
    }
    if lines.next().is_some() || !dirs.contains(&RelPath::root()) {
        return Err(bad("extra/missing root"));
    }
    Ok(DurableState { files, dirs })
}
pub(crate) fn write(path: &Path, state: &DurableState) -> io::Result<()> {
    let bytes = encode(state)?;
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    out.write_all(&bytes)?;
    out.sync_all()?;
    Ok(())
}
pub(crate) fn read(path: &Path) -> io::Result<DurableState> {
    let f = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    f.take((MAX_BYTES + 81) as u64).read_to_end(&mut bytes)?;
    decode(&bytes)
}
pub(crate) fn write_rows(path: &Path, rows: &[StateEntry]) -> io::Result<()> {
    let mut state = DurableState::default();
    state.dirs.insert(RelPath::root());
    for row in rows {
        let name = format!("{}:{}:{}", row.kind, row.revision, hex(&row.key));
        if state
            .files
            .insert(RelPath::join(&RelPath::root(), &name), row.body.clone())
            .is_some()
        {
            return Err(bad("duplicate reference row"));
        }
    }
    write(path, &state)
}
pub(crate) fn read_rows(path: &Path) -> io::Result<Vec<StateEntry>> {
    let state = read(path)?;
    let mut rows = Vec::new();
    let mut seen = BTreeSet::new();
    for (path, body) in state.files {
        let parts: Vec<_> = path.as_str().split(':').collect();
        if parts.len() != 3 {
            return Err(bad("reference row shape"));
        }
        let kind = parts[0].parse().map_err(|_| bad("reference kind"))?;
        let revision = parts[1].parse().map_err(|_| bad("reference revision"))?;
        let key = unhex(parts[2])?;
        if !seen.insert((kind, key.clone())) {
            return Err(bad("duplicate reference identity"));
        }
        rows.push(StateEntry {
            kind,
            key,
            revision,
            body,
        });
    }
    rows.sort_by(|a, b| (a.kind, a.key.as_slice()).cmp(&(b.kind, b.key.as_slice())));
    Ok(rows)
}
#[cfg(test)]
mod tests {
    use super::*;
    fn state() -> DurableState {
        let mut s = DurableState::default();
        s.dirs.insert(RelPath::root());
        s.dirs.insert(RelPath::join(&RelPath::root(), "blobs"));
        s.files
            .insert(RelPath::join(&RelPath::root(), "empty"), Vec::new());
        s.files
            .insert(RelPath::join(&RelPath::root(), "value"), vec![0, 1, 255]);
        s
    }
    fn seal(prefix: &str) -> Vec<u8> {
        format!(
            "{prefix}END {}\n",
            hex(&sage_db_format::digest::raw_sha256(prefix.as_bytes()))
        )
        .into_bytes()
    }
    #[test]
    fn transport_is_byte_exact() {
        let s = state();
        assert_eq!(decode(&encode(&s).unwrap()).unwrap(), s);
    }
    #[test]
    fn malformed_never_becomes_empty_or_normalized() {
        let good = encode(&state()).unwrap();
        assert!(decode(&good[..good.len() - 2]).is_err());
        for bad in [
            "BAD\nDIRS 1\nD -\nFILES 0\n",
            "P16SNAP2\nDIRS 2\nD -\nD -\nFILES 0\n",
            "P16SNAP2\nDIRS 1\nD -\nFILES 2\nF 61 0 -\nF 61 0 -\n",
            "P16SNAP2\nDIRS 1\nD -\nFILES 1\nF 612f2f62 0 -\n",
            "P16SNAP2\nDIRS 1\nD -\nFILES 1\nF 61 1 -\n",
        ] {
            assert!(decode(&seal(bad)).is_err(), "{bad}");
        }
        let mut corrupt = good.clone();
        corrupt[0] ^= 1;
        assert!(decode(&corrupt).is_err());
        let mut extra = good;
        extra.extend_from_slice(b"junk\n");
        assert!(decode(&extra).is_err());
    }
}
