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
/// Each one was observed in a real capture: `email` and `accountId` identify the account,
/// `installationId` and `serverName` identify the machine, `codexHome`, `path` and
/// `instructionSources` carry the user's home directory, `userAgent` carries the operating system
/// build, `originUrl` names the person's own remote — and with it, usually, the person — and
/// `agentNickname` is a name they chose themselves.
///
/// A value being null in the capture at hand is not a reason to leave it off: the list is what the
/// next capture is scrubbed against, and the next capture is on somebody else's machine.
const REDACTED_MEMBERS: &[&str] = &[
    "accountId",
    "agentNickname",
    "codexHome",
    "email",
    "installationId",
    "instructionSources",
    "originUrl",
    "path",
    "serverName",
    "userAgent",
];

/// Members redacted only inside one notification family.
///
/// `mcpServer/startupStatus/updated` names the user's own configured servers, and spells that name
/// `name` where every other frame spells it `serverName`. `name` is far too ordinary a member to
/// redact everywhere — it would take the model tier list and the host's own `clientInfo.name` with
/// it — so the family it appears in is part of the rule.
const REDACTED_IN_FAMILY: &[(&str, &[&str])] = &[("mcpServer/", &["name"])];

/// What a redacted value is replaced with.
pub const PLACEHOLDER: &str = "[REDACTED]";

/// The working directory every captured path is rewritten to.
pub const FIXTURE_CWD: &str = "/workspace";

/// The home directory every captured path is rewritten to.
pub const FIXTURE_HOME: &str = "/home/user";

/// The account name every captured mention of the operator is rewritten to.
pub const FIXTURE_USER: &str = "user";

/// The shortest login name worth rewriting.
///
/// A two-character login is a substring of half the English language, and replacing it everywhere
/// would corrupt the transcript it was meant to clean. A capture from such an account keeps the
/// name, which is the lesser of the two failures — and is why the value is a constant with this
/// paragraph next to it rather than a silent `if`.
const SHORTEST_REWRITABLE_LOGIN: usize = 3;

/// The three literals a capture rewrites out of every string it records.
///
/// Literals, not patterns: the capture knows exactly which directories and which account it ran
/// under, and rewriting those is a different thing from guessing which of a vendor's strings looks
/// like a path or a name.
///
/// The account name is here because a fixture is not only what the vendor wrote. A turn runs
/// commands, and the vendor reports what they printed — `ls -la` in the review scenario, whose
/// output carries the owner of every file in it. No denylist of member names reaches that, because
/// the member is `aggregatedOutput` and its value is the thing the fixture exists to record.
#[derive(Clone, Copy, Debug, Default)]
pub struct Paths<'a> {
    /// The directory the capture ran in, which becomes [`FIXTURE_CWD`].
    pub cwd: &'a str,
    /// The user's home directory, which becomes [`FIXTURE_HOME`].
    pub home: &'a str,
    /// The account the capture ran as, which becomes [`FIXTURE_USER`].
    pub user: &'a str,
}

/// One frame, with everything identifying taken out of it.
///
/// The named members lose their values outright; every other string has the capture's own two
/// directories and its account name rewritten, so a replayed fixture describes a workspace rather
/// than somebody's disk.
#[must_use]
pub fn frame(value: Value, paths: Paths<'_>) -> Value {
    // Read once, at the top, because a member's family is a fact about the frame rather than about
    // the object it happens to sit in.
    let family: Vec<&str> = value
        .get("method")
        .and_then(Value::as_str)
        .map(|method| {
            REDACTED_IN_FAMILY
                .iter()
                .filter(|(prefix, _)| method.starts_with(prefix))
                .flat_map(|(_, members)| members.iter().copied())
                .collect()
        })
        .unwrap_or_default();
    scrub(value, paths, &family)
}

fn scrub(value: Value, paths: Paths<'_>, family: &[&str]) -> Value {
    match value {
        Value::Object(members) => Value::Object(
            members
                .into_iter()
                .map(|(key, value)| {
                    let named =
                        REDACTED_MEMBERS.contains(&key.as_str()) || family.contains(&key.as_str());
                    if named && !value.is_null() {
                        return (key, Value::String(String::from(PLACEHOLDER)));
                    }
                    (key, scrub(value, paths, family))
                })
                .collect::<Map<String, Value>>(),
        ),
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| scrub(item, paths, family))
                .collect(),
        ),
        Value::String(text) => Value::String(rewrite_paths(&text, paths)),
        other => other,
    }
}

