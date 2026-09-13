//! What must not reach a checked-in fixture.
//!
//! A captured transcript is a real conversation with a real account on a real machine, and the
//! rule that fixtures are never hand-edited means the scrubbing has to happen here rather than in
//! a text editor afterwards. Two kinds of thing go: values a vendor writes that identify a person
//! or an installation, and paths that carry a home directory.
//!
//! This is deliberately a denylist of field names rather than a pattern search. A pattern that
//! looked for anything email-shaped would also rewrite the body of a turn, and a fixture whose
//! content was silently altered is worse than no fixture.

use serde_json::{Map, Value};

/// The members whose values are replaced wherever they appear, at any depth.
///
/// Each one was observed in a real capture: `email` identifies the account, `installationId` and
/// `serverName` identify the machine, `codexHome`, `path` and `instructionSources` carry the
/// user's home directory, and `userAgent` carries the operating system build.
const REDACTED_MEMBERS: &[&str] = &[
    "codexHome",
    "email",
    "installationId",
    "instructionSources",
    "path",
    "serverName",
    "userAgent",
];

/// What a redacted value is replaced with.
pub const PLACEHOLDER: &str = "[REDACTED]";

/// The working directory every captured path is rewritten to.
pub const FIXTURE_CWD: &str = "/workspace";

/// The home directory every captured path is rewritten to.
pub const FIXTURE_HOME: &str = "/home/user";

/// The two directories a capture rewrites out of every string it records.
///
/// Prefixes, not patterns: the capture knows exactly which two paths it ran under, and rewriting
/// those is a different thing from guessing which of a vendor's strings looks like a path.
#[derive(Clone, Copy, Debug, Default)]
pub struct Paths<'a> {
    /// The directory the capture ran in, which becomes [`FIXTURE_CWD`].
    pub cwd: &'a str,
    /// The user's home directory, which becomes [`FIXTURE_HOME`].
    pub home: &'a str,
}

/// One frame, with everything identifying taken out of it.
///
/// The named members lose their values outright; every other string has the capture's own two
/// directories rewritten, so a replayed fixture describes a workspace rather than somebody's disk.
#[must_use]
pub fn frame(value: Value, paths: Paths<'_>) -> Value {
    match value {
        Value::Object(members) => Value::Object(
            members
                .into_iter()
                .map(|(key, value)| {
                    if REDACTED_MEMBERS.contains(&key.as_str()) && !value.is_null() {
                        return (key, Value::String(String::from(PLACEHOLDER)));
                    }
                    (key, frame(value, paths))
                })
                .collect::<Map<String, Value>>(),
        ),
        Value::Array(items) => {
            Value::Array(items.into_iter().map(|item| frame(item, paths)).collect())
        }
        Value::String(text) => Value::String(rewrite_paths(&text, paths)),
        other => other,
    }
}

/// The capture's own directories, replaced wherever they appear in one string.
///
/// The working directory first: it usually sits inside the home directory, and rewriting the home
/// prefix first would leave a `/home/user/...` the workspace rule no longer recognises.
fn rewrite_paths(text: &str, paths: Paths<'_>) -> String {
    let mut rewritten = String::from(text);
    if !paths.cwd.is_empty() {
        rewritten = rewritten.replace(paths.cwd, FIXTURE_CWD);
    }
    if !paths.home.is_empty() {
        rewritten = rewritten.replace(paths.home, FIXTURE_HOME);
    }
    rewritten
}

#[cfg(test)]
mod tests {
    use super::{FIXTURE_CWD, FIXTURE_HOME, PLACEHOLDER, Paths, frame};
    use serde_json::json;

    /// The real `account/read` answer, which is where an address would otherwise land in the repo.
    #[test]
    fn an_account_answer_keeps_its_shape_and_loses_the_person() {
        let redacted = frame(
            json!({"id": 1, "result": {"account": {"type": "chatgpt",
                                                   "email": "someone@example.com",
                                                   "planType": "plus"},
                                       "requiresOpenaiAuth": true}}),
            Paths::default(),
        );

        assert_eq!(redacted["result"]["account"]["email"], PLACEHOLDER);
        assert_eq!(redacted["result"]["account"]["type"], "chatgpt");
        assert_eq!(redacted["result"]["requiresOpenaiAuth"], true);
    }

    /// A home directory reaches a fixture through several members at once, so the capture's own
    /// working directory is rewritten wherever it appears rather than only where it was expected.
    #[test]
    fn the_directory_the_capture_ran_in_becomes_a_workspace_everywhere_it_appears() {
        let redacted = frame(
            json!({"params": {"cwd": "/home/ada/code/thing",
                              "item": {"command": "ls /home/ada/code/thing/src"}}}),
            Paths {
                cwd: "/home/ada/code/thing",
                home: "/home/ada",
            },
        );

        assert_eq!(redacted["params"]["cwd"], FIXTURE_CWD);
        assert_eq!(
            redacted["params"]["item"]["command"], "ls /workspace/src",
            "expected the path inside a command to be rewritten too"
        );
    }

    /// A denylist of names, not a pattern search: rewriting anything email-shaped would also
    /// rewrite the body of a turn, and a fixture whose content was altered is worse than none.
    #[test]
    fn text_that_merely_looks_identifying_is_left_exactly_as_it_was() {
        let text = "write to someone@example.com about the release";
        let redacted = frame(json!({"params": {"delta": text}}), Paths::default());
        assert_eq!(redacted["params"]["delta"], text);
    }

    #[test]
    fn a_member_the_vendor_left_null_stays_null_rather_than_becoming_a_placeholder() {
        let redacted = frame(json!({"params": {"serverName": null}}), Paths::default());
        assert!(
            redacted["params"]["serverName"].is_null(),
            "expected an absent value to stay absent, received {redacted}"
        );
    }

    /// The working directory usually sits inside the home directory. Rewriting the home prefix
    /// first would leave a `/home/user/...` the workspace rule no longer recognises.
    #[test]
    fn a_workspace_inside_a_home_directory_is_rewritten_as_a_workspace() {
        let redacted = frame(
            json!({"cwd": "/home/ada/code/thing", "other": "/home/ada/.codex/AGENTS.md"}),
            Paths {
                cwd: "/home/ada/code/thing",
                home: "/home/ada",
            },
        );
        assert_eq!(redacted["cwd"], FIXTURE_CWD);
        assert_eq!(
            redacted["other"],
            format!("{FIXTURE_HOME}/.codex/AGENTS.md")
        );
    }

    #[test]
    fn identifying_members_are_found_however_deep_they_sit() {
        let redacted = frame(
            json!({"a": {"b": [{"installationId": "d91a5f34", "keep": 1}]}}),
            Paths::default(),
        );
        assert_eq!(redacted["a"]["b"][0]["installationId"], PLACEHOLDER);
        assert_eq!(redacted["a"]["b"][0]["keep"], 1);
    }
}
