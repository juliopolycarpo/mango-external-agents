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
    // Stripped first rather than last. A CLI that colours its output writes `Authorization:` and
    // ` Bearer sk-live-x` either side of an escape sequence, and a rule that reads the two as
    // neighbours never sees the token at all if the sequence is still sitting between them.
    let plain = strip_control_characters(raw);
    let redacted = redact_plain(&plain);
    if !raw.contains('\u{1b}') {
        return redacted;
    }
    // `ESC [ SP A` is a complete sequence, and it is also the start of `ESC [ SP API_KEY=x`. The
    // two readings differ only in whether that intermediate byte and the letter after it are
    // taken as a sequence, and each leaves a credential shown for the other's input. Both are
    // redacted and the one that hides more is returned; a tie takes the standard reading, which
    // leaves no stray letter.
    let standard_plain = strip_with(raw, true);
    if standard_plain == plain {
        return redacted;
    }
    let standard = redact_plain(&standard_plain);
    if standard.matches(REDACTED).count() >= redacted.matches(REDACTED).count() {
        return standard;
    }
    redacted
}

/// The three rules, on text that has already been stripped.
fn redact_plain(plain: &str) -> String {
    let bearer = redact_bearer(plain);
    let assignments = redact_assignments(&bearer);
    redact_url_passwords(&assignments)
}

/// A safe executable summary from a host-owned path.
///
/// A diagnostic can name known vendor programs, but an arbitrary executable basename is
/// host-provided text that can carry a secret. Both slash forms are accepted so a Windows path
/// stays safe when formatted on another platform. Unknown names report `custom executable`.
///
/// # Example
///
/// ```
/// use mango_external_agents::redact;
///
/// assert_eq!(redact::program_name("/private/bin/codex"), "codex");
/// assert_eq!(
///     redact::program_name("/private/bin/customer-secret-canary"),
///     "custom executable"
/// );
/// ```
pub fn program_name(raw: &str) -> String {
    let name = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
    if is_known_program(name) {
        return name.to_owned();
    }
    String::from("custom executable")
}

const REDACTED: &str = "[REDACTED]";

/// Every program a harness in this workspace launches on its own account.
///
/// The two first-party CLIs, then the `argv[0]` of each built-in ACP profile in
/// `mango-agent-acp`. A profile whose executable is missing here is reported as
/// `custom executable`, and a launch failure then names a cause without naming which agent it
/// belonged to. The ACP crate holds the list to this one in
/// `every_builtin_profile_is_named_rather_than_redacted_in_a_diagnostic`, so adding a profile
/// without adding its program fails there rather than degrading in a log.
const KNOWN_PROGRAMS: &[&str] = &[
    "claude",
    "codex",
    "cursor-agent",
    "grok",
    "opencode",
    "gemini",
    "copilot",
    "goose",
    "codex-acp",
    "claude-agent-acp",
];

fn is_known_program(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let bare = lower
        .strip_suffix(".exe")
        .or_else(|| lower.strip_suffix(".ps1"))
        .unwrap_or(&lower);
    KNOWN_PROGRAMS.contains(&bare)
}

