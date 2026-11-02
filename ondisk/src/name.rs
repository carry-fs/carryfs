//! Filename profile 2 (FORMAT.md section 7): NFC stored names and their
//! full-case-folded comparison keys, computed without allocation.

use core::cmp::Ordering;
use core::str;

use super::error::{Error, Result};
use super::name_tables;

/// Longest stored name in UTF-8 bytes.
pub const MAX_STORED_BYTES: usize = 255;
/// Longest comparison key in UTF-8 bytes.
pub const MAX_FOLDED_BYTES: usize = 765;
// Unicode 16.0 canonical decomposition expands one scalar to at most four
// scalars; full folding expands it to at most three. The input is at most 255
// UTF-8 bytes, so this fixed bound keeps disk-derived work bounded.
const MAX_CODE_POINTS: usize = 4_096;

/// Working memory for one name: two code-point buffers and the encoded
/// results, about 33 KiB. Callers keep it off the stack and reuse it.
pub struct Scratch {
    input: [u32; MAX_CODE_POINTS],
    output: [u32; MAX_CODE_POINTS],
    stored: [u8; MAX_STORED_BYTES],
    folded: [u8; MAX_FOLDED_BYTES],
}

impl Scratch {
    /// Zeroed scratch. Callers allocate it on the heap or in static memory.
    pub const fn new() -> Self {
        Self {
            input: [0; MAX_CODE_POINTS],
            output: [0; MAX_CODE_POINTS],
            stored: [0; MAX_STORED_BYTES],
            folded: [0; MAX_FOLDED_BYTES],
        }
    }
}

impl Default for Scratch {
    fn default() -> Self {
        Self::new()
    }
}

/// A parsed name borrowed from the input or its `Scratch`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParsedName<'a> {
    /// The stored NFC spelling.
    pub stored: &'a [u8],
    /// The comparison key, the catalog directory-entry key suffix.
    pub folded: &'a [u8],
}

/// A fixed-capacity code-point list over a scratch buffer.
struct CodePoints<'a> {
    buffer: &'a mut [u32; MAX_CODE_POINTS],
    len: usize,
}

impl<'a> CodePoints<'a> {
    fn new(buffer: &'a mut [u32; MAX_CODE_POINTS]) -> Self {
        Self { buffer, len: 0 }
    }

    fn push(&mut self, code_point: u32) -> Result<()> {
        let Some(slot) = self.buffer.get_mut(self.len) else {
            return Err(Error::Corrupt("normalized disk name is too long"));
        };
        *slot = code_point;
        self.len += 1;
        Ok(())
    }

    fn as_slice(&self) -> &[u32] {
        &self.buffer[..self.len]
    }
}

fn lookup_mapping(
    table: &'static [(u32, &'static [u32])],
    code_point: u32,
) -> Option<&'static [u32]> {
    let index = table
        .binary_search_by_key(&code_point, |(code_point, _)| *code_point)
        .ok()?;
    table.get(index).map(|(_, mapping)| *mapping)
}

fn combining_class(code_point: u32) -> u8 {
    let Ok(index) = name_tables::COMBINING_CLASSES
        .binary_search_by_key(&code_point, |(code_point, _)| *code_point)
    else {
        return 0;
    };
    name_tables::COMBINING_CLASSES
        .get(index)
        .map_or(0, |(_, class)| *class)
}

fn is_disallowed_code_point(code_point: u32) -> bool {
    name_tables::INVALID_CODE_POINT_RANGES
        .binary_search_by(|(start, end)| {
            if code_point < *start {
                Ordering::Greater
            } else if code_point > *end {
                Ordering::Less
            } else {
                Ordering::Equal
            }
        })
        .is_ok()
}

