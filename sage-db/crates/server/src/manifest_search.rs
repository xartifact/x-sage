//! Legacy metadata projections over the current, complete native publication.

use std::cmp::Ordering;
use std::io::{self, Write};
use std::net::TcpStream;
use std::sync::LazyLock;

use regex_syntax::Parser;
use regex_syntax::hir::{Class, ClassUnicode, HirKind};
use sage_db_kernel::auth::{Capability, Grants};
use sage_db_kernel::claims::CommandError;
use sage_db_protocol::generated::ErrorCode;
use sage_db_store::fold::FoldedRecord;
use sage_db_store::fs::FileSystem;
use sage_db_store::handle::Store;
use sage_db_store::lease::AttemptLedger;
use sage_db_store::view::{ImportReadiness, RecordReadError, RecordScanError};
use sage_db_types::ledger::SharedLedger;
use sage_db_types::persist::TermRecord;
use serde::{Serialize, Serializer, ser::SerializeSeq};

use crate::activation::{
    Format, JSON, MARKDOWN, MAX_TERMS, TEXT, comma_list, decode_component, file_order, format,
    limit, parameter, parts, push, render_error, reserve, source_label, step, try_sort,
};
use crate::http::{self, Deadline, Failure, Head, Route};

const MAX_KEYWORD_BYTES: usize = 1024;
// Unicode lowercase expands one scalar to at most three scalars of up to four bytes each.
const MAX_LOWER_BYTES: usize = 12 * MAX_KEYWORD_BYTES;
const MANIFEST_HEADER: &str = concat!(
    "# MANIFEST — 全局原子索引\n\n",
    "> 每次 AI 会话开始时必须加载本文件。\n",
    "> 格式：每个概念一行紧凑描述 + 触发关键词。\n\n",
    "| id | atom | layer | source | confidence | triggers |\n",
    "|----|------|-------|--------|------------|----------|\n",
);

