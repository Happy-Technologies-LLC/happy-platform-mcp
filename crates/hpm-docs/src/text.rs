//! JavaScript-compatible string helpers. `str::trim` and `char::is_whitespace` differ from
//! JS `String.prototype.trim` and `/\s/` (U+0085 and U+FEFF), so parity code uses these.

/// JS `/\s/` and `trim()` whitespace: WhiteSpace plus LineTerminator code points.
#[must_use]
pub(crate) const fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{000B}' | '\u{000C}' | '\r' | ' ' | '\u{00A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

pub(crate) fn js_trim(text: &str) -> &str {
    text.trim_matches(is_js_whitespace)
}

pub(crate) fn js_trim_start(text: &str) -> &str {
    text.trim_start_matches(is_js_whitespace)
}

/// JS `string.length` (UTF-16 code units).
pub(crate) fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// JS `encodeURIComponent`: everything except `A-Za-z0-9 - _ . ! ~ * ' ( )` is escaped.
pub(crate) fn encode_uri_component(text: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(text.len());
    for &byte in text.as_bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push(char::from(HEX[usize::from(byte >> 4)]));
            out.push(char::from(HEX[usize::from(byte & 0x0F)]));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whitespace_matches_javascript_not_rust() {
        assert!(is_js_whitespace('\u{FEFF}'));
        assert!(!is_js_whitespace('\u{0085}'));
        assert_eq!(js_trim("\u{FEFF} a b \u{3000}"), "a b");
        assert_eq!(js_trim_start("\u{0085} x"), "\u{0085} x");
    }

    #[test]
    fn encodes_like_encode_uri_component() {
        assert_eq!(
            encode_uri_component("a+b=c,d@e(f)~g.h_i-j"),
            "a%2Bb%3Dc%2Cd%40e(f)~g.h_i-j"
        );
        assert_eq!(encode_uri_component("é"), "%C3%A9");
    }
}
