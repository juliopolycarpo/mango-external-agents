//! The texts the searches read: four families, each built to reach a different part of the
//! readers, and the findings of the reviews as a list of their own.
//!
//! The statements family writes a credential's name, a separator and a value the way a vendor
//! does, with a canary (`QXZ<n>J`) where a credential would be, so that a canary that survives
//! says where. The structures family puts those texts into JSON, up to six levels deep. The
//! fragments family concatenates pieces at random (control bytes, escapes, markers, token
//! prefixes). The soup family mixes the differential's fragments with bytes the readers react to.

use super::Random;
use crate::redact::differential::{FRAGMENTS, SHAPES};

/// A value no rule keeps and no text holds by chance, for the families that have no canaries.
pub(super) const SECRET: &str = "SEKRET1";

/// The names a statement is built from.
const NAMES: &[&str] = &[
    "tokens",
    "token",
    "password",
    "passwd",
    "pwd",
    "api_key",
    "apiKey",
    "secret",
    "Cookie",
    "Set-Cookie",
    "credential",
    "x-api-key",
    "private_key",
    "client_secret",
    "access_token",
    "session_cookie",
    "tokens_used",
    "max_tokens",
    "input_tokens",
    "Authorization",
    "auth",
    "sig",
    "pass",
    "API_KEY",
    "Proxy-Authorization",
    "key",
    "user",
    "note",
];

const SEPARATORS: &[&str] = &[
    ":",
    "=",
    ": ",
    " = ",
    ":\n",
    "=\n",
    ":\n  ",
    " :",
    ": \n",
    " ",
    "=\"",
    ": Bearer ",
    ": Basic ",
];

/// How a value is written. `<c>` is a canary, `<q>` the quote the name was written in, `<k>` a
/// private-key block.
const VALUES: &[&str] = &[
    "<c>",
    "<c> <c>",
    "null",
    "not set",
    "12",
    "token: <c>",
    "<c> password:",
    "<c> secret=",
    "[<c>, <c>]",
    "[\"<c>\", \"<c>\"]",
    "[<c>, password:",
    "[\n  <c>,\n  <c>\n]",
    "{\"a\": \"<c>\"}",
    "{<c> secret=",
    "{\"k\": \"<c>\", \"password\": \"<c>\"}",
    "- <c>",
    "- <c> secret=",
    "- \"<c>\"",
    "- <c>\n- <c>",
    "\n  - <c>\n  - <c>",
    "\n  - <c> password:",
    "\n  - <c> token:\n  <c>",
    "\"<c> <c>\"",
    "\"<c> secret=",
    "'<c> <c>'",
    "'<c> password:",
    "|\n  <c>",
    "|\n  <c>\n  <c>\n<c>",
    ">\n  <c> <c>",
    "|-\n    <c>\n  <c>\n <c>",
    ">+\n\t<c>\n\t<c>",
    "|<c>",
    "|token: <c>",
    ">token: <c>",
    "(<c>)",
    "<c> token <c>",
    "- <c> key - <c>",
    "<c>; <c>",
    "<c>, <c>",
    "<c>&x=<c>",
    "[<c>\n<c>\n<c>",
    "{\n \"a\": [\n  \"<c>\"\n ],\n \"b\": \"<c>\"\n}",
    "<c>] <c>",
    "<c>} <c>",
    "[<c>] password: <c>",
    "- <c>\npassword: <c>",
    "- <c>\n  token:\n    <c>",
    "pwd: token: <c>",
    "passwd:\ntoken: <c>",
    "[[<c>], {\"x\": [<c>]}]",
    "<c>\\n<c>",
    "<c>\\\"<c>",
    "\\\"<c>\\\" <c>",
    "a=b; sid=<c>; x=<c>",
    // A sequence item that is a name, and one that ends in one, at any depth of escaping.
    "- <c> token:",
    "- token:",
    "- <q>token<q>:",
    "- <c> <q>secret<q>:",
    "- <c> token: <c>token:",
    "<q>secret<q>: token: <c>",
    "<q>secret<q>=secret=",
    "<q>secret<q>:<q>token<q>:",
    // A block scalar that holds a name, quoted or not, whose value is on the next line.
    "|\n  <c>\n  token:",
    "|\n  <c>\n  <q>token<q>:",
    "|\n  <c> <q>token<q>:\n<c>",
    "|\n  line1\n  token:",
    "|\n <q>token:",
    // A key block inside a value, and names glued after the close of a container.
    "<k>",
    "\"<k>",
    "<q>token<q>: <k>",
    "[\n <c>\n]password: <c>",
    "{\n<c>\n}secret: <c>",
    "[<c>]<q>token<q> :<c>",
    "[<c>\n]token :<c>",
    "- A\n- B",
    "- <c>\n- <c> token:",
];

