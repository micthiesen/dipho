//! Query-side text normalization, mirroring the sidecar's
//! `dipho_ingest/normalize.py` token for token: lowercase, digits/ordinals
//! expanded to words, punctuation stripped. The index stores normalized
//! tokens; search normalizes the query, not the mapping — "25" must find
//! "twenty five".
//!
//! The number expansion is a token-level port of num2words' English output
//! (the sidecar's expander). Hyphens and commas in num2words' prose become
//! token separators after punctuation stripping, so only the word sequence
//! has to match — including its "and" placement, pinned by tests against
//! sidecar ground truth.
//!
//! Accepted divergences from the Python side, both unreachable for English
//! ASR tokens and search queries: numbers beyond u128 fall back to
//! digit-by-digit earlier than num2words (which names scales up to ~1e306),
//! and non-ASCII unicode digits are stripped rather than expanded.

const UNITS: [&str; 20] = [
    "zero",
    "one",
    "two",
    "three",
    "four",
    "five",
    "six",
    "seven",
    "eight",
    "nine",
    "ten",
    "eleven",
    "twelve",
    "thirteen",
    "fourteen",
    "fifteen",
    "sixteen",
    "seventeen",
    "eighteen",
    "nineteen",
];

const TENS: [&str; 10] = [
    "", "", "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety",
];

/// Short-scale names for 10^(3·(i+1)). u128 tops out at ~340 undecillion,
/// so the table covers the whole parseable range.
const SCALES: [&str; 12] = [
    "thousand",
    "million",
    "billion",
    "trillion",
    "quadrillion",
    "quintillion",
    "sextillion",
    "septillion",
    "octillion",
    "nonillion",
    "decillion",
    "undecillion",
];

/// Characters kept inside tokens (the sidecar's `_STRIP` class).
fn keep(c: char) -> bool {
    matches!(c, 'a'..='z' | '0'..='9' | '\'')
}

fn push_sub_hundred(n: usize, out: &mut Vec<String>) {
    if n < 20 {
        out.push(UNITS[n].to_string());
    } else {
        out.push(TENS[n / 10].to_string());
        if !n.is_multiple_of(10) {
            out.push(UNITS[n % 10].to_string());
        }
    }
}

fn push_group(n: usize, out: &mut Vec<String>) {
    if n >= 100 {
        out.push(UNITS[n / 100].to_string());
        out.push("hundred".to_string());
        if !n.is_multiple_of(100) {
            out.push("and".to_string());
            push_sub_hundred(n % 100, out);
        }
    } else {
        push_sub_hundred(n, out);
    }
}

fn cardinal_tokens(n: u128) -> Vec<String> {
    if n == 0 {
        return vec!["zero".to_string()];
    }
    // Little-endian 3-digit groups; index i scales by 10^(3i).
    let mut groups = Vec::new();
    let mut m = n;
    while m > 0 {
        groups.push((m % 1000) as usize);
        m /= 1000;
    }
    let mut out = Vec::new();
    for i in (0..groups.len()).rev() {
        let g = groups[i];
        if g == 0 {
            continue;
        }
        // num2words joins a trailing sub-hundred remainder with "and"
        // ("one million and five"); larger remainders get a comma, which
        // strips to a plain separator.
        if i == 0 && !out.is_empty() && g < 100 {
            out.push("and".to_string());
        }
        push_group(g, &mut out);
        if i > 0 {
            out.push(SCALES[i - 1].to_string());
        }
    }
    out
}

/// num2words' `to_ordinal`: cardinal words with the last word transformed.
fn ordinal_tokens(n: u128) -> Vec<String> {
    let mut tokens = cardinal_tokens(n);
    let last = tokens.pop().expect("cardinal is never empty");
    let ordinal = match last.as_str() {
        "one" => "first".to_string(),
        "two" => "second".to_string(),
        "three" => "third".to_string(),
        "five" => "fifth".to_string(),
        "eight" => "eighth".to_string(),
        "nine" => "ninth".to_string(),
        "twelve" => "twelfth".to_string(),
        w if w.ends_with('y') => format!("{}ieth", &w[..w.len() - 1]),
        w => format!("{w}th"),
    };
    tokens.push(ordinal);
    tokens
}

fn expand_number(digits: &str, ordinal: bool) -> Vec<String> {
    match digits.parse::<u128>() {
        Ok(n) if ordinal => ordinal_tokens(n),
        Ok(n) => cardinal_tokens(n),
        // Absurd magnitudes: spell digit by digit (cardinals, like the
        // sidecar's overflow fallback).
        Err(_) => digits
            .bytes()
            .map(|b| UNITS[(b - b'0') as usize].to_string())
            .collect(),
    }
}

/// `^(\d+)(st|nd|rd|th)$` over the stripped token; returns the digit run.
fn match_ordinal(stripped: &str) -> Option<&str> {
    let (digits, suffix) = stripped.split_at(stripped.len().checked_sub(2)?);
    if matches!(suffix, "st" | "nd" | "rd" | "th")
        && !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
    {
        Some(digits)
    } else {
        None
    }
}

