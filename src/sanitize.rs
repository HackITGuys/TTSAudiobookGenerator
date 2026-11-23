//! Replace strings that cannot be read aloud meaningfully (hashes, keys,
//! long numbers, binary, base64, paths) with a short spoken description of
//! what they are.
//!
//! Technical documentation is full of `3f786850e387550fdab836ed7e6dc881de23001b`
//! style tokens; a TTS voice reading those character by character is useless
//! to the listener. Such a token is named by its type only: "a SHA-1 hash",
//! "an MD5 hash", "a UUID", "a base64-encoded value", "a file path".

use std::sync::LazyLock;

use regex::Regex;

/// Tokens longer than this are always replaced (unless they are plain words).
pub const MAX_WORD_CHARS: usize = 32;
/// Hex/number/gibberish tokens longer than this are replaced.
pub const MIN_CODE_CHARS: usize = 12;

/// `sha256:…`, `SHA-1=…`, `md5:…`: the label names the hash.
static LABELLED_HASH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^(sha3-?(?:224|256|384|512)|sha-?(?:1|224|256|384|512)|md[45]|blake[23][bs]?)[:=]",
    )
    .unwrap()
});
static UUID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\{?[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\}?$").unwrap()
});

/// Punctuation (and Markdown emphasis) that may wrap a token without being
/// part of it.
const LEADING: &[char] = &['(', '[', '{', '"', '\'', '“', '‘', '«', '<', '*', '`'];
const TRAILING: &[char] = &[
    '.', ',', ';', ':', '!', '?', ')', ']', '}', '"', '\'', '”', '’', '»', '>', '*', '`',
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

/// A long but ordinary word (compound nouns, hyphenated phrases).
fn is_plain_word(t: &str) -> bool {
    t.chars()
        .all(|c| c.is_alphabetic() || matches!(c, '-' | '\'' | '’'))
}

/// Does this token (without surrounding punctuation) need replacing?
pub fn is_non_textual(core: &str) -> bool {
    let len = core.chars().count();
    (len > MAX_WORD_CHARS && !is_plain_word(core))
        || (len > MIN_CODE_CHARS && (is_hex_like(core) || is_gibberish(core)))
}

/// Spoken name of an unlabelled hash by its number of hex digits.
fn hash_name(hex_digits: usize) -> Option<&'static str> {
    Some(match hex_digits {
        32 => "an MD5 hash",
        40 => "a SHA-1 hash",
        56 => "a SHA-224 hash",
        64 => "a SHA-256 hash",
        96 => "a SHA-384 hash",
        128 => "a SHA-512 hash",
        _ => return None,
    })
}

/// "SHA256" -> "SHA-256", "sha3-512" -> "SHA3-512", "md5" -> "MD5".
fn hash_label(raw: &str) -> String {
    let up = raw.to_ascii_uppercase();
    if let Some(bits) = up.strip_prefix("SHA3") {
        format!("SHA3-{}", bits.trim_start_matches('-'))
    } else if let Some(bits) = up.strip_prefix("SHA") {
        format!("SHA-{}", bits.trim_start_matches('-'))
    } else {
        up
    }
}

/// What the token is, without any of its characters: "a SHA-256 hash",
/// "a UUID", "a long number", ...
pub fn placeholder(core: &str) -> String {
    if let Some(c) = LABELLED_HASH.captures(core) {
        let label = hash_label(&c[1]);
        let article = if label.starts_with('M') { "an" } else { "a" };
        return format!("{article} {label} hash");
    }
    if UUID.is_match(core) {
        return "a UUID".into();
    }
    if is_hex_like(core) {
        if core.starts_with("0b") {
            return "a binary number".into();
        }
        let body = core
            .strip_prefix("0x")
            .or_else(|| core.strip_prefix("0X"))
            .unwrap_or(core);
        if body
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '-' | '.' | '_' | ':'))
        {
            return "a long number".into();
        }
        let digits = body.chars().filter(char::is_ascii_hexdigit).count();
        return hash_name(digits).unwrap_or("a hexadecimal value").into();
    }
    if (core.contains('\\') || core.matches('/').count() >= 2)
        && !core.contains("//")
        && core.chars().any(char::is_alphabetic)
    {
        return "a file path".into();
    }
    if is_gibberish(core) {
        let base64ish =
            core.ends_with('=') || core.contains(['+', '/']) || core.chars().count() >= 20;
        return if base64ish {
            "a base64-encoded value".into()
        } else {
            "a random code".into()
        };
    }
    "a long code".into()
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
        // `NAME=value`: keep the variable name, describe only the value.
        let (name, value) = match core.split_once('=') {
            Some((n, v))
                if !n.is_empty()
                    && n.len() <= MAX_WORD_CHARS
                    && !v.is_empty()
                    && !v.starts_with('=')
                    && !LABELLED_HASH.is_match(core)
                    && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') =>
            {
                (&core[..n.len() + 1], v)
            }
            _ => ("", core),
        };
        if !value.is_empty() && is_non_textual(value) {
            out.push_str(&token[..core_start]);
            out.push_str(name);
            out.push_str(&placeholder(value));
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
    fn names_hashes_by_type() {
        assert_eq!(
            sanitize_text("Commit 3f786850e387550fdab836ed7e6dc881de23001b fixed it."),
            "Commit a SHA-1 hash fixed it."
        );
        assert_eq!(
            sanitize_text("got d41d8cd98f00b204e9800998ecf8427e back"),
            "got an MD5 hash back"
        );
        let sha256 = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert_eq!(sanitize_text(&format!("({sha256})")), "(a SHA-256 hash)");
        assert_eq!(
            sanitize_text(&format!("digest sha256:{sha256}.")),
            "digest a SHA-256 hash."
        );
        assert_eq!(
            sanitize_text(&format!("MD5={}", &sha256[..32])),
            "an MD5 hash"
        );
        assert_eq!(
            sanitize_text(&format!("{sha256}{sha256}")),
            "a SHA-512 hash"
        );
        assert_eq!(
            sanitize_text("Mask 0b1101001110010110 and 0xDEADBEEFCAFE01."),
            "Mask a binary number and a hexadecimal value."
        );
        assert_eq!(
            sanitize_text("ID (550e8400-e29b-41d4-a716-446655440000)"),
            "ID (a UUID)"
        );
        assert_eq!(
            sanitize_text("Serial 1234567890123456"),
            "Serial a long number"
        );
    }

    #[test]
    fn names_base64_tokens_and_paths() {
        assert_eq!(
            sanitize_text("token a8Fk29xLq0Zm7Q here"),
            "token a random code here"
        );
        assert_eq!(
            sanitize_text("auth: SGVsbG8gV29ybGQhIQ=="),
            "auth: a base64-encoded value"
        );
        let path = r"C:\Program_Files\VendorName\SubFolder\config.yaml";
        assert_eq!(
            sanitize_text(&format!("Edit {path}, then save.")),
            "Edit a file path, then save."
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
            "Use SHA-256 or MD5 hashes.",
            "Rechtsschutzversicherungsgesellschaften are long-winded.",
        ] {
            assert_eq!(sanitize_text(text), text);
        }
    }

    #[test]
    fn preserves_whitespace_layout() {
        assert_eq!(sanitize_text("  a\tb  "), "  a\tb  ");
    }
}
