//! Reversible, byte-exact text and markup preprocessor for Osmium v3.
//!
//! Two composable bijective passes use bytes that never appear in valid UTF-8
//! (`0xC0`, `0xC1`, `0xF5..=0xFF`), giving zero escaping overhead on UTF-8 text
//! while remaining 100% lossless on arbitrary binary inputs:
//!
//! 1. **Case folding (`case_encode` / `case_decode`)**: folds ASCII uppercase
//!    letters using `0xFC` (single uppercase letter) and `0xFD` (all-uppercase
//!    word of length >= 2), escaping literal `{0xFC, 0xFD, 0xFE}` with `0xFE`.
//!    Byte-level context models and match models then share statistics across
//!    capitalized and lowercase occurrences of the same word.
//! 2. **Static markup & English token compaction (`dict_encode` / `dict_decode`)**:
//!    replaces frequent Wikipedia/XML tags and common multi-byte sequences with
//!    1-byte (`0xC0`, `0xC1`, `0xF5..=0xFA`) or 2-byte (`0xFB <idx>`) tokens,
//!    escaping literal pass-2 markers with `0xFF`.

use crate::Error;

// Pass 1 markers (disjoint from Pass 2 markers and from all valid UTF-8 bytes).
const CAP_ONE: u8 = 0xFC;
const CAP_WORD: u8 = 0xFD;
const CASE_ESC: u8 = 0xFE;

// Pass 2 markers (disjoint from Pass 1 markers and from all valid UTF-8 bytes).
const DICT_EXT: u8 = 0xFB;
const DICT_ESC: u8 = 0xFF;

const SHORT_CODES: [u8; 8] = [0xC0, 0xC1, 0xF5, 0xF6, 0xF7, 0xF8, 0xF9, 0xFA];

/// Eight 1-byte tokens for the highest-frequency English/wiki sequences.
const SHORT_TOKENS: [&[u8]; 8] = [
    b" the ", b"&quot;", b"[[", b"]]", b"{{", b"}}", b"http://", b" and ",
];

/// Extended 2-byte tokens (`0xFB <idx>`), sorted longest-first per starting byte
/// during matching. Every token is pure lowercase/punctuation ASCII (`len >= 4`).
const LONG_TOKENS: &[&[u8]] = &[
    // Wikipedia / MediaWiki XML structure
    b"<text xml:space=\"preserve\">",
    b"</contributor>",
    b"<contributor>",
    b"</timestamp>",
    b"<timestamp>",
    b"</revision>",
    b"<revision>",
    b"</username>",
    b"<username>",
    b"[[category:",
    b"#redirect [[",
    b"</comment>",
    b"<comment>",
    b"<minor />",
    b"</title>",
    b"<title>",
    b"</page>",
    b"<page>",
    b"</text>",
    b"</id>",
    b"<id>",
    b"https://",
    b"wikipedia",
    b"external links",
    b"references",
    b"&amp;",
    b"&lt;",
    b"&gt;",
    b"====",
    b"===",
    b"== ",
    b" ==",
    // High-frequency English words and morphemes
    b" university ",
    b" between ",
    b" through ",
    b" because ",
    b" century ",
    b" national ",
    b" however ",
    b" against ",
    b" without ",
    b" general ",
    b" history ",
    b" american ",
    b" government ",
    b" population ",
    b" during ",
    b" around ",
    b" before ",
    b" called ",
    b" states ",
    b" united ",
    b" system ",
    b" number ",
    b" school ",
    b" people ",
    b" second ",
    b" should ",
    b" became ",
    b" public ",
    b" series ",
    b" member ",
    b" county ",
    b" family ",
    b" which ",
    b" their ",
    b" there ",
    b" about ",
    b" would ",
    b" other ",
    b" first ",
    b" after ",
    b" where ",
    b" under ",
    b" could ",
    b" being ",
    b" while ",
    b" since ",
    b" years ",
    b" known ",
    b" world ",
    b" later ",
    b" early ",
    b" state ",
    b" south ",
    b" north ",
    b" that ",
    b" with ",
    b" from ",
    b" this ",
    b" have ",
    b" were ",
    b" also ",
    b" into ",
    b" been ",
    b" more ",
    b" when ",
    b" they ",
    b" only ",
    b" over ",
    b" such ",
    b" some ",
    b" than ",
    b" then ",
    b" most ",
    b" time ",
    b" made ",
    b" many ",
    b" used ",
    b" part ",
    b" name ",
    b" city ",
    b"tion",
    b"ment",
    b"ing ",
    b" the",
    b" of ",
    b" in ",
    b" to ",
    b" for ",
    b" was ",
    b" are ",
    b" not ",
    b" but ",
    b" his ",
    b" her ",
    b" one ",
    b" two ",
    // Common source-code keywords
    b"function ",
    b"return ",
    b"struct ",
    b"public ",
    b"const ",
    b"impl ",
];

