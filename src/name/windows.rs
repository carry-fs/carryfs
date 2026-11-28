//! The reversible Windows spelling of stored names, FORMAT.md section 7. The
//! Windows adapter shows names this way, and extraction on Windows writes them.

use super::Name;
use crate::{Error, Result};

const ESCAPE: char = '\u{E000}';
const RESERVED_PREFIX: char = '\u{E001}';

/// Characters Windows forbids, in Services for Macintosh order. Each shows as
/// U+F020 plus its index, the stand-ins Samba and the macOS SMB client also
/// use, so copies made on Windows can regain the original name there. Space
/// and period are forbidden only at the end of a name.
const SFM_ORIGINALS: &str = "\"*:<>?\\| .";
const SFM_FIRST: u32 = 0xF020;

const NOT_AN_ALIAS: Error = Error::InvalidName("not the Windows spelling of a name");

fn sfm(character: char) -> Option<char> {
    let index = u32::try_from(SFM_ORIGINALS.find(character)?).ok()?;
    char::from_u32(SFM_FIRST + index)
}

fn from_sfm(character: char) -> Option<char> {
    let index = u32::from(character).checked_sub(SFM_FIRST)?;
    SFM_ORIGINALS.chars().nth(usize::try_from(index).ok()?)
}

/// The fullwidth form of a printable ASCII character.
fn fullwidth(character: char) -> Result<char> {
    char::from_u32(u32::from(character) + 0xFEE0).ok_or(Error::InvalidName(
        "a reserved device name starts with an unprintable character",
    ))
}

/// The reserved device name that `text` spells with a fullwidth first character.
fn device_name(text: &str) -> Option<Name> {
    let mut characters = text.chars();
    let first = characters
        .next()
        .filter(|character| ('\u{FF01}'..='\u{FF5E}').contains(character))?;
    let ascii = char::from_u32(u32::from(first) - 0xFEE0)?;
    Name::parse(&format!("{ascii}{}", characters.as_str()))
        .ok()
        .filter(Name::is_windows_device_name)
}

/// `Name::windows_alias` over text that may not be NFC yet, so decoding can
/// compare a caller's decomposed spelling with its own encoding.
fn encode(text: &str, reserved: bool) -> Result<String> {
    let mut output = String::with_capacity(text.len());
    if reserved
        && !text
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii())
    {
        output.push(RESERVED_PREFIX);
    }
    for (index, character) in text.char_indices() {
        let last = index + character.len_utf8() == text.len();
        if index == 0 && reserved && character.is_ascii() {
            output.push(fullwidth(character)?);
        } else if let Some(substitute) =
            sfm(character).filter(|_| last || !matches!(character, ' ' | '.'))
        {
            output.push(substitute);
        } else {
            if matches!(character, ESCAPE | RESERVED_PREFIX)
                || from_sfm(character).is_some()
                || (index == 0 && device_name(text).is_some())
            {
                output.push(ESCAPE);
            }
            output.push(character);
        }
    }
    debug_assert!(output.encode_utf16().count() <= 255);
    Ok(output)
}

impl Name {
    /// The name as Windows can show it: a directly reversible component
    /// within 255 UTF-16 units. Names Windows accepts are unchanged.
    pub fn windows_alias(&self) -> Result<String> {
        let text = std::str::from_utf8(&self.stored)
            .map_err(|_| Error::Corrupt("stored name is not UTF-8"))?;
        encode(text, self.is_windows_device_name())
    }

    /// Decode a Windows-visible component. Only the spelling `windows_alias`
    /// produces is accepted, so raw forbidden spellings such as `what?` or
    /// `CON` are invalid.
    pub fn from_windows_alias(text: &str) -> Result<Self> {
        let mut output = String::with_capacity(text.len());
        let mut characters = text.chars();
        if text.starts_with(RESERVED_PREFIX) {
            characters.next();
        }
        while let Some(character) = characters.next() {
            output.push(match character {
                ESCAPE => characters.next().ok_or(NOT_AN_ALIAS)?,
                _ => from_sfm(character).unwrap_or(character),
            });
        }
        // Checked first: the fullwidth spelling of a 255-byte device name is too
        // long to parse as a literal name.
        if let Some(device) = device_name(&output)
            && device.windows_alias()? == text
        {
            return Ok(device);
        }
        // Compare before NFC normalization: a decomposed spelling is canonical
        // input for the name it normalizes to.
        let name = Name::parse(&output)?;
        if encode(&output, name.is_windows_device_name())? != text {
            return Err(NOT_AN_ALIAS);
        }
        Ok(name)
    }
}

/// A stored symlink target as a Windows relative path: each component in its
/// Windows spelling, joined by `\`.
pub fn windows_symlink_target(target: &str) -> Result<String> {
    target
        .split('/')
        .map(|part| match part {
            "" | "." | ".." => Ok(part.to_owned()),
            _ => Name::parse(part)?.windows_alias(),
        })
        .collect::<Result<Vec<_>>>()
        .map(|parts| parts.join("\\"))
}