/// `authorization : bearer <token>`, however it was spaced and cased.
///
/// `basic` counts as well as `bearer`: it is the same header carrying the same credential, and a
/// base64 user:password is no less a secret for being the older spelling.
fn redact_bearer(raw: &str) -> String {
    rewrite(raw, |bytes, at| {
        let after_keyword = match_word(bytes, at, b"authorization")?;
        let after_colon = match_byte(bytes, skip_spaces(bytes, after_keyword), b':')?;
        let scheme_start = skip_spaces(bytes, after_colon);
        let after_scheme = match_word(bytes, scheme_start, b"bearer")
            .or_else(|| match_word(bytes, scheme_start, b"basic"))?;
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
        // `AWS_SECRET_ACCESS_KEY=` is a keyword with the rest of a name after it. Requiring the
        // separator to follow the keyword itself would redact only the spellings that happen to
        // end on one, which is a minority of the names credentials actually have.
        let name_end = take_while(bytes, after_keyword, is_name_byte);
        let separator = skip_spaces(bytes, name_end);
        let after_separator =
            match_byte(bytes, separator, b'=').or_else(|| match_byte(bytes, separator, b':'))?;
        let value_start = skip_spaces(bytes, after_separator);
        let value_end = take_while(bytes, value_start, is_value_byte);
        if value_end == value_start {
            return None;
        }
        Some(Rewrite {
            end: value_end,
            replacement: format!("{}={REDACTED}", as_text(bytes, at, name_end)),
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
        // The user half is optional: `redis://:hunter2@db/main` is what a URL looks like when
        // only a password was configured.
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
///
/// An underscore counts as a boundary here even though `\w` counts it as a word character. The
/// spelling that matters is `OPENAI_API_KEY`, and treating `_` as part of the preceding word is
/// what let every screaming-snake-case credential walk past the scanner untouched.
fn starts_a_word(bytes: &[u8], at: usize) -> bool {
    match at.checked_sub(1).and_then(|before| bytes.get(before)) {
        Some(byte) => !byte.is_ascii_alphanumeric(),
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

/// What the rest of a variable's name is made of, after the keyword that identified it.
fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')
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

/// Keeps tab and newline, drops every other C0 control, DEL, the C1 block and every bidirectional
/// formatting character, and takes a CSI sequence out whole rather than leaving its parameters
/// behind as text.
///
/// A lone `\r` or an escape sequence in a vendor's diagnostic is a terminal-rendering problem the
/// moment anyone tails a log. Dropping only the `ESC` would leave `[31m` sitting in the middle of
/// a header, which reads as noise and hides a token from the rules that run after this.
///
/// The bidirectional set goes for the reason it goes everywhere else in this crate — see
/// [`normalize::is_strippable`](crate::normalize) — and this tail is rendered in a host's
/// diagnostics like any other vendor-written string, so the answer has to be the same one.
fn strip_control_characters(raw: &str) -> String {
    strip_with(raw, false)
}

/// [`strip_control_characters`], reading `ESC [ <params> <intermediates> <final>` as ECMA-48 does
/// when `intermediates` is set.
fn strip_with(raw: &str, intermediates: bool) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut characters = raw.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\u{1b}' {
            if characters.peek() == Some(&'[') {
                characters.next();
                skip_csi_body(&mut characters, intermediates);
            }
            continue;
        }
        if character == '\t' || character == '\n' {
            out.push(character);
            continue;
        }
        if !is_unsafe_to_render(character) {
            out.push(character);
        }
    }
    out
}

/// Consumes what follows `ESC [`: parameter bytes (`0x30..=0x3f`), then intermediate bytes
/// (`0x20..=0x2f`) when `intermediates` is set, then one final byte (`0x40..=0x7e`).
///
/// A byte that fits none of those ends the sequence and is left in place. Reading on to the next
/// letter would let `ESC [` followed by a space or a line break swallow the first letter of
/// whatever came next, and with it the name a credential rule needs: `ESC [ API_KEY=x` came back
/// as `PI_KEY=x`. Without `intermediates` an intermediate byte ends the sequence the same way,
/// which is the reading that keeps that name; [`stderr_text`] weighs it against the standard one.
fn skip_csi_body(characters: &mut std::iter::Peekable<std::str::Chars<'_>>, intermediates: bool) {
    let mut in_parameters = true;
    while let Some(next) = characters.peek() {
        if in_parameters && ('\u{30}'..='\u{3f}').contains(next) {
            characters.next();
            continue;
        }
        if intermediates && ('\u{20}'..='\u{2f}').contains(next) {
            in_parameters = false;
            characters.next();
            continue;
        }
        if ('\u{40}'..='\u{7e}').contains(next) {
            characters.next();
        }
        return;
    }
}

/// Every code point a diagnostic must not carry across a boundary, tab and newline excepted.
///
/// One list rather than a second opinion: the ranges are the ones
/// [`normalize`](crate::normalize) applies to every other vendor-written string.
fn is_unsafe_to_render(character: char) -> bool {
    let code = u32::from(character);
    matches!(
        code,
        0x00..=0x1f | 0x7f..=0x9f | 0x061c | 0x200e | 0x200f | 0x202a..=0x202e | 0x2066..=0x2069
    )
}

#[cfg(test)]
mod tests {
    use super::{program_name, stderr_text};

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
    fn program_name_keeps_known_vendors_and_omits_arbitrary_basenames() {
        assert_eq!(program_name("/private/bin/codex"), "codex");
        assert_eq!(program_name(r"C:\\private\\bin\\CLAUDE.EXE"), "CLAUDE.EXE");
        assert_eq!(
            program_name("/private/bin/customer-secret-canary"),
            "custom executable"
        );
        assert_eq!(
            program_name("/private/bin/token=secret"),
            "custom executable"
        );
    }

    /// The ACP adapters a launch failure could otherwise only call `custom executable`.
    ///
    /// This name is the only statement of *which* agent failed to start: the rest of a
    /// `Error::Launch` diagnostic says what stopped it, not whose CLI it was.
    #[test]
    fn program_name_keeps_every_built_in_acp_executable() {
        for program in [
            "gemini",
            "copilot",
            "goose",
            "codex-acp",
            "claude-agent-acp",
        ] {
            assert_eq!(
                program_name(&format!("/private/bin/{program}")),
                program,
                "expected {program} to be named, received a redacted stand-in"
            );
        }
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
        assert_eq!(
            stderr_text("a_token_count_of=3"),
            "a_token_count_of=[REDACTED]"
        );
    }

    /// The spellings credentials actually have. Each of these walked past the scanner while an
    /// underscore counted as a word character and the keyword had to touch the separator.
    #[test]
    fn redacts_a_credential_named_the_way_credentials_are_named() {
        for (line, expected) in [
            ("OPENAI_API_KEY=sk-proj-x", "OPENAI_API_KEY=[REDACTED]"),
            ("ANTHROPIC_API_KEY=sk-ant-x", "ANTHROPIC_API_KEY=[REDACTED]"),
            ("GITHUB_TOKEN=ghp_x", "GITHUB_TOKEN=[REDACTED]"),
            (
                "AWS_SECRET_ACCESS_KEY=wJalrX",
                "AWS_SECRET_ACCESS_KEY=[REDACTED]",
            ),
            ("CLIENT_SECRET=shh", "CLIENT_SECRET=[REDACTED]"),
            ("my_token=v", "my_token=[REDACTED]"),
        ] {
            assert_eq!(stderr_text(line), expected, "expected {expected:?}");
        }
    }

    #[test]
    fn a_colour_sequence_does_not_hide_the_token_that_follows_it() {
        assert_eq!(
            stderr_text("Authorization:\u{1b}[1m Bearer sk-live-x"),
            "Authorization: Bearer [REDACTED]"
        );
        assert_eq!(
            stderr_text("\u{1b}[31mAPI_KEY\u{1b}[0m=secret-value"),
            "API_KEY=[REDACTED]"
        );
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
    fn a_url_whose_userinfo_is_only_a_password_is_redacted() {
        assert_eq!(
            stderr_text("redis://:hunter2@db/main"),
            "redis://:[REDACTED]@db/main"
        );
    }

    #[test]
    fn strips_control_characters_but_keeps_tabs_and_newlines() {
        let redacted = stderr_text("a\u{1b}[31mb\u{0}c\td\ne\u{9f}f");
        assert_eq!(redacted, "abc\td\nef");
    }

    /// The tail is vendor-written text rendered in a host's diagnostics, so it is bounded on the
    /// same terms as every other vendor string. An override left in a log renders a filename, or
    /// the command that produced it, in an order its code points do not have.
    #[test]
    fn strips_the_bidirectional_set_a_log_would_otherwise_render_backwards() {
        for code in [0x061c, 0x200e, 0x200f, 0x202a, 0x202e, 0x2066, 0x2069] {
            let character = char::from_u32(code).expect("expected a character");
            assert_eq!(
                stderr_text(&format!("error: cannot run {character}gpj.exe")),
                "error: cannot run gpj.exe",
                "expected U+{code:04X} to be stripped"
            );
        }
    }

    /// The same header carrying the same credential. A base64 `user:password` is no less a secret
    /// for being the older spelling, and it reached a log while only `bearer` was matched.
    #[test]
    fn redacts_a_basic_credential_as_well_as_a_bearer_one() {
        assert_eq!(
            stderr_text("Authorization: Basic dXNlcjpodW50ZXIy"),
            "Authorization: Basic [REDACTED]"
        );
        assert_eq!(
            stderr_text("authorization:basic\tdXNlcjpodW50ZXIy"),
            "authorization:basic [REDACTED]"
        );
    }

    #[test]
    fn redacts_multiline_credentials_after_stderr_arrives_in_chunks() {
        let tail = concat!(
            "request failed\nAuthorization:\n",
            "  Bearer multiline-secret\n",
            "retry at https://user:url-secret@agent.internal"
        );
        let redacted = stderr_text(tail);

        for secret in ["multiline-secret", "url-secret"] {
            assert!(
                !redacted.contains(secret),
                "expected no credential from chunked stderr, received {redacted:?}"
            );
        }
        assert!(
            redacted.contains("request failed"),
            "expected safe diagnostic context, received {redacted:?}"
        );
    }

    #[test]
    fn keeps_text_that_carries_no_credential() {
        let line = "error: the vendor exited with status 1 (no configuration found)";
        assert_eq!(stderr_text(line), line);
    }

    /// After `ESC [` the stripper must stop at the first byte that cannot belong to the sequence
    /// and leave it alone. A space or a line break used to be skipped over, and the next letter
    /// was taken as the final byte: the first letter of a credential's name.
    #[test]
    fn a_malformed_escape_sequence_does_not_hide_a_credential_name() {
        for (raw, cause) in [
            ("\u{1b}[ API_KEY=secret", "a space"),
            ("\u{1b}[\nAPI_KEY=secret", "a line break"),
            ("\u{1b}[1;\nAPI_KEY=secret", "parameters then a line break"),
            ("\u{1b}[\u{7}TOKEN=secret", "a control character"),
        ] {
            let redacted = stderr_text(raw);
            assert!(
                !redacted.contains("secret"),
                "expected the value redacted after an escape sequence broken by {cause} | received {redacted:?}"
            );
        }
    }

    /// A complete sequence with an intermediate byte is also legal, and when the letter after it
    /// begins a credential name the two readings disagree; the one that hides more is returned.
    #[test]
    fn a_complete_intermediate_sequence_before_a_name_does_not_hide_the_value() {
        for raw in [
            "\u{1b}[ qAPI_KEY=secret",
            "\u{1b}[0 qTOKEN=secret",
            "\u{1b}[1;2 @password=secret",
        ] {
            let redacted = stderr_text(raw);
            assert!(
                !redacted.contains("secret"),
                "expected the value redacted after a complete sequence with an intermediate byte | received {redacted:?}"
            );
        }
        assert_eq!(
            stderr_text("\u{1b}[0 qready"),
            "ready",
            "expected a complete intermediate sequence taken out whole"
        );
    }

    #[test]
    fn a_complete_escape_sequence_is_still_taken_out_whole() {
        assert_eq!(
            stderr_text("\u{1b}[1;31mred\u{1b}[0m and \u{1b}[2Kdone\u{1b}[31"),
            "red and done"
        );
    }

    #[test]
    fn survives_multibyte_text() {
        assert_eq!(
            stderr_text("não encontrado: token=café"),
            "não encontrado: token=[REDACTED]"
        );
    }
}