#[inline]
fn is_case_marker(b: u8) -> bool {
    matches!(b, CAP_ONE | CAP_WORD | CASE_ESC)
}

#[inline]
fn is_dict_marker(b: u8) -> bool {
    matches!(b, 0xC0 | 0xC1 | 0xF5..=0xFB | DICT_ESC)
}

/// Pass 1 encode: reversible ASCII capitalization folding.
pub fn case_encode(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len() + input.len() / 16 + 16);
    let mut i = 0;
    while i < input.len() {
        let b = input[i];
        if b.is_ascii_alphabetic() {
            let mut j = i + 1;
            while j < input.len() && input[j].is_ascii_alphabetic() {
                j += 1;
            }
            let run = &input[i..j];
            if run.len() >= 2 && run.iter().all(|c| c.is_ascii_uppercase()) {
                out.push(CAP_WORD);
                for &c in run {
                    out.push(c.to_ascii_lowercase());
                }
            } else {
                for &c in run {
                    if c.is_ascii_uppercase() {
                        out.push(CAP_ONE);
                        out.push(c.to_ascii_lowercase());
                    } else {
                        out.push(c);
                    }
                }
            }
            i = j;
        } else {
            if is_case_marker(b) {
                out.push(CASE_ESC);
            }
            out.push(b);
            i += 1;
        }
    }
    out
}

/// Pass 1 decode: exact inverse of [`case_encode`].
pub fn case_decode(input: &[u8], max_out: usize) -> Result<Vec<u8>, Error> {
    let mut out = Vec::with_capacity(input.len().min(max_out));
    let mut i = 0;
    while i < input.len() {
        let b = input[i];
        match b {
            CASE_ESC => {
                i += 1;
                let &lit = input.get(i).ok_or(Error::CorruptTransform)?;
                if !is_case_marker(lit) {
                    return Err(Error::CorruptTransform);
                }
                if out.len() >= max_out {
                    return Err(Error::CorruptTransform);
                }
                out.push(lit);
                i += 1;
            }
            CAP_ONE => {
                i += 1;
                let &ch = input.get(i).ok_or(Error::CorruptTransform)?;
                if !ch.is_ascii_lowercase() {
                    return Err(Error::CorruptTransform);
                }
                if out.len() >= max_out {
                    return Err(Error::CorruptTransform);
                }
                out.push(ch.to_ascii_uppercase());
                i += 1;
            }
            CAP_WORD => {
                i += 1;
                let start = i;
                while i < input.len() && input[i].is_ascii_lowercase() {
                    if out.len() >= max_out {
                        return Err(Error::CorruptTransform);
                    }
                    out.push(input[i].to_ascii_uppercase());
                    i += 1;
                }
                if i - start < 2 {
                    return Err(Error::CorruptTransform);
                }
            }
            b'A'..=b'Z' => return Err(Error::CorruptTransform),
            _ => {
                if out.len() >= max_out {
                    return Err(Error::CorruptTransform);
                }
                out.push(b);
                i += 1;
            }
        }
    }
    Ok(out)
}