const KEYS: &[&str] = &[
    concat!("-----BEGIN ", "PRIVATE KEY-----"),
    concat!("-----BEGIN ", "RSA PRIVATE KEY-----"),
    concat!("-----BEGIN ", "OPENSSH PRIVATE KEY-----"),
    concat!("-----BEGIN ", "EC PRIVATE KEY-----"),
];

/// Base64 of nothing: the shape of a key's body lines.
const BODY: &str = "MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQC7VJTUt9Us8cKj";

const ENDS: &[&str] = &[
    "-----END PRIVATE KEY-----",
    "-----END RSA PRIVATE KEY-----",
    "",
    "",
    "-----END",
];

const URLS: &[&str] = &[
    "https://user:<c>@host/p",
    "postgres://token:<c>@db/x?password=<c>",
    "https://h/p?api_key=<c>&x=<c>",
    "redis://:<c>@h",
    "https://password:<c>@h/<c>",
    "ftp://u:<c>@h token=<c>",
    "x://token:;<c>@host",
];

const QUOTES: &[&str] = &[
    "",
    "",
    "",
    "",
    "\"",
    "'",
    "\\\"",
    "\\\\\"",
    "\\\\\\\"",
    "\\\\\\\\\\\\\\\"",
];

const BREAKS: &[&str] = &[
    "\n", "\n", " ", ", ", "; ", "\n  ", "\r\n", " & ", "\n\n", "", "]", "}", "\"", "\\\"", "\n- ",
    "\t",
];

const WRAPPERS: &[&str] = &[
    "[<t>]",
    "{<t>}",
    "{\"m\": \"<t>\"}",
    "- <t>",
    "x: |\n  <t>",
    "\"<t>\"",
    "error: <t>",
    "<t>\n]",
];

/// A text of statements, and how many canaries it holds.
pub(super) fn statements(random: &mut Random) -> String {
    let mut canaries = 0;
    let mut text = String::new();
    if random.below(100) < 12 {
        text.push_str(&match random.below(3) {
            0 => format!("{} ", canary(&mut canaries)),
            1 => format!(
                "{}{}",
                key_block(random, &mut canaries),
                random.pick(BREAKS)
            ),
            _ => format!("{}{} ", concat!("gh", "p_"), "a1B2".repeat(9)),
        });
    }
    for index in 0..1 + random.below(3) {
        if index > 0 {
            text.push_str(random.pick(BREAKS));
        }
        match random.below(20) {
            0 => text.push_str(&key_block(random, &mut canaries)),
            1 => text.push_str(&filled(random.pick(URLS), "", random, &mut canaries)),
            _ => text.push_str(&statement(random, &mut canaries)),
        }
        if random.below(100) < 60 {
            text.push_str(random.pick(BREAKS));
            text.push_str(&canary(&mut canaries));
            text.push(' ');
        }
    }
    if random.below(100) < 90 {
        // Most are bare, as a vendor's line is; the rest sit in a container or a block.
    } else {
        let wrapper = random.pick(WRAPPERS);
        text = wrapper.replace("<t>", &text);
    }
    if random.below(100) < 5 {
        text = json_string(&text);
    }
    if random.below(100) < 3 {
        let inner = format!("{{\"note\":{},\"n\":1}}", json_string(&text));
        let twice = json_string(&inner);
        text = twice[1..twice.len() - 1].to_owned();
    }
    text
}

fn statement(random: &mut Random, canaries: &mut usize) -> String {
    let quote = *random.pick(QUOTES);
    let name = random.pick(NAMES);
    let separator = random.pick(SEPARATORS);
    let mut value = (*random.pick(VALUES)).to_owned();
    if random.below(100) < 7 {
        value = value.replacen("<c>", &format!("<c>{}", random.pick(&["]", "}", " ]"])), 1);
    }
    if random.below(100) < 7 {
        let tail = if random.below(3) == 0 { "" } else { ":" };
        value = format!("{value} {}{tail}", random.pick(NAMES));
    }
    if random.below(100) < 5 {
        value = format!("{value}{}<k>", random.pick(BREAKS));
    }
    let value = filled(&value, quote, random, canaries);
    format!("{quote}{name}{quote}{separator}{value}")
}

