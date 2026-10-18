//! Raw 8-bit headers. A header is meant to be ASCII, its other characters written as encoded
//! words; some senders put the bytes of their own code page there instead, and the message
//! parser turns any byte that is not UTF-8 into U+FFFD. The code page is almost always
//! Windows-1252 (Latin-1 and its extra punctuation), so that is what such bytes are read as.

use std::borrow::Cow;

/// `raw` with every header line that is not valid UTF-8 read as Windows-1252 and written as UTF-8,
/// for the message parser to read; the body and the lines that are valid UTF-8 are as they were.
/// This is a copy for reading: what Missive keeps of the message is never altered.
pub fn decoded(raw: &[u8]) -> Cow<'_, [u8]> {
    let end = headers_end(raw);
    let (headers, body) = raw.split_at(end);
    if std::str::from_utf8(headers).is_ok() {
        return Cow::Borrowed(raw);
    }
    let mut out = Vec::with_capacity(raw.len() + 16);
    for line in headers.split_inclusive(|b| *b == b'\n') {
        match std::str::from_utf8(line) {
            Ok(_) => out.extend_from_slice(line),
            Err(_) => line.iter().for_each(|b| {
                out.extend_from_slice(windows_1252(*b).encode_utf8(&mut [0; 4]).as_bytes())
            }),
        }
    }
    out.extend_from_slice(body);
    Cow::Owned(out)
}

/// Where the header section ends: at the blank line, which stays with the body.
fn headers_end(raw: &[u8]) -> usize {
    let mut start = 0;
    for line in raw.split_inclusive(|b| *b == b'\n') {
        if line == b"\n" || line == b"\r\n" {
            return start;
        }
        start += line.len();
    }
    raw.len()
}

/// The character a Windows-1252 byte stands for. The five bytes it leaves undefined are the
/// control characters of the same number, as browsers read them.
fn windows_1252(byte: u8) -> char {
    const HIGH: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž',
        '\u{8f}', '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9d}',
        'ž', 'Ÿ',
    ];
    match byte {
        0x80..=0x9f => HIGH[usize::from(byte - 0x80)],
        _ => char::from(byte),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_header_line_in_latin_1_is_read_as_windows_1252() {
        let raw =
            b"From: Zo\xe9 <zoe@example.org>\r\nSubject: Caf\xe9 \x80 5 \x96 fin\r\n\r\nBody\r\n";
        assert_eq!(
            decoded(raw).as_ref(),
            "From: Zoé <zoe@example.org>\r\nSubject: Café € 5 – fin\r\n\r\nBody\r\n".as_bytes()
        );
    }

    #[test]
    fn a_header_that_is_valid_utf_8_or_ascii_is_left_alone() {
        let raw = "Subject: Café\r\nTo: a@example.org\r\n\r\nBody\r\n".as_bytes();
        assert!(matches!(decoded(raw), Cow::Borrowed(_)));
    }

    #[test]
    fn only_the_lines_that_are_not_utf_8_change() {
        let raw = b"From: Zo\xc3\xa9 <zoe@example.org>\r\nSubject: Caf\xe9\r\n\r\n";
        assert_eq!(
            decoded(raw).as_ref(),
            "From: Zoé <zoe@example.org>\r\nSubject: Café\r\n\r\n".as_bytes()
        );
    }

    #[test]
    fn the_body_is_never_touched_even_when_it_is_not_utf_8() {
        let raw = b"Subject: Caf\xe9\n\nBody \xe9\n";
        assert_eq!(
            decoded(raw).as_ref(),
            b"Subject: Caf\xc3\xa9\n\nBody \xe9\n".as_slice()
        );
    }

    #[test]
    fn the_bytes_windows_1252_leaves_undefined_are_kept_as_controls() {
        let raw = b"Subject: a\x81b\r\n\r\n";
        assert_eq!(
            decoded(raw).as_ref(),
            "Subject: a\u{81}b\r\n\r\n".as_bytes()
        );
    }

    #[test]
    fn a_message_that_starts_with_its_body_has_no_header_to_read() {
        let raw = b"\r\nCaf\xe9\r\n";
        assert_eq!(decoded(raw).as_ref(), raw);
    }
}