/// Pass 2 encode: static markup & English token compaction.
pub fn dict_encode(input: &[u8]) -> Vec<u8> {
    // Build a tiny first-byte index into SHORT_TOKENS and LONG_TOKENS.
    let mut short_by_first: [Vec<usize>; 256] = std::array::from_fn(|_| Vec::new());
    for (idx, tok) in SHORT_TOKENS.iter().enumerate() {
        short_by_first[tok[0] as usize].push(idx);
    }
    let mut long_by_first: [Vec<usize>; 256] = std::array::from_fn(|_| Vec::new());
    for (idx, tok) in LONG_TOKENS.iter().enumerate() {
        long_by_first[tok[0] as usize].push(idx);
    }
    for bucket in &mut short_by_first {
        bucket.sort_by(|&a, &b| SHORT_TOKENS[b].len().cmp(&SHORT_TOKENS[a].len()));
    }
    for bucket in &mut long_by_first {
        bucket.sort_by(|&a, &b| LONG_TOKENS[b].len().cmp(&LONG_TOKENS[a].len()));
    }

    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        let b = input[i];
        let rest = &input[i..];

        // Choose whichever token gives the largest byte saving (tie-break: longer match).
        let mut best_kind: u8 = 0; // 0 = none, 1 = short, 2 = long
        let mut best_idx: usize = 0;
        let mut best_len: usize = 0;
        let mut best_saving: usize = 0;

        for &idx in &short_by_first[b as usize] {
            let tok = SHORT_TOKENS[idx];
            if rest.starts_with(tok) {
                let saving = tok.len() - 1;
                if saving > best_saving || (saving == best_saving && tok.len() > best_len) {
                    best_kind = 1;
                    best_idx = idx;
                    best_len = tok.len();
                    best_saving = saving;
                }
                break; // sorted longest first
            }
        }

        for &idx in &long_by_first[b as usize] {
            let tok = LONG_TOKENS[idx];
            if rest.starts_with(tok) {
                let saving = tok.len().saturating_sub(2);
                if saving > best_saving || (saving == best_saving && tok.len() > best_len) {
                    best_kind = 2;
                    best_idx = idx;
                    best_len = tok.len();
                }
                break; // sorted longest first
            }
        }

        match best_kind {
            1 => {
                out.push(SHORT_CODES[best_idx]);
                i += best_len;
            }
            2 => {
                out.push(DICT_EXT);
                out.push(best_idx as u8);
                i += best_len;
            }
            _ => {
                if is_dict_marker(b) {
                    out.push(DICT_ESC);
                }
                out.push(b);
                i += 1;
            }
        }
    }
    out
}

/// Pass 2 decode: exact inverse of [`dict_encode`].
pub fn dict_decode(input: &[u8], max_out: usize) -> Result<Vec<u8>, Error> {
    let mut out = Vec::with_capacity(input.len().min(max_out));
    let mut i = 0;
    while i < input.len() {
        let b = input[i];
        match b {
            DICT_ESC => {
                i += 1;
                let &lit = input.get(i).ok_or(Error::CorruptTransform)?;
                if !is_dict_marker(lit) {
                    return Err(Error::CorruptTransform);
                }
                if out.len() >= max_out {
                    return Err(Error::CorruptTransform);
                }
                out.push(lit);
                i += 1;
            }
            DICT_EXT => {
                i += 1;
                let &idx = input.get(i).ok_or(Error::CorruptTransform)?;
                let tok = *LONG_TOKENS
                    .get(idx as usize)
                    .ok_or(Error::CorruptTransform)?;
                if out.len().saturating_add(tok.len()) > max_out {
                    return Err(Error::CorruptTransform);
                }
                out.extend_from_slice(tok);
                i += 1;
            }
            0xC0 | 0xC1 | 0xF5..=0xFA => {
                let idx = match b {
                    0xC0 => 0,
                    0xC1 => 1,
                    _ => (b - 0xF5) as usize + 2,
                };
                let tok = SHORT_TOKENS[idx];
                if out.len().saturating_add(tok.len()) > max_out {
                    return Err(Error::CorruptTransform);
                }
                out.extend_from_slice(tok);
                i += 1;
            }
            _ => {
                if out.len() >= max_out {
                    return Err(Error::CorruptTransform);
                }
                out.push(b);
                i += 1;
            }
        }
    }
    Ok(out)
}

