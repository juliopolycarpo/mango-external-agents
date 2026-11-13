//! What a host-supplied HTTP MCP endpoint must look like before a vendor is told to dial it.
//!
//! [`McpTransport::Http`](crate::McpTransport) carries a URL and a header map the host wrote. A
//! harness copies both into a vendor's configuration, where the vendor's own HTTP client reads
//! them. A URL with user information, a fragment or a control character, or a header a client
//! cannot encode, is refused here, before a vendor process can observe it.
//!
//! The grammar is deliberately narrower than RFC 3986: an ASCII `http` or `https` URL whose host
//! is a registered name or a bracketed IPv6 literal. The ACP and Codex harnesses apply the same
//! grammar. The tests hold it against the `http`
//! crate, which the harnesses used before: nothing accepted here is something that crate refuses.

/// The longest URL [`is_http_url`] reads, in bytes.
pub const HTTP_URL_MAX_BYTES: usize = 65_534;

/// The longest header name [`is_http_field_name`] reads, in bytes.
pub const HTTP_FIELD_NAME_MAX_BYTES: usize = 65_535;

/// Whether `raw` is an absolute `http` or `https` URL a vendor can be handed as an MCP endpoint.
///
/// Accepted: a case-insensitive `http://` or `https://`, a host that is a registered name
/// (ASCII letters, digits, `-`, `.`, `_`) or a bracketed IPv6 literal, an optional decimal port
/// that fits sixteen bits, then an optional path and query of printable ASCII.
///
/// Refused: user information (`user@host`), a fragment, whitespace, control characters, any
/// non-ASCII character (percent-encode it), and a port that is empty or out of range.
/// Paths also refuse `"`, `{` and `}`; bracketed hosts refuse IPv6 zone identifiers such as
/// `[fe80::1%25eth0]`.
///
/// # Example
///
/// ```
/// use mango_external_agents::http_endpoint::is_http_url;
///
/// assert!(is_http_url("https://mcp.example.com/mcp?tenant=docs"));
/// assert!(is_http_url("http://[::1]:8080/mcp"));
/// assert!(!is_http_url("https://user:secret@mcp.example.com/mcp"));
/// assert!(!is_http_url("https://mcp.example.com/mcp#section"));
/// assert!(!is_http_url("ftp://mcp.example.com/mcp"));
/// ```
#[must_use]
pub fn is_http_url(raw: &str) -> bool {
    if raw.len() > HTTP_URL_MAX_BYTES {
        return false;
    }
    let Some(rest) = strip_prefix_ignoring_case(raw, "https://")
        .or_else(|| strip_prefix_ignoring_case(raw, "http://"))
    else {
        return false;
    };
    let (authority, path_and_query) = rest.split_at(rest.find(['/', '?']).unwrap_or(rest.len()));
    is_authority(authority) && is_path_and_query(path_and_query)
}

/// Whether `name` is an HTTP field name: a non-empty RFC 9110 token.
///
/// # Example
///
/// ```
/// use mango_external_agents::http_endpoint::is_http_field_name;
///
/// assert!(is_http_field_name("X-Docs_Key"));
/// assert!(!is_http_field_name("X Docs"));
/// assert!(!is_http_field_name(""));
/// ```
#[must_use]
pub fn is_http_field_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= HTTP_FIELD_NAME_MAX_BYTES
        && name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

/// Whether `value` is an HTTP field value: no control character except a horizontal tab.
///
/// A carriage return or a line feed would end the field and start another, which is how a header
/// value becomes a second header.
///
/// # Example
///
/// ```
/// use mango_external_agents::http_endpoint::is_http_field_value;
///
/// assert!(is_http_field_value("Bearer abc.def"));
/// assert!(!is_http_field_value("abc\r\nX-Injected: 1"));
/// ```
#[must_use]
pub fn is_http_field_value(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte == b'\t' || (byte >= 0x20 && byte != 0x7f))
}

/// `text` without `prefix`, compared without regard to ASCII case.
fn strip_prefix_ignoring_case<'text>(text: &'text str, prefix: &str) -> Option<&'text str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &text[prefix.len()..])
}

/// A host and an optional port, with no user information.
fn is_authority(authority: &str) -> bool {
    let (host, after_host) = if let Some(bracketed) = authority.strip_prefix('[') {
        let Some((literal, after_host)) = bracketed.split_once(']') else {
            return false;
        };
        if literal.parse::<std::net::Ipv6Addr>().is_err() {
            return false;
        }
        (literal, after_host)
    } else {
        let (host, after_host) = authority.split_at(authority.find(':').unwrap_or(authority.len()));
        let is_registered_name = host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_'));
        if !is_registered_name {
            return false;
        }
        (host, after_host)
    };
    if host.is_empty() {
        return false;
    }
    match after_host.strip_prefix(':') {
        // `u16::from_str` alone would read `+80`, so the digits are checked first.
        Some(port) => {
            !port.is_empty()
                && port.bytes().all(|byte| byte.is_ascii_digit())
                && port.parse::<u16>().is_ok()
        }
        None => after_host.is_empty(),
    }
}

