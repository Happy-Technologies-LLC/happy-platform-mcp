//! URL/path input bounds. Ports `src/url-limits.js`.
//!
//! Shared by the instance registry, OAuth endpoint policy and ServiceNow
//! client (issue #67: VULN-024..029, VULN-031). Slash trimming is a linear
//! scan, and every URL or path is capped at [`MAX_URL_LENGTH`] before it is
//! parsed or normalized.

use super::js;

/// Maximum URL length, in UTF-16 code units (JavaScript `string.length`).
pub const MAX_URL_LENGTH: usize = 4096;

/// Remove every trailing '/' in O(n).
pub fn trim_trailing_slashes(value: &str) -> &str {
    value.trim_end_matches('/')
}

/// Remove every leading '/' in O(n).
pub fn trim_leading_slashes(value: &str) -> &str {
    value.trim_start_matches('/')
}

/// True when value is longer than [`MAX_URL_LENGTH`] UTF-16 code units.
pub fn exceeds_url_length(value: &str) -> bool {
    js::utf16_len_exceeds(value, MAX_URL_LENGTH)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_slashes_linearly() {
        assert_eq!(
            trim_trailing_slashes("https://example.com//"),
            "https://example.com"
        );
        assert_eq!(
            trim_trailing_slashes("https://example.com"),
            "https://example.com"
        );
        assert_eq!(trim_trailing_slashes("///"), "");
        assert_eq!(trim_trailing_slashes(""), "");
        assert_eq!(trim_leading_slashes("///path/to/"), "path/to/");
        assert_eq!(trim_leading_slashes("path"), "path");
        assert_eq!(trim_leading_slashes("///"), "");
        let long = format!("{}x", "/".repeat(100_000));
        assert_eq!(trim_trailing_slashes(&long), long);
        assert_eq!(trim_leading_slashes(&long), "x");
    }

    #[test]
    fn measures_length_in_utf16_code_units() {
        assert_eq!(MAX_URL_LENGTH, 4096);
        assert!(!exceeds_url_length(&"a".repeat(MAX_URL_LENGTH)));
        assert!(exceeds_url_length(&"a".repeat(MAX_URL_LENGTH + 1)));
        // 4096 two-byte characters are 4096 code units, not 8192.
        assert!(!exceeds_url_length(&"é".repeat(MAX_URL_LENGTH)));
        // An astral character counts as two code units.
        assert!(exceeds_url_length(
            &"\u{1F600}".repeat(MAX_URL_LENGTH / 2 + 1)
        ));
    }
}