/// Apply the full two-pass reversible text preprocessor (`case_encode` then `dict_encode`).
pub fn encode(input: &[u8]) -> Vec<u8> {
    let folded = case_encode(input);
    dict_encode(&folded)
}

/// Exact inverse of [`encode`]. `expected_len` bounds output growth against
/// hostile expansion bombs and must match the decoded length.
pub fn decode(input: &[u8], expected_len: usize) -> Result<Vec<u8>, Error> {
    // Pass 1 can expand by at most 2x (every byte escaped or CAP_ONE + letter),
    // so Pass 2's output cannot exceed 2 * expected_len.
    let mid_limit = expected_len.saturating_mul(2).saturating_add(16);
    let mid = dict_decode(input, mid_limit)?;
    let out = case_decode(&mid, expected_len)?;
    if out.len() != expected_len {
        return Err(Error::CorruptTransform);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(data: &[u8]) {
        let enc = encode(data);
        let dec = decode(&enc, data.len()).expect("decode should succeed");
        assert_eq!(dec, data);
    }

    #[test]
    fn roundtrip_text_and_markup() {
        roundtrip(b"");
        roundtrip(b"A");
        roundtrip(b"AB");
        roundtrip(b"ABc");
        roundtrip(b"aBC");
        roundtrip(b"HTTPServer and HTTPS and XML");
        roundtrip(
            b"<page><title>Wikipedia</title><revision><timestamp>2026-11-24</timestamp>\
              <text xml:space=\"preserve\">#REDIRECT [[Category:The quick brown fox &amp; &quot;dog&quot;]] http://example.org</text>\
              </revision></page>",
        );
    }

    #[test]
    fn roundtrip_all_bytes_and_markers() {
        let all: Vec<u8> = (0..=255u8).collect();
        roundtrip(&all);
        roundtrip(&[0xC0, 0xC1, 0xF5, 0xFB, 0xFC, 0xFD, 0xFE, 0xFF]);
        roundtrip(&[b'A', b'B', 0xFC, b'c', b'd', 0xFD, b'E', b'F', 0xFE, 0xFF]);
        roundtrip(&[0xFD, b'a', b'b', 0xFC, b'x', 0xFF, 0xFB, 0x00]);
    }

    #[test]
    fn rejects_corrupt_streams() {
        assert_eq!(decode(&[CASE_ESC], 1), Err(Error::CorruptTransform));
        assert_eq!(decode(&[CASE_ESC, b'a'], 1), Err(Error::CorruptTransform));
        assert_eq!(decode(&[CAP_ONE, b'1'], 1), Err(Error::CorruptTransform));
        assert_eq!(decode(&[CAP_WORD, b'a'], 1), Err(Error::CorruptTransform));
        assert_eq!(decode(b"A", 1), Err(Error::CorruptTransform));
        assert_eq!(decode(&[DICT_ESC], 1), Err(Error::CorruptTransform));
        assert_eq!(decode(&[DICT_ESC, b'a'], 1), Err(Error::CorruptTransform));
        assert_eq!(decode(&[DICT_EXT], 1), Err(Error::CorruptTransform));
        assert_eq!(decode(&[DICT_EXT, 255], 100), Err(Error::CorruptTransform));
    }

    #[test]
    fn shrinks_wikipedia_xml() {
        let sample = b"<page><title>Osmium</title><revision><id>12345</id>\
            <timestamp>2026-11-24T00:00:00Z</timestamp><contributor><username>Editor</username></contributor>\
            <text xml:space=\"preserve\">The element [[Osmium]] is the densest naturally occurring element in the world. \
            See [[Category:Chemical elements]] and http://example.org &quot;references&quot;.</text></revision></page>";
        let enc = encode(sample);
        assert!(
            enc.len() < sample.len(),
            "expected shrinkage: {} -> {}",
            sample.len(),
            enc.len()
        );
        assert_eq!(decode(&enc, sample.len()).unwrap(), sample);
    }
}