/// Normalized tokens for one whitespace-delimited word. May be empty (pure
/// punctuation) or several tokens ("25" → ["twenty", "five"]).
fn normalize_word(raw: &str) -> Vec<String> {
    let lowered = raw.to_lowercase();
    let stripped: String = lowered.chars().filter(|&c| keep(c)).collect();
    let expanded = if let Some(digits) = match_ordinal(&stripped) {
        expand_number(digits, true).join(" ")
    } else {
        // Expand each maximal digit run in place, then strip punctuation.
        let mut s = String::new();
        let mut run = String::new();
        for c in lowered.chars() {
            if c.is_ascii_digit() {
                run.push(c);
            } else {
                flush_run(&mut run, &mut s);
                s.push(c);
            }
        }
        flush_run(&mut run, &mut s);
        s
    };
    expanded
        .split_whitespace()
        .flat_map(|part| {
            part.chars()
                .map(|c| if keep(c) { c } else { ' ' })
                .collect::<String>()
                .split_whitespace()
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .filter(|t| !t.trim_matches('\'').is_empty())
        .collect()
}

fn flush_run(run: &mut String, s: &mut String) {
    if !run.is_empty() {
        s.push(' ');
        s.push_str(&expand_number(run, false).join(" "));
        s.push(' ');
        run.clear();
    }
}

/// Normalize a search query into the index's token stream.
pub fn normalize_query(query: &str) -> Vec<String> {
    query.split_whitespace().flat_map(normalize_word).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(raw: &str) -> Vec<String> {
        normalize_word(raw)
    }

    /// Ground truth captured from `dipho_ingest.normalize.normalize_word`
    /// (num2words 'en') — the exact token stream the index was built with.
    #[test]
    fn matches_sidecar_normalization() {
        #[rustfmt::skip]
        let cases: &[(&str, &[&str])] = &[
            ("25", &["twenty", "five"]),
            ("25th", &["twenty", "fifth"]),
            ("1st", &["first"]),
            ("2nd", &["second"]),
            ("3rd", &["third"]),
            ("0", &["zero"]),
            ("5", &["five"]),
            ("13", &["thirteen"]),
            ("20", &["twenty"]),
            ("21", &["twenty", "one"]),
            ("40", &["forty"]),
            ("100", &["one", "hundred"]),
            ("105", &["one", "hundred", "and", "five"]),
            ("111", &["one", "hundred", "and", "eleven"]),
            ("907", &["nine", "hundred", "and", "seven"]),
            ("1000", &["one", "thousand"]),
            ("1234", &["one", "thousand", "two", "hundred", "and", "thirty", "four"]),
            ("1,000", &["one", "zero"]),
            ("20100", &["twenty", "thousand", "one", "hundred"]),
            ("100026", &["one", "hundred", "thousand", "and", "twenty", "six"]),
            ("123456", &["one", "hundred", "and", "twenty", "three", "thousand",
                         "four", "hundred", "and", "fifty", "six"]),
            ("300000", &["three", "hundred", "thousand"]),
            ("1000000", &["one", "million"]),
            ("1000005", &["one", "million", "and", "five"]),
            ("1000026", &["one", "million", "and", "twenty", "six"]),
            ("1000126", &["one", "million", "one", "hundred", "and", "twenty", "six"]),
            ("2026", &["two", "thousand", "and", "twenty", "six"]),
            ("9000000000000000000", &["nine", "quintillion"]),
            ("1000000000000000000000000000000000000", &["one", "undecillion"]),
            ("20th", &["twentieth"]),
            ("23rd", &["twenty", "third"]),
            ("70th", &["seventieth"]),
            ("100th", &["one", "hundredth"]),
            ("110th", &["one", "hundred", "and", "tenth"]),
            ("101st", &["one", "hundred", "and", "first"]),
            ("12th", &["twelfth"]),
            ("1000000th", &["one", "millionth"]),
            ("abc25def", &["abc", "twenty", "five", "def"]),
            ("Hello,", &["hello"]),
            ("don't", &["don't"]),
            ("'em", &["'em"]),
            ("...", &[]),
            ("''", &[]),
            ("WORLD!", &["world"]),
            ("3.14", &["three", "fourteen"]),
            ("twenty-five", &["twenty", "five"]),
            ("fifth", &["fifth"]),
        ];
        for (raw, expected) in cases {
            assert_eq!(&norm(raw), expected, "normalize_word({raw:?})");
        }
    }

    #[test]
    fn huge_numbers_fall_back_to_digit_by_digit() {
        // 99 nines: beyond u128.
        let raw = "9".repeat(99);
        let tokens = norm(&raw);
        assert_eq!(tokens.len(), 99);
        assert!(tokens.iter().all(|t| t == "nine"));
    }

    #[test]
    fn query_normalization_splits_on_whitespace() {
        assert_eq!(
            normalize_query("  I have 25 CATS! "),
            vec!["i", "have", "twenty", "five", "cats"]
        );
        assert!(normalize_query("  ...  ").is_empty());
        assert!(normalize_query("").is_empty());
    }
}
