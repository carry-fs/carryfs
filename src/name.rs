use std::cmp::Ordering;
use std::str;

use crate::{Error, Result, name_tables};

const MAX_STORED_BYTES: usize = 255;
const MAX_FOLDED_BYTES: usize = 765;
// Unicode 16.0 canonical decomposition expands one scalar to at most four
// scalars; full folding expands it to at most three. The input is at most 255
// UTF-8 bytes, so this fixed bound keeps disk-derived allocations bounded.
const MAX_CODE_POINTS: usize = 4_096;

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

fn push_code_point(output: &mut Vec<u32>, code_point: u32) -> Result<()> {
    if output.len() >= MAX_CODE_POINTS {
        return Err(Error::Corrupt("normalized disk name is too long"));
    }
    output.push(code_point);
    Ok(())
}

fn decompose(code_point: u32, output: &mut Vec<u32>) -> Result<()> {
    if let Some(mapping) = lookup_mapping(name_tables::CANONICAL_DECOMPOSITIONS, code_point) {
        for &part in mapping {
            decompose(part, output)?;
        }
        return Ok(());
    }

    if (0xAC00..0xD7A4).contains(&code_point) {
        let index = code_point - 0xAC00;
        push_code_point(output, 0x1100 + index / 588)?;
        push_code_point(output, 0x1161 + index % 588 / 28)?;
        if !index.is_multiple_of(28) {
            push_code_point(output, 0x11A7 + index % 28)?;
        }
        return Ok(());
    }

    push_code_point(output, code_point)
}

fn normalize_nfc(input: &[u32]) -> Result<Vec<u32>> {
    let mut ordered = Vec::with_capacity(input.len().saturating_mul(4).min(MAX_CODE_POINTS));
    for &code_point in input {
        decompose(code_point, &mut ordered)?;
    }

    // Preserve the order of equal-class marks. MAX_CODE_POINTS bounds the
    // quadratic insertion sort, including decomposition and folding expansions.
    for index in 0..ordered.len() {
        let class = combining_class(ordered[index]);
        let mut position = index;
        while class != 0 && position > 0 && combining_class(ordered[position - 1]) > class {
            ordered.swap(position, position - 1);
            position -= 1;
        }
    }

    let mut result = Vec::with_capacity(ordered.len());
    let mut starter = None;
    let mut last_class = 0;
    for code_point in ordered {
        let class = combining_class(code_point);
        let combined = starter
            .and_then(|index| result.get(index).copied())
            .and_then(|first| compose(first, code_point));
        if let Some(combined) = combined
            && (last_class == 0 || last_class < class)
        {
            let Some(starter_index) = starter else {
                return Err(Error::Corrupt("invalid normalization state"));
            };
            let Some(slot) = result.get_mut(starter_index) else {
                return Err(Error::Corrupt("invalid normalization state"));
            };
            *slot = combined;
            continue;
        }

        if class == 0 {
            starter = Some(result.len());
        }
        push_code_point(&mut result, code_point)?;
        last_class = class;
    }
    Ok(result)
}

fn full_case_fold(input: &[u32]) -> Result<Vec<u32>> {
    let mut folded = Vec::with_capacity(input.len().saturating_mul(3).min(MAX_CODE_POINTS));
    for &code_point in input {
        if let Some(mapping) = lookup_mapping(name_tables::CASE_FOLDINGS, code_point) {
            for &part in mapping {
                push_code_point(&mut folded, part)?;
            }
        } else {
            push_code_point(&mut folded, code_point)?;
        }
    }
    normalize_nfc(&folded)
}

fn encode_code_points(code_points: &[u32], max_bytes: usize) -> Result<Vec<u8>> {
    let mut encoded = Vec::with_capacity(code_points.len().min(max_bytes));
    for &code_point in code_points {
        let Some(character) = char::from_u32(code_point) else {
            return Err(Error::Corrupt(
                "normalized disk name contains invalid Unicode",
            ));
        };
        let mut bytes = [0; 4];
        let character_bytes = character.encode_utf8(&mut bytes).as_bytes();
        if character_bytes.len() > max_bytes.saturating_sub(encoded.len()) {
            return Err(Error::Corrupt("normalized disk name is too long"));
        }
        encoded.extend_from_slice(character_bytes);
    }
    Ok(encoded)
}