/// An empty remainder, or a path starting with `/`, or a query starting with `?`.
fn is_path_and_query(path_and_query: &str) -> bool {
    let (path, query) = path_and_query
        .split_once('?')
        .map_or((path_and_query, ""), |(path, query)| (path, query));
    path.bytes().all(is_path_byte) && query.bytes().all(is_query_byte)
}

/// The ASCII bytes a path may hold.
///
/// Printable ASCII without the characters a client could read as the end of the URL or of the
/// path: space, `"`, `#`, `<`, `>`, `?`, the backtick, `{` and `}`.
const fn is_path_byte(byte: u8) -> bool {
    matches!(byte, 0x21 | 0x24..=0x3b | 0x3d | 0x40..=0x5f | 0x61..=0x7a | 0x7c | 0x7e)
}

/// The ASCII bytes a query may hold: the path set, plus `?`, the backtick, `{` and `}`.
const fn is_query_byte(byte: u8) -> bool {
    matches!(byte, 0x21 | 0x24..=0x3b | 0x3d | 0x3f..=0x7e)
}

#[cfg(test)]
mod tests {
    use super::{
        HTTP_FIELD_NAME_MAX_BYTES, HTTP_URL_MAX_BYTES, is_http_field_name, is_http_field_value,
        is_http_url,
    };

    /// The check both HTTP-capable harnesses ran before this module existed, on the `http` crate.
    fn previous_http_url_check(url: &str) -> bool {
        let Ok(uri) = url.parse::<http::Uri>() else {
            return false;
        };
        let Some(authority) = uri.authority() else {
            return false;
        };
        let host = authority.host();
        let Some(after_host) = authority.as_str().strip_prefix(host) else {
            return false;
        };
        matches!(uri.scheme_str(), Some("http" | "https"))
            && !host.is_empty()
            && !authority.as_str().contains('@')
            && (after_host.is_empty()
                || (after_host.starts_with(':') && authority.port_u16().is_some()))
            && !url.contains('#')
            && url
                .chars()
                .all(|character| !character.is_control() && !character.is_whitespace())
    }

    /// Every ASCII character and a few beyond it, as the text to drop into a URL or a header.
    fn probe_characters() -> impl Iterator<Item = char> {
        (0u8..=0x7f)
            .map(char::from)
            .chain(['\u{80}', '\u{a0}', 'é', '\u{202e}', '日', '\u{1f600}'])
    }

    #[test]
    fn ordinary_mcp_endpoints_are_accepted() {
        for url in [
            "https://mcp.notion.com/mcp",
            "http://localhost:3000",
            "HTTPS://Docs.Example/mcp",
            "https://docs.example",
            "https://docs.example/",
            "https://docs.example?tenant=a",
            "https://docs_internal.example:8443/v1/mcp?tenant=a&mode=b%20c",
            "http://127.0.0.1:65535/mcp",
            "http://[::1]/mcp",
            "http://[::1]:0/mcp",
            "http://[::1]:65535/mcp",
            "http://[2001:db8::1]:8080/mcp",
            "http://[::ffff:192.0.2.1]/mcp",
            "http://[::1:2:3:4:5:6:7]/mcp",
            "http://[1:2:3:4:5:6:7::]/mcp",
            "http://[::ffff:0:0:0:0:0:1]/mcp",
        ] {
            assert!(
                is_http_url(url),
                "expected {url:?} to be an accepted HTTP MCP URL | received a refusal"
            );
            assert!(
                previous_http_url_check(url),
                "expected the http crate to accept {url:?} too | received a refusal"
            );
        }
    }

    #[test]
    fn urls_a_vendor_must_not_dial_are_refused() {
        for url in [
            "",
            "http://",
            "https:///mcp",
            "ftp://example.com/mcp?secret=canary",
            "docs.example/mcp",
            "//docs.example/mcp",
            "https://user@docs.example/mcp",
            "https://user:secret@docs.example/mcp",
            "https://docs.example/mcp#fragment",
            "https://docs.example/mcp endpoint",
            "https://docs.example/mcp\tendpoint",
            "https://docs.example/mcp\nendpoint",
            "https://docs.example:/mcp",
            "https://docs.example:+80/mcp",
            "https://docs.example:65536/mcp",
            "https://docs.example:80:90/mcp",
            "https://[::1/mcp",
            "https://[]/mcp",
            "https://[abc]/mcp",
            "https://[1::2::3]/mcp",
            "https://[1:2:3:4:5:6:7:8:9]/mcp",
            "https://[12345::1]/mcp",
            "https://[::ffff:192.0.2.256]/mcp",
            "https://[::1]:/mcp",
            "https://[::1]:65536/mcp",
            "https://[::1]x/mcp",
            "https://[::1:2:3:4:5:6:7:8]/mcp",
            "https://[fe80::1%25eth0]/mcp",
            "https://docs.example/{\"a\":1}",
            "https://docs.example/\"q\"",
            "https://docs.exämple/mcp",
            "https://docs.example/mcp/é",
        ] {
            assert!(
                !is_http_url(url),
                "expected {url:?} to be refused as an HTTP MCP URL | received an acceptance"
            );
        }
    }

