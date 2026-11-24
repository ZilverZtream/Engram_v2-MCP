#[derive(Debug)]
pub struct Token {
    pub text: String,
    pub string: bool,
    pub line: usize,
}

/// A deliberately small lexical reader: quoted examples and commented calls cannot
/// become registrations. Values stay strings; this does not execute routing code.
pub fn tokens(source: &str, vb: bool) -> Vec<Token> {
    let bytes = source.as_bytes();
    let (mut i, mut line) = (0, 1);
    let mut out = Vec::new();
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            line += 1;
            i += 1;
            continue;
        }
        if bytes[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if (vb && bytes[i] == b'\'') || (!vb && bytes[i..].starts_with(b"//")) {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if !vb && bytes[i..].starts_with(b"/*") {
            i += 2;
            while i < bytes.len() && !bytes[i..].starts_with(b"*/") {
                if bytes[i] == b'\n' {
                    line += 1;
                }
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            continue;
        }
        let start = i;
        let start_line = line;
        if bytes[i] == b'"' || (!vb && bytes[i] == b'\'') {
            let quote = bytes[i];
            i += 1;
            let mut value = Vec::new();
            while i < bytes.len() {
                if bytes[i] == quote {
                    if vb && bytes.get(i + 1) == Some(&quote) {
                        value.push(quote);
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                if !vb && bytes[i] == b'\\' {
                    // Nonliteral escapes are retained, so they cannot silently
                    // match a different URL or filesystem target.
                    value.push(bytes[i]);
                    i += 1;
                    if i == bytes.len() {
                        break;
                    }
                }
                if bytes[i] == b'\n' {
                    line += 1;
                }
                value.push(bytes[i]);
                i += 1;
            }
            out.push(Token {
                text: String::from_utf8_lossy(&value).into_owned(),
                string: true,
                line: start_line,
            });
        } else if bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' {
            i += 1;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let text = &source[start..i];
            if vb && text.eq_ignore_ascii_case("rem") {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            } else {
                out.push(Token {
                    text: text.to_owned(),
                    string: false,
                    line: start_line,
                });
            }
        } else {
            // Only ASCII punctuation participates in the grammar. Advance whole
            // Unicode scalars so source slicing always remains on a boundary.
            let ch = source[i..].chars().next().expect("nonempty source tail");
            i += ch.len_utf8();
            out.push(Token {
                text: ch.to_string(),
                string: false,
                line: start_line,
            });
        }
    }
    out
}