fn is_reserved_device_name(folded: &[u8]) -> bool {
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

/// A validated, normalized directory-entry name.
///
/// Holds the NFC stored spelling and the folded comparison key used as the
/// catalog directory-entry key suffix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Name {
    pub(crate) stored: Vec<u8>,
    pub(crate) folded: Vec<u8>,
}

impl Name {
    /// Parse and validate a name from an adapter's decoded input string.
    ///
    /// Performs NFC normalization, full case folding, and the pinned
    /// filename-profile checks in FORMAT.md. Native adapters decode their own
    /// input string encoding and then call this same parser.
    pub fn parse(input: &str) -> Result<Self> {
        // NFC leaves ASCII unchanged and the pinned case folding maps only
        // A-Z within it, so an ASCII name needs no code-point pipeline. Every
        // FUSE lookup, create and removal parses a name.
        if input.is_ascii() {
            if input
                .bytes()
                .any(|byte| is_disallowed_code_point(u32::from(byte)))
            {
                return Err(Error::InvalidName("disallowed Unicode code point"));
            }
            if input.len() > MAX_STORED_BYTES {
                return Err(Error::TooLong("name is too long"));
            }
            let stored = input.as_bytes().to_vec();
            let folded = input.to_ascii_lowercase().into_bytes();
            validate_disk_name(&stored, &folded).map_err(|_| {
                Error::InvalidName("name is empty, reserved, or contains forbidden characters")
            })?;
            return Ok(Self { stored, folded });
        }
        Self::parse_unicode(input)
    }

    fn parse_unicode(input: &str) -> Result<Self> {
        let mut code_points = Vec::with_capacity(input.len().min(MAX_CODE_POINTS));
        for character in input.chars() {
            let code_point = u32::from(character);
            if is_disallowed_code_point(code_point) {
                return Err(Error::InvalidName("disallowed Unicode code point"));
            }
            push_code_point(&mut code_points, code_point)
                .map_err(|_| Error::TooLong("name is too long"))?;
        }
        let normalized =
            normalize_nfc(&code_points).map_err(|_| Error::TooLong("name is too long"))?;
        let stored = encode_code_points(&normalized, MAX_STORED_BYTES)
            .map_err(|_| Error::TooLong("name is too long"))?;
        let folded_code_points =
            full_case_fold(&normalized).map_err(|_| Error::TooLong("name is too long"))?;
        let folded = encode_code_points(&folded_code_points, MAX_FOLDED_BYTES)
            .map_err(|_| Error::TooLong("name is too long"))?;
        validate_disk_name(&stored, &folded).map_err(|_| {
            Error::InvalidName("name is empty, reserved, or contains forbidden characters")
        })?;
        Ok(Self { stored, folded })
    }

    /// The stored NFC spelling, as bytes.
    pub fn stored(&self) -> &[u8] {
        &self.stored
    }

    /// Whether Windows reserves this name, including names with extensions.
    pub fn is_windows_device_name(&self) -> bool {
        is_reserved_device_name(&self.folded)
    }
}

/// Validates a stored directory name and its pinned comparison key.
pub(crate) fn validate_disk_name(stored: &[u8], folded: &[u8]) -> Result<()> {
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
        validate_unicode_disk_name(stored, stored_text, folded)
    }
}

/// The checks common to every disk name.
fn validate_disk_name_characters(stored: &[u8]) -> Result<()> {
    if stored == b"." || stored == b".." {
        return Err(Error::Corrupt("dot disk name"));
    }
    if stored.contains(&b'/') {
        return Err(Error::Corrupt("disk name contains a path separator"));
    }
    Ok(())
}