pub(crate) fn serve<F: FileSystem>(
    store: &Store<F>,
    grants: &Grants<'_>,
    head: &Head<'_>,
    stream: &TcpStream,
    deadline: &Deadline,
    ledger: &mut AttemptLedger<'_>,
) -> Result<(), Failure> {
    grants
        .authorize(store.kb_uuid(), Capability::Read, ledger)
        .map_err(crate::runtime::auth_error)?;
    if let ImportReadiness::Incomplete { .. } = store
        .import_readiness(ledger)
        .map_err(|error| crate::runtime::kernel_error(error.into()))?
    {
        return Err(crate::runtime::kernel_error(CommandError::ImportInProgress));
    }
    let (_, query) = parts(head);
    let mut kb_buf = [0; 64];
    let requested = parameter(query, "kb")
        .map(|raw| decode_component(raw, &mut kb_buf, true))
        .transpose()?;
    let mut query_buf = [0; MAX_KEYWORD_BYTES];
    let mut lower_buf = [0; MAX_LOWER_BYTES];
    let mut fallback = [0; MAX_LOWER_BYTES];
    let keyword = if matches!(head.route, Route::Search) {
        let raw = parameter(query, "q")
            .filter(|raw| !raw.is_empty())
            .ok_or_else(|| Failure::bad("Missing query parameter: q"))?;
        Some(decode_component(raw, &mut query_buf, true)?)
    } else {
        None
    };
    let needle = keyword
        .map(|text| Needle::new(text, &mut lower_buf, &mut fallback, ledger))
        .transpose()?;
    let rebuild = matches!(head.route, Route::ManifestRebuild);
    let mut rows = reserve(if rebuild { 0 } else { MAX_TERMS }, ledger)?;
    let mut kb_slug = None;
    let mut scratch = [0; MAX_LOWER_BYTES];
    store
        .scan_current_records(ledger, |record, work| {
            match record {
                FoldedRecord::KbMetadata(meta) => kb_slug = Some(meta.slug.as_str()),
                FoldedRecord::Term(term) if !term.retired && !rebuild => {
                    let score = match &needle {
                        Some(needle) => rank(term, needle, &mut scratch, work)?,
                        None => 1,
                    };
                    if score != 0 {
                        if rows.len() == MAX_TERMS {
                            return Err(limit("catalog result exceeds its row ceiling"));
                        }
                        push(&mut rows, Match { term, score }, work)?;
                    }
                }
                _ => {}
            }
            Ok(())
        })
        .map_err(|error| match error {
            RecordScanError::Visitor(failure) => failure,
            RecordScanError::Read(RecordReadError::Refused(refusal)) => Failure::refused(refusal),
            RecordScanError::Incomplete(_) => {
                Failure::unavailable("no complete readable publication")
            }
            RecordScanError::Read(_) => Failure::internal(),
        })?;
    if requested.is_some_and(|slug| Some(slug) != kb_slug) {
        return Err(Failure::new(
            "404 Not Found",
            ErrorCode::InvalidArgument,
            "Unknown knowledge base",
        ));
    }
    if rebuild {
        return Err(Failure::new(
            "409 Conflict",
            ErrorCode::Conflict,
            concat!(
                "MANIFEST.md is derived from the structured store and no longer needs regenerating. ",
                "Read it with GET /api/manifest; the file is a projection, not an input."
            ),
        ));
    }
    try_sort(&mut rows, before, ledger)?;
    let manifest_format = parameter(query, "format") == Some("manifest");
    let selected = format(head);
    if keyword.is_none() && (selected != Format::Json || manifest_format)
        || keyword.is_some() && manifest_format
    {
        return respond(stream, deadline, ledger, MARKDOWN, |out| {
            manifest(out, &rows)?;
            out.write_all(b"\n")
        });
    }
    if let Some(keyword) = keyword {
        if selected == Format::Text {
            return respond(stream, deadline, ledger, TEXT, |out| {
                search_text(out, keyword, &rows)
            });
        }
        return respond(stream, deadline, ledger, JSON, |out| {
            serde_json::to_writer(
                &mut *out,
                &SearchOutput {
                    keyword,
                    results: SearchRows(&rows),
                },
            )?;
            out.write_all(b"\n")
        });
    }
    let mut storage = [0; http::MAX_RESPONSE];
    let markdown =
        http::render(&mut storage, ledger, |out| manifest(out, &rows)).map_err(render_error)?;
    let markdown = std::str::from_utf8(markdown).map_err(|_| Failure::internal())?;
    #[derive(Serialize)]
    struct ManifestOutput<'a> {
        markdown: &'a str,
    }
    respond(stream, deadline, ledger, JSON, |out| {
        serde_json::to_writer(&mut *out, &ManifestOutput { markdown })?;
        out.write_all(b"\n")
    })
}

fn respond(
    stream: &TcpStream,
    deadline: &Deadline,
    ledger: &mut dyn SharedLedger,
    content_type: &'static str,
    write: impl FnOnce(&mut dyn Write) -> io::Result<()>,
) -> Result<(), Failure> {
    http::respond_with(stream, "200 OK", content_type, deadline, ledger, write)
        .map_err(render_error)
}

struct Match<'a> {
    term: &'a TermRecord,
    score: u8,
}

fn before(a: &Match<'_>, b: &Match<'_>, ledger: &mut dyn SharedLedger) -> Result<bool, Failure> {
    let class_order = b
        .score
        .cmp(&a.score)
        .then(a.term.layer.get().cmp(&b.term.layer.get()));
    if class_order != Ordering::Equal {
        return Ok(class_order.is_lt());
    }
    let mut left = file_order(a.term).encode_utf16();
    let mut right = file_order(b.term).encode_utf16();
    loop {
        step(ledger)?;
        match (left.next(), right.next()) {
            (None, None) => return Ok(a.term.key.id() < b.term.key.id()),
            (lhs, rhs) if lhs != rhs => return Ok(lhs < rhs),
            _ => {}
        }
    }
}

#[derive(Serialize)]
struct SearchOutput<'a> {
    keyword: &'a str,
    results: SearchRows<'a>,
}

struct SearchRows<'a>(&'a [Match<'a>]);

impl Serialize for SearchRows<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Row<'a> {
            id: &'a str,
            atom: &'a str,
            layer: u8,
            domain: &'a [String],
            confidence: &'static str,
            source: &'static str,
            triggers: &'a [String],
        }
        let mut list = serializer.serialize_seq(Some(self.0.len()))?;
        for hit in self.0 {
            let term = hit.term;
            list.serialize_element(&Row {
                id: &term.slug,
                atom: &term.atom,
                layer: term.layer.get(),
                domain: &term.domains,
                confidence: term.confidence.as_str(),
                source: source_label(term.source),
                triggers: &term.triggers,
            })?;
        }
        list.end()
    }
}