fn compose(first: u32, second: u32) -> Option<u32> {
    if (0x1100..0x1113).contains(&first) && (0x1161..0x1176).contains(&second) {
        return Some(0xAC00 + ((first - 0x1100) * 21 + second - 0x1161) * 28);
    }
    if (0xAC00..0xD7A4).contains(&first)
        && (first - 0xAC00).is_multiple_of(28)
        && (0x11A8..0x11C3).contains(&second)
    {
        return Some(first + second - 0x11A7);
    }

    let Ok(index) = name_tables::COMPOSITIONS
        .binary_search_by(|(left, right, _)| (*left, *right).cmp(&(first, second)))
    else {
        return None;
    };
    name_tables::COMPOSITIONS
        .get(index)
        .map(|(_, _, composed)| *composed)
}

/// Canonical decomposition. Table mappings are already fully decomposed
/// apart from Hangul, so the recursion stays shallow.
fn decompose(code_point: u32, output: &mut CodePoints<'_>) -> Result<()> {
    if let Some(mapping) = lookup_mapping(name_tables::CANONICAL_DECOMPOSITIONS, code_point) {
        for &part in mapping {
            decompose(part, output)?;
        }
        return Ok(());
    }

    if (0xAC00..0xD7A4).contains(&code_point) {
        let index = code_point - 0xAC00;
        output.push(0x1100 + index / 588)?;
        output.push(0x1161 + index % 588 / 28)?;
        if !index.is_multiple_of(28) {
            output.push(0x11A7 + index % 28)?;
        }
        return Ok(());
    }

    output.push(code_point)
}

/// NFC of `input` into `output`.
fn normalize_nfc(input: &[u32], output: &mut CodePoints<'_>) -> Result<()> {
    output.len = 0;
    for &code_point in input {
        decompose(code_point, output)?;
    }

    // Preserve the order of equal-class marks. MAX_CODE_POINTS bounds the
    // quadratic insertion sort, including decomposition and folding expansions.
    let ordered = &mut output.buffer[..output.len];
    for index in 0..ordered.len() {
        let class = combining_class(ordered[index]);
        let mut position = index;
        while class != 0 && position > 0 && combining_class(ordered[position - 1]) > class {
            ordered.swap(position, position - 1);
            position -= 1;
        }
    }

    // Composition only shrinks the list, so it runs in place: the write
    // position never passes the read position.
    let mut written = 0;
    let mut starter: Option<usize> = None;
    let mut last_class = 0;
    for read in 0..ordered.len() {
        let code_point = ordered[read];
        let class = combining_class(code_point);
        let combined = starter.and_then(|index| compose(ordered[index], code_point));
        if let Some(combined) = combined
            && (last_class == 0 || last_class < class)
        {
            let Some(starter_index) = starter else {
                return Err(Error::Corrupt("invalid normalization state"));
            };
            ordered[starter_index] = combined;
            continue;
        }

        if class == 0 {
            starter = Some(written);
        }
        ordered[written] = code_point;
        written += 1;
        last_class = class;
    }
    output.len = written;
    Ok(())
}

/// Full default case folding of `input` into `output`, not yet renormalized.
fn full_case_fold(input: &[u32], output: &mut CodePoints<'_>) -> Result<()> {
    output.len = 0;
    for &code_point in input {
        if let Some(mapping) = lookup_mapping(name_tables::CASE_FOLDINGS, code_point) {
            for &part in mapping {
                output.push(part)?;
            }
        } else {
            output.push(code_point)?;
        }
    }
    Ok(())
}

/// UTF-8 of `code_points` into `output`; the encoded length, or an error
/// when it does not fit.
fn encode_code_points(code_points: &[u32], output: &mut [u8]) -> Result<usize> {
    let mut len = 0;
    for &code_point in code_points {
        let Some(character) = char::from_u32(code_point) else {
            return Err(Error::Corrupt(
                "normalized disk name contains invalid Unicode",
            ));
        };
        let Some(destination) = output.get_mut(len..len + character.len_utf8()) else {
            return Err(Error::Corrupt("normalized disk name is too long"));
        };
        character.encode_utf8(destination);
        len += character.len_utf8();
    }
    Ok(len)
}

