//! Replace strings that cannot be read aloud meaningfully (hashes, keys,
//! long numbers, binary, base64) with a short spoken placeholder.
//!
//! Technical documentation is full of `3f786850e387550fdab836ed7e6dc881de23001b`
//! style tokens; a TTS voice reading those character by character is useless
//! to the listener. Such a token becomes
//! "long non-textual string ending in 0 0 1 b", keeping only its last six
//! characters (spelled out) so it can still be told apart from others.

/// Tokens longer than this are always replaced.
pub const MAX_WORD_CHARS: usize = 32;
/// Hex/number/gibberish tokens longer than this are replaced.
pub const MIN_CODE_CHARS: usize = 12;
/// Characters of the original kept in the placeholder.
const TAIL_CHARS: usize = 6;

/// Punctuation that may wrap a token without being part of it.
const LEADING: &[char] = &['(', '[', '{', '"', '\'', '“', '‘', '«', '<'];
const TRAILING: &[char] = &[
    '.', ',', ';', ':', '!', '?', ')', ']', '}', '"', '\'', '”', '’', '»', '>',
];

/// Hex digits (so also decimal and binary), optionally with a `0x`/`0b`
/// prefix and `-`, `:`, `.`, `_` separators: hashes, UUIDs, long numbers.
/// IP and MAC addresses (short digit groups) are left alone.
fn is_hex_like(t: &str) -> bool {
    let body = t
        .strip_prefix("0x")
        .or_else(|| t.strip_prefix("0X"))
        .or_else(|| t.strip_prefix("0b"))
        .unwrap_or(t);
    if body.is_empty()
        || !body
            .chars()
            .all(|c| c.is_ascii_hexdigit() || matches!(c, '-' | ':' | '.' | '_'))
    {
        return false;
    }
    let digits = body.chars().filter(char::is_ascii_hexdigit).count();
    let longest_run = body
        .split(|c: char| !c.is_ascii_hexdigit())
        .map(str::len)
        .max()
        .unwrap_or(0);
    digits > MIN_CODE_CHARS && longest_run >= 8
}

/// Letters and digits jumbled together (`a8Fk29xLq0Zm`, base64), as opposed
/// to names with a number in them (`Windows10Home`, `sha256sum`).
fn is_gibberish(t: &str) -> bool {
    if !t
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '_' | '-'))
    {
        return false;
    }
    if t.ends_with('=') && t.chars().filter(char::is_ascii_alphanumeric).count() > MIN_CODE_CHARS {
        return true; // base64 padding
    }
    let chars: Vec<char> = t.chars().collect();
    let switches = chars
        .windows(2)
        .filter(|w| {
            (w[0].is_ascii_alphabetic() && w[1].is_ascii_digit())
                || (w[0].is_ascii_digit() && w[1].is_ascii_alphabetic())
        })
        .count();
    switches >= 3
}

/// Does this token (without surrounding punctuation) need replacing?
pub fn is_non_textual(core: &str) -> bool {
    let len = core.chars().count();
    len > MAX_WORD_CHARS || (len > MIN_CODE_CHARS && (is_hex_like(core) || is_gibberish(core)))
}

/// "long non-textual string ending in 0 0 1 b": the last six letters or
/// digits, spaced so the voice spells them out.
pub fn placeholder(core: &str) -> String {
    let alnum: Vec<char> = core.chars().filter(|c| c.is_alphanumeric()).collect();
    let tail = &alnum[alnum.len().saturating_sub(TAIL_CHARS)..];
    let spelled: Vec<String> = tail.iter().map(char::to_string).collect();
    if spelled.is_empty() {
        "long non-textual string".into()
    } else {
        format!("long non-textual string ending in {}", spelled.join(" "))
    }
}

/// Replace every non-textual token in `text`, keeping the punctuation and
/// spacing around it.
pub fn sanitize_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        let ws = rest.len() - rest.trim_start().len();
        out.push_str(&rest[..ws]);
        rest = &rest[ws..];
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let token = &rest[..end];
        rest = &rest[end..];

        let core_start = token.len() - token.trim_start_matches(LEADING).len();
        let after_lead = &token[core_start..];
        let core = after_lead.trim_end_matches(TRAILING);
        let tail = &after_lead[core.len()..];
        if !core.is_empty() && is_non_textual(core) {
            out.push_str(&token[..core_start]);
            out.push_str(&placeholder(core));
            out.push_str(tail);
        } else {
            out.push_str(token);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_hashes_numbers_and_binary() {
        assert_eq!(
            sanitize_text("Commit 3f786850e387550fdab836ed7e6dc881de23001b fixed it."),
            "Commit long non-textual string ending in 2 3 0 0 1 b fixed it."
        );
        assert_eq!(
            sanitize_text("Mask 0b1101001110010110 and 0xDEADBEEFCAFE01."),
            "Mask long non-textual string ending in 0 1 0 1 1 0 and long non-textual string ending in C A F E 0 1."
        );
        assert_eq!(
            sanitize_text("ID (550e8400-e29b-41d4-a716-446655440000)"),
            "ID (long non-textual string ending in 4 4 0 0 0 0)"
        );
        assert_eq!(
            sanitize_text("Serial 1234567890123456"),
            "Serial long non-textual string ending in 1 2 3 4 5 6"
        );
    }

    #[test]
    fn replaces_gibberish_base64_and_overlong_words() {
        assert_eq!(
            sanitize_text("token a8Fk29xLq0Zm7Q here"),
            "token long non-textual string ending in q 0 Z m 7 Q here"
        );
        assert_eq!(
            sanitize_text("auth: SGVsbG8gV29ybGQhIQ=="),
            "auth: long non-textual string ending in b G Q h I Q"
        );
        let path = r"C:\Program_Files\VendorName\SubFolder\config.yaml";
        assert_eq!(
            sanitize_text(&format!("Edit {path}, then save.")),
            "Edit long non-textual string ending in i g y a m l, then save."
        );
    }

    #[test]
    fn keeps_readable_tokens() {
        for text in [
            "Use Windows10Home with sha256sum on x86_64-linux-gnu.",
            "Connect to 192.168.100.200 or 00:1A:2B:3C:4D:5E.",
            "The getElementById method and internationalization rules.",
            "Call 555-1234 on 2026-11-01.",
            "deadbeef is short, and so is a8Fk29xLq0Zm (12 characters).",
        ] {
            assert_eq!(sanitize_text(text), text);
        }
    }

    #[test]
    fn preserves_whitespace_layout() {
        assert_eq!(sanitize_text("  a\tb  "), "  a\tb  ");
    }
}