fn manifest(out: &mut dyn Write, rows: &[Match<'_>]) -> io::Result<()> {
    out.write_all(MANIFEST_HEADER.as_bytes())?;
    for row in rows {
        let term = row.term;
        write!(
            out,
            "| {} | {} | {} | {} | {} | ",
            term.slug,
            term.atom,
            term.layer.get(),
            source_label(term.source),
            term.confidence.as_str()
        )?;
        comma_list(out, &term.triggers)?;
        out.write_all(b" |\n")?;
    }
    Ok(())
}

fn search_text(out: &mut dyn Write, keyword: &str, rows: &[Match<'_>]) -> io::Result<()> {
    if rows.is_empty() {
        return writeln!(out, "No concepts found matching \"{keyword}\".");
    }
    for row in rows {
        let term = row.term;
        writeln!(
            out,
            "{}  (L{}, {})  {}",
            term.slug,
            term.layer.get(),
            term.confidence.as_str(),
            term.atom
        )?;
        out.write_all(b"  triggers: ")?;
        comma_list(out, &term.triggers)?;
        out.write_all(b"\n\n")?;
    }
    Ok(())
}

fn rank(
    term: &TermRecord,
    needle: &Needle<'_>,
    scratch: &mut [u8],
    ledger: &mut dyn SharedLedger,
) -> Result<u8, Failure> {
    let slug = needle.matches(lowercase(&term.slug, scratch, ledger)?, ledger)?;
    if slug == Hit::Exact {
        return Ok(4);
    }
    let mut rank = u8::from(slug == Hit::Contains);
    if needle.matches(lowercase(&term.atom, scratch, ledger)?, ledger)? != Hit::Absent {
        rank = 2;
    }
    for trigger in &term.triggers {
        match needle.matches(lowercase(trigger, scratch, ledger)?, ledger)? {
            Hit::Exact => return Ok(3),
            Hit::Contains => rank = rank.max(1),
            Hit::Absent => {}
        }
    }
    Ok(rank)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Hit {
    Exact,
    Contains,
    Absent,
}

/// KMP searches each folded field in linear work, including repetitive targets.
struct Needle<'a> {
    bytes: &'a [u8],
    fallback: &'a [u16],
}

impl<'a> Needle<'a> {
    fn new(
        query: &str,
        scratch: &'a mut [u8],
        fallback: &'a mut [u16],
        ledger: &mut dyn SharedLedger,
    ) -> Result<Self, Failure> {
        let bytes = lowercase(query, scratch, ledger)?;
        if bytes.is_empty() {
            return Err(Failure::internal());
        }
        let fallback = &mut fallback[..bytes.len()];
        let mut matched = 0;
        for index in 1..bytes.len() {
            matched = Self::advance(bytes, fallback, matched, bytes[index], ledger)?;
            fallback[index] = u16::try_from(matched).map_err(|_| Failure::internal())?;
        }
        Ok(Self { bytes, fallback })
    }

    fn advance(
        bytes: &[u8],
        fallback: &[u16],
        mut matched: usize,
        next: u8,
        ledger: &mut dyn SharedLedger,
    ) -> Result<usize, Failure> {
        loop {
            step(ledger)?;
            if bytes[matched] == next {
                return Ok(matched + 1);
            }
            if matched == 0 {
                return Ok(0);
            }
            matched = usize::from(fallback[matched - 1]);
        }
    }

    fn matches(&self, field: &[u8], ledger: &mut dyn SharedLedger) -> Result<Hit, Failure> {
        step(ledger)?;
        if self.bytes.len() > field.len() {
            return Ok(Hit::Absent);
        }
        let mut matched = 0;
        for byte in field {
            matched = Self::advance(self.bytes, self.fallback, matched, *byte, ledger)?;
            if matched == self.bytes.len() {
                return Ok(if field.len() == self.bytes.len() {
                    Hit::Exact
                } else {
                    Hit::Contains
                });
            }
        }
        Ok(Hit::Absent)
    }
}

fn unicode_property(pattern: &str) -> ClassUnicode {
    match Parser::new()
        .parse(pattern)
        .expect("fixed Unicode property")
        .into_kind()
    {
        HirKind::Class(Class::Unicode(class)) => class,
        _ => unreachable!("expected a Unicode class"),
    }
}

static CASED: LazyLock<ClassUnicode> = LazyLock::new(|| unicode_property(r"\p{Cased}"));
static CASE_IGNORABLE: LazyLock<ClassUnicode> =
    LazyLock::new(|| unicode_property(r"\p{Case_Ignorable}"));

pub(crate) fn initialize() {
    LazyLock::force(&CASED);
    LazyLock::force(&CASE_IGNORABLE);
}

fn contains(class: &ClassUnicode, ch: char) -> bool {
    class
        .ranges()
        .binary_search_by(|range| {
            if range.end() < ch {
                Ordering::Less
            } else if range.start() > ch {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        })
        .is_ok()
}

fn lowercase<'a>(
    value: &str,
    buffer: &'a mut [u8],
    ledger: &mut dyn SharedLedger,
) -> Result<&'a [u8], Failure> {
    let mut written = 0;
    let mut preceding_cased = false;
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        step(ledger)?;
        if ch == 'Σ' && preceding_cased {
            let mut following_cased = false;
            for next in chars.clone() {
                step(ledger)?;
                if !contains(&CASE_IGNORABLE, next) {
                    following_cased = contains(&CASED, next);
                    break;
                }
            }
            append(
                if following_cased { 'σ' } else { 'ς' },
                buffer,
                &mut written,
            )?;
        } else {
            for lower in ch.to_lowercase() {
                append(lower, buffer, &mut written)?;
            }
        }
        if !contains(&CASE_IGNORABLE, ch) {
            preceding_cased = contains(&CASED, ch);
        }
    }
    Ok(&buffer[..written])
}

