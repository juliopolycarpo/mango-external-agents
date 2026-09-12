//! Credential-shaped text removed before stderr crosses a diagnostic boundary.
//!
//! A vendor CLI that fails while printing the request it was making prints the header it sent.
//! The unredacted tail is never handed out: [`stderr_text`] is what
//! [`StderrTail::read`](crate::StderrTail::read) returns, so a host that logs the tail logs this
//! form or nothing.
//!
//! Written as a scanner rather than as regular expressions. Three passes, in the same order and
//! with the same shapes as the patterns they replace, so each rule can be read on its own:
//! a bearer header, a `key = value` assignment, and the password in a URL's userinfo.

/// Redacts a stderr tail and strips terminal-unsafe control characters.
///
/// # Example
///
/// ```
/// use mango_external_agents::redact;
///
/// let tail = redact::stderr_text("Authorization: Bearer sk-live-42 API_KEY=hunter2");
/// assert_eq!(tail, "Authorization: Bearer [REDACTED] API_KEY=[REDACTED]");
/// ```
pub fn stderr_text(raw: &str) -> String {
    let bearer = redact_bearer(raw);
    let assignments = redact_assignments(&bearer);
    let urls = redact_url_passwords(&assignments);
    strip_control_characters(&urls)
}

const REDACTED: &str = "[REDACTED]";

/// `authorization : bearer <token>`, however it was spaced and cased.
fn redact_bearer(raw: &str) -> String {
    rewrite(raw, |bytes, at| {
        let after_keyword = match_word(bytes, at, b"authorization")?;
        let after_colon = match_byte(bytes, skip_spaces(bytes, after_keyword), b':')?;
        let after_scheme = match_word(bytes, skip_spaces(bytes, after_colon), b"bearer")?;
        let token_start = skip_spaces(bytes, after_scheme);
        if token_start == after_scheme {
            return None;
        }
        let token_end = take_while(bytes, token_start, is_value_byte);
        if token_end == token_start {
            return None;
        }
        Some(Rewrite {
            end: token_end,
            replacement: format!("{} {REDACTED}", as_text(bytes, at, after_scheme)),
        })
    })
}

/// `api_key=`, `secret:`, `token = `, … and whatever value follows.
fn redact_assignments(raw: &str) -> String {
    const KEYWORDS: &[&[u8]] = &[b"secret", b"token", b"password", b"passwd", b"credential"];
    rewrite(raw, |bytes, at| {
        let after_keyword = match_api_key(bytes, at)
            .or_else(|| KEYWORDS.iter().find_map(|word| match_word(bytes, at, word)))?;
        let separator = skip_spaces(bytes, after_keyword);
        let after_separator =
            match_byte(bytes, separator, b'=').or_else(|| match_byte(bytes, separator, b':'))?;
        let value_start = skip_spaces(bytes, after_separator);
        let value_end = take_while(bytes, value_start, is_value_byte);
        if value_end == value_start {
            return None;
        }
        Some(Rewrite {
            end: value_end,
            replacement: format!("{}={REDACTED}", as_text(bytes, at, after_keyword)),
        })
    })
}

/// The password half of `scheme://user:password@host`.
fn redact_url_passwords(raw: &str) -> String {
    rewrite(raw, |bytes, at| {
        if !bytes.get(at)?.is_ascii_alphabetic() {
            return None;
        }
        let scheme_end = take_while(bytes, at + 1, |byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'.' | b'-')
        });
        let after_scheme = match_word(bytes, scheme_end, b"://")?;
        let user_end = take_while(bytes, after_scheme, |byte| {
            !matches!(
                byte,
                b' ' | b'\t' | b'\n' | b'\r' | b':' | b'/' | b'?' | b'#'
            )
        });
        if user_end == after_scheme {
            return None;
        }
        let password_start = match_byte(bytes, user_end, b':')?;
        let password_end = take_while(bytes, password_start, |byte| {
            !matches!(
                byte,
                b' ' | b'\t' | b'\n' | b'\r' | b'@' | b'/' | b'?' | b'#'
            )
        });
        if password_end == password_start || bytes.get(password_end) != Some(&b'@') {
            return None;
        }
        Some(Rewrite {
            end: password_end,
            replacement: format!("{}{REDACTED}", as_text(bytes, at, password_start)),
        })
    })
}

/// What one rule matched: where the match ends, and what stands in its place.
struct Rewrite {
    end: usize,
    replacement: String,
}

/// Scans left to right, letting `rule` claim a span starting at each word boundary.
fn rewrite(raw: &str, rule: impl Fn(&[u8], usize) -> Option<Rewrite>) -> String {
    let bytes = raw.as_bytes();
    let mut out = String::with_capacity(raw.len());
    let mut at = 0;
    while at < bytes.len() {
        if starts_a_word(bytes, at)
            && let Some(found) = rule(bytes, at)
        {
            out.push_str(&found.replacement);
            at = found.end;
            continue;
        }
        let next = next_char_boundary(raw, at);
        out.push_str(&raw[at..next]);
        at = next;
    }
    out
}

/// The `\b` the patterns open with: a match may not start inside a word.
fn starts_a_word(bytes: &[u8], at: usize) -> bool {
    match at.checked_sub(1).and_then(|before| bytes.get(before)) {
        Some(byte) => !byte.is_ascii_alphanumeric() && *byte != b'_',
        None => true,
    }
}

