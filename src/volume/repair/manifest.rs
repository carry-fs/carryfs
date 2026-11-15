//! Recovery provenance stored inside a reconstructed image and in its sidecar.

use std::fmt::Write;

use crate::{ExtractionSummary, Finding, Name, ObjectId};

#[derive(Clone)]
pub(super) struct Damage {
    pub offset: Option<u64>,
    pub length: u64,
    pub error: String,
}

impl Damage {
    pub fn finding(finding: &Finding<'_>) -> Self {
        Self {
            offset: finding.offset,
            length: finding.length,
            error: finding.error.to_string(),
        }
    }
}

pub(super) struct MissingRange {
    pub object: u64,
    pub damage: Damage,
}

pub(super) struct FileRecord {
    pub path: Vec<Name>,
    pub source: ObjectId,
    pub destination: ObjectId,
    pub size: u64,
    pub copied_bytes: u64,
    pub verified_bytes: u64,
    pub source_damage: Vec<Damage>,
    pub destination_damage: Vec<Damage>,
}

pub(super) struct Manifest {
    pub source_uuid: [u8; 16],
    pub source_generation: u64,
    pub source_bytes: u64,
    pub destination_uuid: [u8; 16],
    pub metadata_complete: bool,
    pub actions: Vec<String>,
    pub files: Vec<FileRecord>,
    pub verification: &'static str,
}

impl Manifest {
    pub fn json(&self, state: &str, summary: &ExtractionSummary) -> String {
        let mut out = String::from("{\"version\":1,\"state\":");
        string(&mut out, state);
        out.push_str(",\"source\":{\"uuid\":");
        uuid(&mut out, &self.source_uuid);
        write!(
            out,
            ",\"generation\":{},\"byte_len\":{},\"metadata_complete\":{}",
            self.source_generation, self.source_bytes, self.metadata_complete
        )
        .unwrap();
        out.push_str("},\"destination\":{\"uuid\":");
        uuid(&mut out, &self.destination_uuid);
        out.push_str("},\"verification\":");
        string(&mut out, self.verification);
        write!(out, ",\"summary\":{{\"objects\":{},\"files\":{},\"verified_source_bytes\":{},\"unreadable_source_ranges\":{},\"enumeration_complete\":{}}},\"repair_plan\":[", summary.objects, summary.files, summary.verified_bytes, summary.unreadable_ranges, summary.complete).unwrap();
        for (index, action) in self.actions.iter().enumerate() {
            if index != 0 {
                out.push(',');
            }
            string(&mut out, action);
        }
        out.push_str("],\"files\":[");
        for (index, file) in self.files.iter().enumerate() {
            if index != 0 {
                out.push(',');
            }
            out.push_str("{\"path\":[");
            for (index, name) in file.path.iter().enumerate() {
                if index != 0 {
                    out.push(',');
                }
                // Names have already passed the on-disk UTF-8/name validator.
                string(
                    &mut out,
                    std::str::from_utf8(name.stored()).expect("validated name"),
                );
            }
            let status = if !file.destination_damage.is_empty() {
                "damaged"
            } else if !file.source_damage.is_empty() {
                "partial"
            } else if !self.metadata_complete {
                "unknown"
            } else {
                "complete"
            };
            write!(out, "],\"source_object\":{},\"destination_object\":{},\"size\":{},\"copied_bytes\":{},\"destination_verified_bytes\":{},\"status\":", file.source.get(), file.destination.get(), file.size, file.copied_bytes, file.verified_bytes).unwrap();
            string(&mut out, status);
            out.push_str(",\"source_damage\":");
            damage(&mut out, &file.source_damage);
            out.push_str(",\"destination_damage\":");
            damage(&mut out, &file.destination_damage);
            out.push('}');
        }
        out.push_str("]}\n");
        out
    }
}

fn uuid(out: &mut String, uuid: &[u8; 16]) {
    out.push('"');
    for byte in uuid {
        write!(out, "{byte:02x}").unwrap();
    }
    out.push('"');
}

fn damage(out: &mut String, ranges: &[Damage]) {
    out.push('[');
    for (index, range) in ranges.iter().enumerate() {
        if index != 0 {
            out.push(',');
        }
        out.push_str("{\"offset\":");
        match range.offset {
            Some(offset) => write!(out, "{offset}").unwrap(),
            None => out.push_str("null"),
        }
        write!(out, ",\"length\":{},\"error\":", range.length).unwrap();
        string(out, &range.error);
        out.push('}');
    }
    out.push(']');
}

fn string(out: &mut String, value: &str) {
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if control < ' ' => write!(out, "\\u{:04x}", control as u32).unwrap(),
            other => out.push(other),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    #[test]
    fn json_strings_preserve_unicode_and_escape_controls() {
        let mut output = String::new();
        super::string(&mut output, "café\"\\\n\r\t\0\u{8}");
        assert_eq!(output, "\"café\\\"\\\\\\n\\r\\t\\u0000\\u0008\"");
    }
}