fn append(ch: char, buffer: &mut [u8], offset: &mut usize) -> Result<(), Failure> {
    let end = *offset + ch.len_utf8();
    ch.encode_utf8(buffer.get_mut(*offset..end).ok_or_else(Failure::internal)?);
    *offset = end;
    Ok(())
}

#[cfg(test)]
mod tests {
    use sage_db_types::query::RunToCompletion;

    use super::*;

    fn ledger() -> AttemptLedger<'static> {
        AttemptLedger::open(1024, 10_000, &RunToCompletion).unwrap()
    }

    #[test]
    fn lowercase_handles_context_and_expansion() {
        let mut buf = [0; MAX_LOWER_BYTES];
        for value in [
            "Σ",
            "ΟΣ",
            "ΟΣΑ",
            "Ο.Σ",
            "ΟΣ.Α",
            "Ο'Σ",
            "A\u{0301}Σ",
            "İ",
            "风险 AGENT",
        ] {
            assert_eq!(
                lowercase(value, &mut buf, &mut ledger()).ok().unwrap(),
                value.to_lowercase().as_bytes(),
                "{value}"
            );
        }
    }

    #[test]
    fn folded_search_handles_overlapping_prefix_and_unicode() {
        let mut buf = [0; MAX_LOWER_BYTES];
        let mut table = [0; MAX_LOWER_BYTES];
        let mut scratch = [0; MAX_LOWER_BYTES];
        for (query, field, expected) in [
            ("aab", "aaab", Hit::Contains),
            ("ABA", "aba", Hit::Exact),
            ("aaab", "aaaaa", Hit::Absent),
            ("ος", "ΟΣ", Hit::Exact),
            ("οσ", "ΟΣ", Hit::Absent),
            ("i\u{0307}", "XİX", Hit::Contains),
            ("风", "识别风险", Hit::Contains),
        ] {
            let mut work = ledger();
            let needle = Needle::new(query, &mut buf, &mut table, &mut work)
                .ok()
                .unwrap();
            let field = lowercase(field, &mut scratch, &mut work).ok().unwrap();
            assert!(
                needle.matches(field, &mut work).ok() == Some(expected),
                "{query}"
            );
        }
    }
}