fn next_char_boundary(raw: &str, at: usize) -> usize {
    let mut next = at + 1;
    while next < raw.len() && !raw.is_char_boundary(next) {
        next += 1;
    }
    next.min(raw.len())
}

fn match_word(bytes: &[u8], at: usize, word: &[u8]) -> Option<usize> {
    let end = at.checked_add(word.len())?;
    bytes
        .get(at..end)?
        .eq_ignore_ascii_case(word)
        .then_some(end)
}

/// `api[_-]?key`, the one keyword with a shape rather than a spelling.
fn match_api_key(bytes: &[u8], at: usize) -> Option<usize> {
    let after_api = match_word(bytes, at, b"api")?;
    let after_separator = match bytes.get(after_api) {
        Some(b'_' | b'-') => after_api + 1,
        _ => after_api,
    };
    match_word(bytes, after_separator, b"key")
}

fn match_byte(bytes: &[u8], at: usize, expected: u8) -> Option<usize> {
    (bytes.get(at) == Some(&expected)).then_some(at + 1)
}

fn skip_spaces(bytes: &[u8], at: usize) -> usize {
    take_while(bytes, at, |byte| {
        matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
    })
}

fn take_while(bytes: &[u8], at: usize, keep: impl Fn(u8) -> bool) -> usize {
    let mut end = at;
    while let Some(byte) = bytes.get(end) {
        if !keep(*byte) {
            break;
        }
        end += 1;
    }
    end
}

/// The `[^\s,;]+` every value in these patterns is.
fn is_value_byte(byte: u8) -> bool {
    !matches!(
        byte,
        b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c | b',' | b';'
    )
}

fn as_text(bytes: &[u8], from: usize, to: usize) -> String {
    String::from_utf8_lossy(bytes.get(from..to).unwrap_or_default()).into_owned()
}

/// Keeps tab and newline, drops every other C0 control, DEL and the C1 block.
///
/// A lone `\r` or an escape sequence in a vendor's diagnostic is a terminal-rendering problem the
/// moment anyone tails a log.
fn strip_control_characters(raw: &str) -> String {
    raw.chars()
        .filter(|character| {
            let code = u32::from(*character);
            code == 0x09 || code == 0x0a || (code > 0x1f && !(0x7f..=0x9f).contains(&code))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::stderr_text;

    /// The fixture a vendor child writes in the port's own process test.
    const FIXTURE: &str =
        "Authorization: Bearer top-secret API_KEY=another-secret redis://app:password@db/main";

    #[test]
    fn redacts_every_credential_shape_in_the_fixture() {
        let redacted = stderr_text(FIXTURE);

        assert!(
            !redacted.contains("top-secret"),
            "expected no bearer token, received {redacted:?}"
        );
        assert!(
            !redacted.contains("another-secret"),
            "expected no api key, received {redacted:?}"
        );
        assert!(
            !redacted.contains("password@"),
            "expected no url password, received {redacted:?}"
        );
        assert_eq!(
            redacted,
            "Authorization: Bearer [REDACTED] API_KEY=[REDACTED] redis://app:[REDACTED]@db/main"
        );
    }

    #[test]
    fn matches_however_the_vendor_spaced_and_cased_it() {
        assert_eq!(
            stderr_text("authorization :  BEARER\tsk-live-1"),
            "authorization :  BEARER [REDACTED]"
        );
        assert_eq!(stderr_text("Api-Key : v"), "Api-Key=[REDACTED]");
        assert_eq!(stderr_text("apikey=v"), "apikey=[REDACTED]");
        assert_eq!(stderr_text("PASSWD:v"), "PASSWD=[REDACTED]");
    }

    #[test]
    fn leaves_a_keyword_inside_a_longer_word_alone() {
        assert_eq!(stderr_text("mytoken=v"), "mytoken=v");
        assert_eq!(stderr_text("my_token=v"), "my_token=v");
    }

    #[test]
    fn a_value_ends_at_the_separator_that_follows_it() {
        assert_eq!(stderr_text("token=abc, next=1"), "token=[REDACTED], next=1");
        assert_eq!(stderr_text("secret=abc; more"), "secret=[REDACTED]; more");
    }

    #[test]
    fn a_url_without_a_password_is_left_intact() {
        assert_eq!(
            stderr_text("https://example.com/x"),
            "https://example.com/x"
        );
        assert_eq!(stderr_text("redis://db:6379/0"), "redis://db:6379/0");
    }

    #[test]
    fn strips_control_characters_but_keeps_tabs_and_newlines() {
        let redacted = stderr_text("a\u{1b}[31mb\u{0}c\td\ne\u{9f}f");
        assert_eq!(redacted, "a[31mbc\td\nef");
    }

    #[test]
    fn keeps_text_that_carries_no_credential() {
        let line = "error: the vendor exited with status 1 (no configuration found)";
        assert_eq!(stderr_text(line), line);
    }

    #[test]
    fn survives_multibyte_text() {
        assert_eq!(
            stderr_text("não encontrado: token=café"),
            "não encontrado: token=[REDACTED]"
        );
    }
}