/// `template` with its canaries, quotes and key blocks written.
fn filled(template: &str, quote: &str, random: &mut Random, canaries: &mut usize) -> String {
    let mut out = String::new();
    let mut rest = template;
    while let Some(at) = rest.find('<') {
        let (before, marker) = rest.split_at(at);
        out.push_str(before);
        if let Some(after) = marker.strip_prefix("<c>") {
            out.push_str(&canary(canaries));
            rest = after;
        } else if let Some(after) = marker.strip_prefix("<q>") {
            out.push_str(quote);
            rest = after;
        } else if let Some(after) = marker.strip_prefix("<k>") {
            out.push_str(&key_block(random, canaries));
            rest = after;
        } else {
            out.push('<');
            rest = &marker[1..];
        }
    }
    out.push_str(rest);
    out
}

fn canary(canaries: &mut usize) -> String {
    let label = format!("QXZ{canaries}J");
    *canaries += 1;
    label
}

fn key_block(random: &mut Random, canaries: &mut usize) -> String {
    let begin = random.pick(KEYS);
    let body = match random.below(9) {
        0 => format!("\n{BODY}\n"),
        1 => format!("\n{BODY}\n{BODY}\n"),
        2 => BODY.to_owned(),
        3 => format!("\n{BODY}]\n{BODY}\n"),
        4 => format!("\n{BODY}}}\n"),
        5 => format!("\n{}\n{BODY}\n", canary(canaries)),
        6 => format!("\n{BODY}\n{}\n", canary(canaries)),
        7 => format!(" {} ", canary(canaries)),
        _ => format!("\\n{BODY}\\n"),
    };
    let end = random.pick(ENDS);
    let glued = if random.below(6) == 0 {
        *random.pick(NAMES)
    } else {
        ""
    };
    format!("{begin}{body}{end}{glued}")
}

/// `text` as a JSON string, quotes included.
pub(super) fn json_string(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_default()
}

/// A statements text, put into JSON between one and six levels deep, as a field of an object or an
/// entry of an array, beside numbers a parser could round and members the walk reads.
pub(super) fn structures(random: &mut Random) -> String {
    let mut text = statements(random);
    if random.below(100) < 20 {
        text.push_str(random.pick(&[
            "\u{e9}",
            "\u{1f600}",
            "\\u0041",
            "\\ud83d\\ude00",
            "\\",
            "\"",
            "\u{2028}",
        ]));
    }
    for _ in 0..1 + random.below(6) {
        let string = json_string(&text);
        let string = if random.below(2) == 0 {
            ascii_only(&string)
        } else {
            string
        };
        let (comma, colon) = if random.below(2) == 0 {
            (",", ":")
        } else {
            (", ", ": ")
        };
        text = match random.below(6) {
            0 => format!(
                "{{\"note\"{colon}{string}{comma}\"name\"{colon}\"x\"{comma}\"n\"{colon}1}}"
            ),
            1 => format!("[{string}{comma}{{\"k\"{colon}\"x\"}}]"),
            2 => format!(
                "{{\"a\"{colon}{{\"b\"{colon}[1{comma}{string}]}}{comma}\"z\"{colon}100000000000000000000000000001}}"
            ),
            3 => format!(
                "{{\"error\"{colon}{{\"message\"{colon}{string}{comma}\"code\"{colon}1.50}}}}"
            ),
            4 => format!("{{\"name\"{colon}\"x\"{comma}\"value\"{colon}{string}}}"),
            _ if random.below(10) < 3 => {
                format!("{{\"m\"{colon}{string}{comma}\"password\"{colon}\"p\"}}")
            }
            _ => format!("{{\"m\"{colon}{string}}}"),
        };
    }
    text
}

/// A JSON text with every non-ASCII character written as the `\u` escape for it.
fn ascii_only(json: &str) -> String {
    let mut out = String::with_capacity(json.len());
    for character in json.chars() {
        if character.is_ascii() {
            out.push(character);
            continue;
        }
        let mut units = [0u16; 2];
        for unit in character.encode_utf16(&mut units) {
            out.push_str(&format!("\\u{unit:04x}"));
        }
    }
    out
}