/// The stored spelling and comparison key of `text`. Both live in `scratch`.
fn normalize_and_fold<'s>(text: &str, scratch: &'s mut Scratch) -> Result<(&'s [u8], &'s [u8])> {
    let Scratch {
        input,
        output,
        stored,
        folded,
    } = scratch;
    let mut code_points = CodePoints::new(input);
    for character in text.chars() {
        code_points.push(u32::from(character))?;
    }
    let mut normalized = CodePoints::new(output);
    normalize_nfc(code_points.as_slice(), &mut normalized)?;
    let stored_len = encode_code_points(normalized.as_slice(), stored)?;
    // Fold into the input buffer, then renormalize into the output buffer.
    full_case_fold(normalized.as_slice(), &mut code_points)?;
    normalize_nfc(code_points.as_slice(), &mut normalized)?;
    let folded_len = encode_code_points(normalized.as_slice(), folded)?;
    Ok((&stored[..stored_len], &folded[..folded_len]))
}

const FORBIDDEN: Error =
    Error::InvalidName("name is empty, reserved, or contains forbidden characters");

/// Parse and validate a name from an adapter's decoded input string.
///
/// Performs NFC normalization, full case folding, and the pinned
/// filename-profile checks in FORMAT.md. ASCII names, the common case, touch
/// only the comparison-key buffer of `scratch`.
pub fn parse<'a>(input: &'a str, scratch: &'a mut Scratch) -> Result<ParsedName<'a>> {
    // NFC leaves ASCII unchanged and the pinned case folding maps only
    // A-Z within it, so an ASCII name needs no code-point pipeline.
    if !input.is_ascii() {
        return parse_unicode(input, scratch);
    }
    if input
        .bytes()
        .any(|byte| is_disallowed_code_point(u32::from(byte)))
    {
        return Err(Error::InvalidName("disallowed Unicode code point"));
    }
    if input.len() > MAX_STORED_BYTES {
        return Err(Error::TooLong("name is too long"));
    }
    if input.is_empty() || validate_disk_name_characters(input.as_bytes()).is_err() {
        return Err(FORBIDDEN);
    }
    let folded = &mut scratch.folded[..input.len()];
    folded.copy_from_slice(input.as_bytes());
    folded.make_ascii_lowercase();
    Ok(ParsedName {
        stored: input.as_bytes(),
        folded,
    })
}

/// `parse` through the full code-point pipeline, whatever the input.
pub fn parse_unicode<'a>(input: &str, scratch: &'a mut Scratch) -> Result<ParsedName<'a>> {
    if input
        .chars()
        .any(|character| is_disallowed_code_point(u32::from(character)))
    {
        return Err(Error::InvalidName("disallowed Unicode code point"));
    }
    let (stored, folded) =
        normalize_and_fold(input, scratch).map_err(|_| Error::TooLong("name is too long"))?;
    if stored.is_empty() || validate_disk_name_characters(stored).is_err() {
        return Err(FORBIDDEN);
    }
    Ok(ParsedName { stored, folded })
}

/// Whether Windows reserves this comparison key, including names with
/// extensions.
pub fn is_reserved_device_name(folded: &[u8]) -> bool {
    let mut end = folded
        .iter()
        .position(|byte| *byte == b'.')
        .unwrap_or(folded.len());
    while end > 0 && folded[end - 1] == b' ' {
        end -= 1;
    }
    let base = &folded[..end];
    if matches!(
        base,
        b"con" | b"prn" | b"aux" | b"nul" | b"conin$" | b"conout$"
    ) {
        return true;
    }
    if base.len() == 4
        && (base.starts_with(b"com") || base.starts_with(b"lpt"))
        && base[3].is_ascii_digit()
        && base[3] != b'0'
    {
        return true;
    }
    matches!(
        base,
        b"com\xC2\xB9"
            | b"com\xC2\xB2"
            | b"com\xC2\xB3"
            | b"lpt\xC2\xB9"
            | b"lpt\xC2\xB2"
            | b"lpt\xC2\xB3"
    )
}

