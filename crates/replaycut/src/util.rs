//! Small helpers shared by several modules.

use std::io;
use std::path::Path;
use std::time::SystemTime;

use chrono::{DateTime, Local};

const LOCAL_FORMAT: &str = "%Y-%m-%dT%H:%M:%S";

/// Local time without offset, as the API contract requires (`2026-09-04T11:38:35`).
pub fn now_local() -> String {
    Local::now().format(LOCAL_FORMAT).to_string()
}

pub fn system_time_local(t: SystemTime) -> String {
    DateTime::<Local>::from(t).format(LOCAL_FORMAT).to_string()
}

/// Write via a temporary file and rename, so a crash never leaves a half-written state file.
pub fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)
}

/// Percent-encode a clip base for `/media/<base>.mp4`, RFC 3986 unreserved
/// characters excepted (matches `[uri]::EscapeDataString` of the 1.4 service).
pub fn encode_path_segment(s: &str) -> String {
    use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
    const SET: &AsciiSet = &NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'.')
        .remove(b'_')
        .remove(b'~');
    utf8_percent_encode(s, SET).to_string()
}

/// Title normalisation of the contract: CR/LF/TAB become spaces, trimmed, at most 80 characters.
pub fn normalize_title(name: &str) -> String {
    let replaced: String = name
        .chars()
        .map(|c| {
            if matches!(c, '\r' | '\n' | '\t') {
                ' '
            } else {
                c
            }
        })
        .collect();
    replaced.trim().chars().take(80).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_have_contract_shape() {
        let s = now_local();
        assert_eq!(s.len(), 19);
        assert_eq!(&s[10..11], "T");
    }

    #[test]
    fn encoding_matches_dotnet_escape_data_string() {
        assert_eq!(
            encode_path_segment("Replay 2026-09-04 11-40-00"),
            "Replay%202026-09-04%2011-40-00"
        );
        assert_eq!(encode_path_segment("a(b)ä"), "a%28b%29%C3%A4");
    }

    #[test]
    fn title_normalisation() {
        assert_eq!(normalize_title("  Test\ntitle\t "), "Test title");
        assert_eq!(normalize_title(&"x".repeat(81)).len(), 80);
        assert_eq!(normalize_title(" \n "), "");
    }
}