/// Pieces concatenated at random: names, separators, quotes at every depth, control bytes, escape
/// sequences, markers, token prefixes.
const PIECES: &[&str] = &[
    "token",
    "tokens",
    "password",
    "pwd",
    "passwd",
    "secret",
    "api_key",
    "Cookie",
    "Set-Cookie",
    "Authorization",
    "Bearer",
    "Basic",
    "auth",
    "sig",
    "pass",
    "max_tokens",
    "private_key",
    "credential",
    ":",
    "=",
    ": ",
    "= ",
    ":\n",
    "=\n",
    " ",
    "  ",
    "\n",
    "\r\n",
    "\r",
    "\t",
    ",",
    ";",
    "&",
    "?",
    "#",
    "@",
    "/",
    "//",
    "://",
    "-",
    "- ",
    "--",
    "-----",
    "_",
    ".",
    "+",
    "|",
    ">",
    "|-",
    ">+",
    "|2",
    "# c",
    "[",
    "]",
    "{",
    "}",
    "(",
    ")",
    "\"",
    "'",
    "\\",
    "\\\\",
    "\\\"",
    "\\\\\"",
    "\\n",
    "\\r",
    "\\t",
    "\\u0041",
    "\\ud83d",
    "\x00",
    "\x01",
    "\x07",
    "\x0b",
    "\x0c",
    "\x1b",
    "\x1b[",
    "\x1b[31m",
    "\x1b]0;",
    "\x1b\\",
    "\x7f",
    "\u{9b}",
    "\u{9c}",
    "\u{9d}",
    "\u{90}",
    "\u{e9}",
    "\u{61c}",
    "\u{3042}",
    "\u{202e}",
    "\u{2014}",
    "\u{1f600}",
    "\u{fffd}",
    "\u{feff}",
    "[REDACTED]",
    "[REDACTED",
    "REDACTED]",
    "-----BEGIN ",
    "-----END ",
    "PRIVATE KEY",
    "PRIVATE KEY BLOCK-----",
    "==",
    "ghp_",
    "github_pat_",
    "glpat-",
    "xoxb-",
    "sk-ant-",
    "sk-ant-api03-",
    "sk-proj-",
    "sk-live-",
    "sk_live_",
    "sk-",
    "sk-svcacct-",
    "sk-admin-",
    "AKIA",
    "AIza",
    "ya29.",
    "eyJ",
    "npm_",
    "hf_",
    "a1B2c3D4e5F6g7H8i9J0",
    "0123456789ABCDEF",
    "abcdefghij",
    "1234567890123",
    "12",
    "null",
    "true",
    "x",
    "QXZ0J",
    "https://",
    "user:",
    "host",
    "not set",
    "",
    "",
];

const LONG_RUNS: &[usize] = &[31, 32, 33];

pub(super) fn fragments(random: &mut Random) -> String {
    let mut text = String::new();
    for _ in 0..1 + random.below(30) {
        match random.below(60) {
            0 => text.push_str(&"\\".repeat(*random.pick(LONG_RUNS))),
            1 => text.push_str(concat!("-----BEGIN ", "PRIVATE KEY-----")),
            2 => text.push_str(concat!("-----BEGIN ", "RSA PRIVATE KEY-----")),
            3 => text.push_str(concat!("-----END ", "PRIVATE KEY-----")),
            4 => text.push_str(BODY),
            5 => text.push_str(&BODY[..16]),
            _ => text.push_str(random.pick(PIECES)),
        }
    }
    match random.below(12) {
        0 => json_string(&text),
        1 => format!("{{\"m\":{},\"n\":1}}", json_string(&text)),
        2 => format!(
            "[{},{{\"password\":{}}}]",
            json_string(&text),
            json_string(&text)
        ),
        3 => json_string(&format!("{{\"k\":{}}}", json_string(&text))),
        _ => text,
    }
}

/// The differential's fragments, token pieces and a soup of the bytes the readers react to.
const SOUP: &[u8] = b"authorizationbearerbasicapikeysecrettokenpasswordcredential \
    ::==  \n\n\r\t,;-_.+/@?#19^AUTHZBS\x00\x01\x07\x0b\x0c\x1b\x1b[]\\\\\\()PXcmq\x7f\"\"''{}[]ghp_sk-eyJ.\
    \xc2\x9b\xc2\x9c\xc2\x9d\xc2\x90\xc2\xa0\xc3\xa9\xd8\x9c\xe3\x81\x82\xe2\x80\xae\xe2\x80\x94\xf0\x9f\x98\x80\xff\xc3";