/// The capture's own directories and account name, replaced wherever they appear in one string.
///
/// The working directory first: it usually sits inside the home directory, and rewriting the home
/// prefix first would leave a `/home/user/...` the workspace rule no longer recognises. The
/// account name last, for the same reason — both directories usually contain it.
fn rewrite_paths(text: &str, paths: Paths<'_>) -> String {
    let mut rewritten = String::from(text);
    if !paths.cwd.is_empty() {
        rewritten = rewritten.replace(paths.cwd, FIXTURE_CWD);
    }
    if !paths.home.is_empty() {
        rewritten = rewritten.replace(paths.home, FIXTURE_HOME);
    }
    if paths.user.len() >= SHORTEST_REWRITABLE_LOGIN {
        rewritten = rewritten.replace(paths.user, FIXTURE_USER);
    }
    rewritten
}

#[cfg(test)]
mod tests {
    use super::{FIXTURE_CWD, FIXTURE_HOME, FIXTURE_USER, PLACEHOLDER, Paths, frame};
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

    /// The two that reached a checked-in fixture before this list grew. `accountId` names the
    /// account as surely as its address does, and `originUrl` names the person's own remote — and
    /// with it, on any host that puts a username in the URL, the person.
    #[test]
    fn a_thread_keeps_its_shape_and_loses_the_account_and_the_remote() {
        let redacted = frame(
            json!({"result": {"thread": {
                "id": "01a09999-7858",
                "accountId": "22c05f8e-5615-4da1-985f-f70d9f88b751",
                "agentNickname": "ada's codex",
                "gitInfo": {"branch": "main", "originUrl": "git@github.com:ada/thing.git",
                            "sha": "5c9a929"},
            }}}),
            Paths::default(),
        );

        let thread = &redacted["result"]["thread"];
        assert_eq!(thread["accountId"], PLACEHOLDER);
        assert_eq!(thread["agentNickname"], PLACEHOLDER);
        assert_eq!(thread["gitInfo"]["originUrl"], PLACEHOLDER);
        assert_eq!(
            thread["id"], "01a09999-7858",
            "expected the thread a replay addresses to survive"
        );
        assert_eq!(
            thread["gitInfo"]["branch"], "main",
            "expected what the vendor reported about the tree to survive"
        );
    }

    /// Which servers a person has configured is a fact about their installation. The vendor
    /// spells it `name` in this one family, and `name` everywhere else is ordinary.
    #[test]
    fn the_servers_a_person_configured_lose_their_names_and_nothing_else_does() {
        let redacted = frame(
            json!({"method": "mcpServer/startupStatus/updated",
                   "params": {"name": "exa", "status": "starting"}}),
            Paths::default(),
        );
        assert_eq!(redacted["params"]["name"], PLACEHOLDER);
        assert_eq!(redacted["params"]["status"], "starting");

        let kept = frame(
            json!({"id": 0, "method": "initialize",
                   "params": {"clientInfo": {"name": "mea"}}}),
            Paths::default(),
        );
        assert_eq!(
            kept["params"]["clientInfo"]["name"], "mea",
            "expected the host's own name, which the compliance log reads, to survive"
        );
    }

    /// The vendor reports what a command printed, and a command that lists files prints the name
    /// of whoever owns them. No denylist of member names reaches that: the member is the output
    /// itself, which is the thing the fixture exists to record.
    #[test]
    fn the_account_a_capture_ran_as_is_rewritten_out_of_what_a_command_printed() {
        let redacted = frame(
            json!({"params": {"item": {
                "aggregatedOutput": "drwxr-xr-x 2 ada ada 40 Sep 13 05:41 .\n",
            }}}),
            Paths {
                cwd: "/home/ada/code/thing",
                home: "/home/ada",
                user: "ada",
            },
        );

        assert_eq!(
            redacted["params"]["item"]["aggregatedOutput"],
            format!("drwxr-xr-x 2 {FIXTURE_USER} {FIXTURE_USER} 40 Sep 13 05:41 .\n")
        );
    }

    /// A login short enough to be a substring of ordinary words is left alone: a transcript
    /// corrupted by its own scrubbing is worse than one naming an account.
    #[test]
    fn a_login_too_short_to_rewrite_safely_is_left_where_it_is() {
        let redacted = frame(
            json!({"params": {"item": {"aggregatedOutput": "an ad for adobe, owned by ad\n"}}}),
            Paths {
                cwd: "",
                home: "",
                user: "ad",
            },
        );

        assert_eq!(
            redacted["params"]["item"]["aggregatedOutput"],
            "an ad for adobe, owned by ad\n"
        );
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
                user: "",
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
                user: "",
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
