//! Abbreviation-aware sentence splitting, used to place SSML `<s>` tags and
//! to cut over-long paragraphs into TTS-sized segments.

/// Lower-case abbreviations (without the trailing period) that do not end a
/// sentence. Includes a few Norwegian ones.
const ABBREVIATIONS: &[&str] = &[
    "mr", "mrs", "ms", "dr", "prof", "st", "jr", "sr", "rev", "gen", "col", "lt", "sgt", "capt",
    "vs", "etc", "e.g", "i.e", "cf", "al", "no", "nos", "vol", "fig", "figs", "p", "pp", "ch",
    "ed", "eds", "approx", "dept", "est", "inc", "ltd", "co", "mt", "ft", "u.s", "u.k", "a.m",
    "p.m", "jan", "feb", "mar", "apr", "jun", "jul", "aug", "sep", "sept", "oct", "nov", "dec",
    "bl.a", "f.eks", "dvs", "osv", "ca", "nr", "kl", "mht", "evt",
];

const CLOSERS: &[char] = &['"', '\'', '”', '’', '»', ')', ']'];

/// Split `text` into sentences. Whitespace between sentences is dropped;
/// the sentences themselves are returned trimmed.
pub fn split_sentences(text: &str) -> Vec<&str> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut sentences = Vec::new();
    let mut start = 0;
    let mut i = 0;

    while i < chars.len() {
        let (_, c) = chars[i];
        if matches!(c, '.' | '!' | '?' | '…') {
            // Swallow runs like "?!" or "..." and closing quotes/brackets.
            let mut j = i + 1;
            while j < chars.len()
                && (matches!(chars[j].1, '.' | '!' | '?' | '…') || CLOSERS.contains(&chars[j].1))
            {
                j += 1;
            }
            let end_byte = chars.get(j).map_or(text.len(), |(b, _)| *b);
            let followed_by_space = chars.get(j).is_some_and(|(_, ch)| ch.is_whitespace());
            if followed_by_space
                && starts_new_sentence(&chars[j..])
                && !(c == '.' && is_abbreviation(&text[start..chars[i].0]))
            {
                let s = text[start..end_byte].trim();
                if !s.is_empty() {
                    sentences.push(s);
                }
                start = end_byte;
            }
            i = j;
        } else {
            i += 1;
        }
    }
    let rest = text[start..].trim();
    if !rest.is_empty() {
        sentences.push(rest);
    }
    sentences
}

/// The next non-space character must look like a sentence start.
fn starts_new_sentence(rest: &[(usize, char)]) -> bool {
    rest.iter()
        .map(|(_, c)| *c)
        .find(|c| !c.is_whitespace())
        .is_some_and(|c| {
            c.is_uppercase()
                || c.is_ascii_digit()
                || matches!(c, '"' | '\'' | '“' | '‘' | '«' | '(' | '[' | '–' | '—')
        })
}

/// Is the word right before the period an abbreviation or an initial?
fn is_abbreviation(before: &str) -> bool {
    let word = before
        .rsplit(|c: char| c.is_whitespace() || c == '(' || c == '"' || c == '“')
        .next()
        .unwrap_or("");
    if word.chars().count() == 1 && word.chars().all(char::is_alphabetic) {
        return true; // initial, e.g. "J. R. R. Tolkien"
    }
    let lower = word.to_lowercase();
    ABBREVIATIONS.contains(&lower.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_simple_sentences() {
        assert_eq!(
            split_sentences("It was late. The lamp burned! Did it? Yes."),
            vec!["It was late.", "The lamp burned!", "Did it?", "Yes."]
        );
    }

    #[test]
    fn keeps_abbreviations_and_initials() {
        assert_eq!(
            split_sentences("Mr. Hansen met Dr. J. K. Berg at 5 p.m. today. Then he left."),
            vec![
                "Mr. Hansen met Dr. J. K. Berg at 5 p.m. today.",
                "Then he left."
            ]
        );
    }

    #[test]
    fn handles_quotes_and_ellipses() {
        assert_eq!(
            split_sentences("“Go now.” She went... Later, nothing."),
            vec!["“Go now.”", "She went...", "Later, nothing."]
        );
        assert_eq!(
            split_sentences("version 2.5 is out"),
            vec!["version 2.5 is out"]
        );
    }
}