/// Pieces of the well-known tokens, split so no line holds a whole one.
const TOKEN_PIECES: &[&str] = &[
    concat!("gh", "p_"),
    concat!("github_p", "at_"),
    concat!("glp", "at-"),
    concat!("xo", "xb-"),
    concat!("sk", "-ant-"),
    concat!("sk", "-proj-"),
    concat!("sk", "-svcacct-"),
    concat!("sk", "-admin-"),
    concat!("sk", "_live_"),
    concat!("AK", "IA"),
    concat!("AI", "za"),
    concat!("ya", "29."),
    concat!("ey", "J"),
    concat!("np", "m_"),
    concat!("-----BEGIN ", "PRIVATE KEY-----"),
    concat!("-----BEGIN ", "RSA PRIVATE KEY-----"),
    "-----END PRIVATE KEY-----",
    "0123456789abcdefghijklmnopqrstuvwxyz",
    "ABCDEFGHIJKLMNOPQRST",
    "0123456789ABCDEFGHIJ",
    "xxxxxxxxxxxxxxxxxxxx",
    "\\n",
    "\\\\n",
    "\n",
    ".",
    SECRET,
];

pub(super) fn soup(random: &mut Random) -> String {
    let mut text = String::new();
    for _ in 0..1 + random.below(24) {
        match random.below(10) {
            0..=3 => text.push_str(random.pick(FRAGMENTS)),
            4 => text.push_str(random.pick(SHAPES)),
            5..=6 => text.push_str(random.pick(TOKEN_PIECES)),
            _ => {
                let bytes: Vec<u8> = (0..1 + random.below(12))
                    .map(|_| *random.pick(SOUP))
                    .collect();
                text.push_str(&String::from_utf8_lossy(&bytes));
            }
        }
    }
    text
}

/// What the reviews of this redactor found, as the text that showed it. `<k>` is a private-key
/// block, `<ghp>` a token. Every one is hidden, and stable, by every entry point.
pub(super) const FINDINGS: &[&str] = &[
    "secrets:\n  - token:\n      QXZ1J",
    "env:\n  secrets:\n    - api_key:\n        QXZ2J",
    "credentials:\n  - password:\n      QXZ3J\n    user: bob",
    "\"token\": - J token:\nQXZ4J",
    "\\\\\"secret\\\\\": token: QXZ5J",
    "secret\\\\\"=secret=\nQXZ6J",
    "password:\n  - abc\n  - def secret=\nQXZ7J",
    "password: |\n  line1\n  token:\nQXZ8J",
    "token:|\n \"token:\nQXZ9J",
    "{\"m\":\"Authorization\\\":J\\nQXZ10J\\\"\"}",
    "token=\"<k>\": QXZ11J",
    "passwd:<k>:\nQXZ12J",
    "token:[<b>]<e>]token :QXZ13J",
    "tokens: [\n a\n]password: QXZ14J",
    "token: {\na\n}secret: QXZ15J",
    "token=- token: QXZ16Jtoken:",
    "{\"m\":\"token=- A\\n- B\",\"password\":\"p\"}",
    "{\"m\":\"token=- QXZ17J\\n- QXZ18J\",\"password\":\"p\"}",
    "{\"total_tokens\":{\"<ghp>\":1}}",
    "{\"note\":\"\u{e9} token=QXZ19J \u{e9}\",\"n\":1}",
];

/// [`FINDINGS`] with the blocks and tokens written.
pub(super) fn findings() -> Vec<String> {
    let key = format!(
        "{}\n{BODY}\n{BODY}\n{}",
        concat!("-----BEGIN ", "PRIVATE KEY-----"),
        concat!("-----END ", "PRIVATE KEY-----")
    );
    let token = format!("{}{}", concat!("gh", "p_"), "a1B2".repeat(9));
    FINDINGS
        .iter()
        .map(|text| {
            text.replace("<k>", &key)
                .replace("<b>", concat!("-----BEGIN ", "PRIVATE KEY-----"))
                .replace("<e>", concat!("-----END ", "PRIVATE KEY-----"))
                .replace("<ghp>", &token)
        })
        .collect()
}