/// Validates a stored directory name and its pinned comparison key.
pub fn validate_disk_name(stored: &[u8], folded: &[u8], scratch: &mut Scratch) -> Result<()> {
    if !(1..=MAX_STORED_BYTES).contains(&stored.len()) {
        return Err(Error::Corrupt("stored disk name length"));
    }
    if folded.len() > MAX_FOLDED_BYTES {
        return Err(Error::Corrupt("folded disk name length"));
    }
    let stored_text = str::from_utf8(stored).map_err(|_| Error::Corrupt("stored name UTF-8"))?;
    str::from_utf8(folded).map_err(|_| Error::Corrupt("folded name UTF-8"))?;
    if stored.is_ascii() {
        validate_ascii_disk_name(stored, folded)
    } else {
        validate_unicode_disk_name(stored_text, folded, scratch)
    }
}

/// The checks common to every disk name, and all a reader needs before it
/// hands a stored name to a host: no `.`, `..`, or path separator.
pub fn validate_disk_name_characters(stored: &[u8]) -> Result<()> {
    if stored == b"." || stored == b".." {
        return Err(Error::Corrupt("dot disk name"));
    }
    if stored.contains(&b'/') {
        return Err(Error::Corrupt("disk name contains a path separator"));
    }
    Ok(())
}

/// `validate_unicode_disk_name` for an ASCII name, without the code-point
/// pipeline: its NFC form is itself and its folded key its lowercase form.
fn validate_ascii_disk_name(stored: &[u8], folded: &[u8]) -> Result<()> {
    validate_disk_name_characters(stored)?;
    if stored
        .iter()
        .any(|byte| is_disallowed_code_point(u32::from(*byte)))
    {
        return Err(Error::Corrupt("disallowed Unicode code point in disk name"));
    }
    if folded.len() != stored.len()
        || stored
            .iter()
            .zip(folded)
            .any(|(stored, folded)| stored.to_ascii_lowercase() != *folded)
    {
        return Err(Error::Corrupt("disk name comparison key mismatch"));
    }
    Ok(())
}