    #[test]
    fn a_url_longer_than_the_cap_is_refused_and_one_at_the_cap_is_not() {
        let prefix = "https://docs.example/";
        let at_cap = format!("{prefix}{}", "a".repeat(HTTP_URL_MAX_BYTES - prefix.len()));
        assert!(is_http_url(&at_cap));
        assert!(previous_http_url_check(&at_cap));
        assert!(!is_http_url(&format!("{at_cap}a")));
    }

    /// The property that makes this module a safe replacement: it is at most as permissive as the
    /// `http` crate check it replaced, at every position of a URL and for every ASCII character.
    #[test]
    fn nothing_is_accepted_that_the_http_crate_refused() {
        let templates = [
            "{}",
            "{}://docs.example/mcp",
            "http{}://docs.example/mcp",
            "https:{}//docs.example/mcp",
            "https://{}",
            "https://{}/mcp",
            "https://{}docs.example/mcp",
            "https://docs{}example/mcp",
            "https://docs.example{}",
            "https://docs.example{}/mcp",
            "https://docs.example:{}/mcp",
            "https://docs.example:80{}/mcp",
            "https://docs.example:{}80/mcp",
            "https://[{}]/mcp",
            "https://[::1{}]/mcp",
            "https://[::1]{}/mcp",
            "https://[::1]:{}/mcp",
            "https://docs.example/{}",
            "https://docs.example/mcp{}",
            "https://docs.example/m{}cp?tenant=a",
            "https://docs.example/mcp?{}",
            "https://docs.example/mcp?tenant={}a",
            "https://docs.example?{}",
        ];
        let mut accepted = 0usize;
        for template in templates {
            for first in probe_characters() {
                // Pairs as well as single characters, so `%41`, `::`, `..` and `[]` shapes occur.
                for second in [
                    None,
                    Some(first),
                    Some(':'),
                    Some('.'),
                    Some('/'),
                    Some('@'),
                ] {
                    let mut inserted = String::from(first);
                    inserted.extend(second);
                    let url = template.replace("{}", &inserted);
                    if is_http_url(&url) {
                        accepted += 1;
                        assert!(
                            previous_http_url_check(&url),
                            "expected a URL the http crate accepts | received {url:?}, which it refuses"
                        );
                    }
                }
            }
        }
        assert!(
            accepted > 1_000,
            "expected the probe to exercise over 1000 accepted URLs | received {accepted}"
        );
    }

    #[test]
    fn field_names_match_the_http_crate_for_every_probe_character() {
        for character in probe_characters() {
            for name in [String::from(character), format!("X-{character}Docs")] {
                assert_eq!(
                    is_http_field_name(&name),
                    http::header::HeaderName::from_bytes(name.as_bytes()).is_ok(),
                    "expected the http crate's verdict on field name {name:?} | received the opposite"
                );
            }
        }
        assert!(!is_http_field_name(""));
        assert!(is_http_field_name("X-Docs_Key"));
        assert!(!is_http_field_name("X Docs"));
        assert!(!is_http_field_name("X:Docs"));
        let at_cap = "a".repeat(HTTP_FIELD_NAME_MAX_BYTES);
        assert_eq!(
            is_http_field_name(&at_cap),
            http::header::HeaderName::from_bytes(at_cap.as_bytes()).is_ok()
        );
        let over_cap = format!("{at_cap}a");
        assert!(!is_http_field_name(&over_cap));
        assert!(http::header::HeaderName::from_bytes(over_cap.as_bytes()).is_err());
    }

    #[test]
    fn field_values_match_the_http_crate_for_every_probe_character() {
        for character in probe_characters() {
            for value in [String::from(character), format!("Bearer {character}token")] {
                assert_eq!(
                    is_http_field_value(&value),
                    http::header::HeaderValue::from_str(&value).is_ok(),
                    "expected the http crate's verdict on field value {value:?} | received the opposite"
                );
            }
        }
        assert!(is_http_field_value(""));
        assert!(!is_http_field_value("token\u{7f}"));
        assert!(!is_http_field_value("abc\r\nX-Injected: 1"));
    }
}