/// `validate_unicode_disk_name` for an ASCII name, without allocating: its
/// NFC form is itself and its folded key its lowercase form.
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

fn validate_unicode_disk_name(stored: &[u8], stored_text: &str, folded: &[u8]) -> Result<()> {
    validate_disk_name_characters(stored)?;
    let mut code_points = Vec::with_capacity(stored_text.len());
    for character in stored_text.chars() {
        let code_point = u32::from(character);
        if is_disallowed_code_point(code_point) {
            return Err(Error::Corrupt("disallowed Unicode code point in disk name"));
        }
        push_code_point(&mut code_points, code_point)?;
    }

    let normalized = normalize_nfc(&code_points)?;
    if encode_code_points(&normalized, MAX_STORED_BYTES)? != stored {
        return Err(Error::Corrupt("stored disk name is not NFC"));
    }

    let folded_code_points = full_case_fold(&normalized)?;
    let expected_folded = encode_code_points(&folded_code_points, MAX_FOLDED_BYTES)?;
    if expected_folded != folded {
        return Err(Error::Corrupt("disk name comparison key mismatch"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        Name, full_case_fold, normalize_nfc, validate_ascii_disk_name, validate_disk_name,
        validate_unicode_disk_name,
    };
    use crate::name_tables;

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
            names.push(special.to_owned());
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
        for name in &names {
            let fast = Name::parse(name).map(|name| (name.stored, name.folded));
            let slow = Name::parse_unicode(name).map(|name| (name.stored, name.folded));
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
                let slow = validate_unicode_disk_name(stored, name, &folded);
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
        let name = Name::parse("Straße").unwrap();
        assert_eq!(name.stored, "Straße".as_bytes());
        assert_eq!(name.folded, "strasse".as_bytes());
    }

    #[test]
    fn parse_rejects_unrepresentable_names() {
        for input in ["", ".", "..", "a/b"] {
            assert!(
                Name::parse(input).is_err(),
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
            ("각", "각", "각"),
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
        for (input, expected_stored, expected_folded) in cases {
            let result = validate_disk_name(expected_stored.as_bytes(), expected_folded.as_bytes());
            assert!(result.is_ok(), "{input:?}: {result:?}");
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
                validate_disk_name(stored.as_bytes(), folded.as_bytes()).is_err(),
                "unexpectedly accepted {stored:?}"
            );
        }
    }

    #[test]
    fn invalid_utf8_is_rejected_without_panicking() {
        assert!(validate_disk_name(&[0xFF], b"x").is_err());
    }

    #[test]
    fn non_nfc_storage_is_rejected() {
        assert!(validate_disk_name("e\u{301}".as_bytes(), "é".as_bytes()).is_err());
    }

    #[test]
    fn wrong_folded_key_is_rejected() {
        assert!(validate_disk_name(b"Notes.TXT", b"Notes.TXT").is_err());
    }

    #[test]
    fn pinned_normalization_test_rows_match_nfc() {
        let mut rows = 0;
        for (line_number, line) in include_str!("../testdata/unicode/16.0.0/NormalizationTest.txt")
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
                let normalized = normalize_nfc(input).expect("pinned normalization input");
                assert_eq!(normalized, *expected, "normalization row {line_number}");
            }
            rows += 1;
        }
        assert_eq!(rows, 19_965);
    }

    #[test]
    fn pinned_case_folding_mappings_match_nfc() {
        let mut mappings = 0;
        for (line_number, line) in include_str!("../testdata/unicode/16.0.0/CaseFolding.txt")
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
            let source = parse_code_points(code_point);
            let expected =
                normalize_nfc(&parse_code_points(mapping)).expect("pinned case-fold mapping");
            let folded = full_case_fold(&source).expect("pinned case-fold source");
            assert_eq!(folded, expected, "case-fold row {line_number}");
            mappings += 1;
        }
        assert_eq!(mappings, name_tables::CASE_FOLDINGS.len());
    }
}