fn validate_unicode_disk_name(
    stored_text: &str,
    folded: &[u8],
    scratch: &mut Scratch,
) -> Result<()> {
    validate_disk_name_characters(stored_text.as_bytes())?;
    if stored_text
        .chars()
        .any(|character| is_disallowed_code_point(u32::from(character)))
    {
        return Err(Error::Corrupt("disallowed Unicode code point in disk name"));
    }
    let (expected_stored, expected_folded) = normalize_and_fold(stored_text, scratch)?;
    if expected_stored != stored_text.as_bytes() {
        return Err(Error::Corrupt("stored disk name is not NFC"));
    }
    if expected_folded != folded {
        return Err(Error::Corrupt("disk name comparison key mismatch"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::boxed::Box;
    use std::format;
    use std::string::{String, ToString};
    use std::vec::Vec;

    use super::*;

    fn scratch() -> Box<Scratch> {
        Box::new(Scratch::new())
    }

    fn nfc(input: &[u32]) -> Vec<u32> {
        let mut scratch = scratch();
        let Scratch {
            input: a,
            output: b,
            ..
        } = &mut *scratch;
        let mut source = CodePoints::new(a);
        for &code_point in input {
            source.push(code_point).unwrap();
        }
        let mut output = CodePoints::new(b);
        normalize_nfc(source.as_slice(), &mut output).expect("pinned normalization input");
        output.as_slice().to_vec()
    }

    fn fold(input: &[u32]) -> Vec<u32> {
        let mut scratch = scratch();
        let Scratch {
            input: a,
            output: b,
            ..
        } = &mut *scratch;
        let mut folded = CodePoints::new(a);
        full_case_fold(input, &mut folded).unwrap();
        let mut output = CodePoints::new(b);
        normalize_nfc(folded.as_slice(), &mut output).unwrap();
        output.as_slice().to_vec()
    }

    fn validate(stored: &[u8], folded: &[u8]) -> Result<()> {
        validate_disk_name(stored, folded, &mut scratch())
    }

    /// The ASCII shortcuts answer exactly as the code-point pipeline does,
    /// error messages included: every one- and two-byte ASCII name, reserved
    /// device names, boundary lengths and pseudo-random names, each checked
    /// against its correct key and against corrupted ones.
    #[test]
    fn ascii_names_take_the_same_path() {
        let mut names: Vec<String> = Vec::new();
        for a in 0u8..128 {
            names.push((a as char).to_string());
            for b in 0u8..128 {
                names.push([a as char, b as char].iter().collect());
            }
        }
        for special in [
            "CON", "con.txt", "Com1", "LPT9 .x", "nul ", "Conin$", "a.b", "...",
        ] {
            names.push(special.into());
        }
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        for len in [3, 17, 254, 255, 256, 260].into_iter().cycle().take(3000) {
            names.push(
                (0..len)
                    .map(|_| {
                        state ^= state << 13;
                        state ^= state >> 7;
                        state ^= state << 17;
                        char::from(b" AZaz09._-~()CONLPT"[(state % 19) as usize])
                    })
                    .collect(),
            );
        }
        let (mut first, mut second) = (scratch(), scratch());
        for name in &names {
            let fast = parse(name, &mut first);
            let slow = parse_unicode(name, &mut second);
            assert_eq!(format!("{fast:?}"), format!("{slow:?}"), "parse {name:?}");
            let stored = name.as_bytes();
            if stored.is_empty() || stored.len() > 255 {
                continue;
            }
            let lower = name.to_ascii_lowercase().into_bytes();
            let mut wrong = lower.clone();
            wrong[0] ^= 0x20;
            for folded in [lower.clone(), wrong, lower[1..].to_vec(), stored.to_vec()] {
                let fast = validate_ascii_disk_name(stored, &folded);
                let slow = validate_unicode_disk_name(name, &folded, &mut first);
                assert_eq!(
                    format!("{fast:?}"),
                    format!("{slow:?}"),
                    "validate {name:?} against {folded:?}"
                );
            }
        }
    }

    #[test]
    fn parse_normalizes_and_folds_like_validate_disk_name() {
        let mut scratch = scratch();
        let name = parse("Straße", &mut scratch).unwrap();
        assert_eq!(name.stored, "Straße".as_bytes());
        assert_eq!(name.folded, "strasse".as_bytes());
        let name = parse("e\u{301}.TXT", &mut scratch).unwrap();
        assert_eq!(name.stored, "é.TXT".as_bytes());
        assert_eq!(name.folded, "é.txt".as_bytes());
    }

    #[test]
    fn parse_rejects_unrepresentable_names() {
        let mut scratch = scratch();
        for input in ["", ".", "..", "a/b"] {
            assert!(
                parse(input, &mut scratch).is_err(),
                "unexpectedly accepted {input:?}"
            );
        }
    }

    fn parse_code_points(field: &str) -> Vec<u32> {
        field
            .split_whitespace()
            .map(|code_point| {
                u32::from_str_radix(code_point, 16).expect("pinned Unicode code point")
            })
            .collect()
    }

    #[test]
    fn pinned_accepted_names_have_expected_storage_and_key() {
        let cases = [
            ("notes.txt", "notes.txt", "notes.txt"),
            ("Notes.TXT", "Notes.TXT", "notes.txt"),
            ("e\u{301}.txt", "é.txt", "é.txt"),
            ("Straße", "Straße", "strasse"),
            ("İ", "İ", "i\u{307}"),
            ("ı", "ı", "ı"),
            ("ﬃ", "ﬃ", "ffi"),
            ("Ａ", "Ａ", "ａ"),
            ("Σςσ", "Σςσ", "σσσ"),
            ("각", "각", "각"),
            ("👩‍💻", "👩‍💻", "👩‍💻"),
            ("\u{E000}", "\u{E000}", "\u{E000}"),
            ("COM10.txt", "COM10.txt", "com10.txt"),
            ("what?", "what?", "what?"),
            ("a:b", "a:b", "a:b"),
            ("a\\b", "a\\b", "a\\b"),
            ("name.", "name.", "name."),
            ("name ", "name ", "name "),
            ("CON", "CON", "con"),
        ];
        let mut scratch = scratch();
        for (input, expected_stored, expected_folded) in cases {
            let result = validate(expected_stored.as_bytes(), expected_folded.as_bytes());
            assert!(result.is_ok(), "{input:?}: {result:?}");
            let parsed = parse(input, &mut scratch).unwrap();
            assert_eq!(parsed.stored, expected_stored.as_bytes(), "{input:?}");
            assert_eq!(parsed.folded, expected_folded.as_bytes(), "{input:?}");
        }
    }

    #[test]
    fn pinned_rejected_names_are_corrupt() {
        let cases = [
            ("", ""),
            (".", "."),
            ("..", ".."),
            ("a/b", "a/b"),
            ("a\u{0}b", "a\u{0}b"),
            ("a\u{7F}b", "a\u{7F}b"),
            ("a\u{85}b", "a\u{85}b"),
            ("\u{FDD0}", "\u{FDD0}"),
            ("\u{378}", "\u{378}"),
        ];
        for (stored, folded) in cases {
            assert!(
                validate(stored.as_bytes(), folded.as_bytes()).is_err(),
                "unexpectedly accepted {stored:?}"
            );
        }
    }

    #[test]
    fn invalid_utf8_is_rejected_without_panicking() {
        assert!(validate(&[0xFF], b"x").is_err());
    }

    #[test]
    fn non_nfc_storage_is_rejected() {
        assert!(validate("e\u{301}".as_bytes(), "é".as_bytes()).is_err());
    }

    #[test]
    fn wrong_folded_key_is_rejected() {
        assert!(validate(b"Notes.TXT", b"Notes.TXT").is_err());
    }

    #[test]
    fn pinned_normalization_test_rows_match_nfc() {
        let mut rows = 0;
        for (line_number, line) in
            include_str!("../../testdata/unicode/16.0.0/NormalizationTest.txt")
                .lines()
                .enumerate()
        {
            let text = line.split('#').next().unwrap_or_default().trim();
            if text.is_empty() || text.starts_with('@') {
                continue;
            }
            let columns: Vec<Vec<u32>> = text.split(';').take(5).map(parse_code_points).collect();
            assert_eq!(columns.len(), 5, "normalization row {line_number}");
            for (index, input) in columns.iter().enumerate() {
                let expected = if index < 3 { &columns[1] } else { &columns[3] };
                assert_eq!(nfc(input), *expected, "normalization row {line_number}");
            }
            rows += 1;
        }
        assert_eq!(rows, 19_965);
    }

    #[test]
    fn pinned_case_folding_mappings_match_nfc() {
        let mut mappings = 0;
        for (line_number, line) in include_str!("../../testdata/unicode/16.0.0/CaseFolding.txt")
            .lines()
            .enumerate()
        {
            let text = line.split('#').next().unwrap_or_default().trim();
            if text.is_empty() {
                continue;
            }
            let mut fields = text.split(';').map(str::trim);
            let code_point = fields.next().expect("case-fold code point");
            let status = fields.next().expect("case-fold status");
            let mapping = fields.next().expect("case-fold mapping");
            if status != "C" && status != "F" {
                continue;
            }
            let expected = nfc(&parse_code_points(mapping));
            assert_eq!(
                fold(&parse_code_points(code_point)),
                expected,
                "case-fold row {line_number}"
            );
            mappings += 1;
        }
        assert_eq!(mappings, name_tables::CASE_FOLDINGS.len());
    }
}
