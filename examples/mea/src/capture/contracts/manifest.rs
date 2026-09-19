//! The digests a capture manifest carries, and the check that reads them back.
//!
//! "Captured by `mea capture`, never hand-edited" is the rule every fixture in this repository is
//! under, and a manifest that only names the probes it ran claims nothing about the bytes beside
//! it. Every manifest this module writes carries one SHA-256 digest per captured file, in the
//! `files` shape the historical Claude manifest already used, and [`verify`] recomputes them — so
//! an edited fixture is a failing test rather than a diff nobody reads.
//!
//! [`refresh`] is the same computation applied to a manifest that is already committed. It is how
//! the digests reach a capture nobody can re-run: it reads the files as they stand and writes what
//! they hash to, which records a fact about the tree rather than inventing a capture.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use mango_external_agents::error::{Error, Result};
use serde_json::{Map, Value};

/// The manifest member holding one digest per captured file.
const FILES: &str = "files";

/// The file that carries a capture directory's manifest.
pub const MANIFEST: &str = "manifest.json";

/// `sha256:<hex>` over `bytes`, the spelling every manifest already uses.
///
/// Example: `sha256(b"abc")` is `sha256:ba7816bf…`, pinned by the NIST vector below.
fn sha256(bytes: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
    let mut hex = String::from("sha256:");
    for byte in digest.as_ref() {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// Every `manifest.json` under `root`, in a stable order.
///
/// # Examples
///
/// `manifests(Path::new("fixtures"))` returns the four committed capture manifests.
///
/// # Errors
///
/// Returns the filesystem error when a directory under `root` cannot be read.
pub fn manifests(root: &Path) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    collect_manifests(root, &mut found)?;
    found.sort();
    Ok(found)
}

fn collect_manifests(directory: &Path, found: &mut Vec<PathBuf>) -> Result<()> {
    for entry in read_directory(directory)? {
        let path = entry.path();
        if path.is_dir() {
            collect_manifests(&path, found)?;
            continue;
        }
        if entry.file_name() == MANIFEST {
            found.push(path);
        }
    }
    Ok(())
}

/// The digest of every captured file beside a manifest in `directory`.
///
/// The manifest itself is excluded: it is the document making the claim, and a digest of a file
/// that carries its own digest can never be written.
///
/// # Examples
///
/// `digests(Path::new("fixtures/claude/contract"))` names `cli-surface.json`, `help.txt` and
/// `version.json`.
///
/// # Errors
///
/// Returns the filesystem error when the directory or one of its files cannot be read, and refuses
/// a capture directory holding a subdirectory: a nested file would be claimed by nothing.
pub fn digests(directory: &Path) -> Result<Map<String, Value>> {
    let mut digests = BTreeMap::new();
    for entry in read_directory(directory)? {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == MANIFEST {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            return Err(Error::HostConfiguration {
                expected: "a capture directory holding only the files its manifest declares",
                received: format!("{} is a directory", path.display()),
            });
        }
        digests.insert(name, Value::String(sha256(&read_file(&path)?)));
    }
    Ok(digests.into_iter().collect())
}

/// Writes `directory/manifest.json` from `fields`, plus the digest of every file beside it.
///
/// Called last by each capture, once the files it describes have been written.
///
/// # Examples
///
/// `write(&contract, &json!({"format": 1}))` writes a manifest whose `files` member holds the
/// digest of every other file in `contract`.
///
/// # Errors
///
/// Returns an error when `fields` is not a JSON object, or when the directory cannot be read or
/// written.
pub fn write(directory: &Path, fields: &Value) -> Result<()> {
    let mut members = fields
        .as_object()
        .cloned()
        .ok_or_else(|| Error::HostConfiguration {
            expected: "a manifest built from a JSON object",
            received: shape_of(fields),
        })?;
    members.insert(String::from(FILES), Value::Object(digests(directory)?));
    super::write_json(&directory.join(MANIFEST), &Value::Object(members))
}

/// Recomputes one committed manifest's digests from the files beside it.
///
/// Returns whether the manifest changed. A manifest whose digests are already current is left
/// untouched, so running this over the tree never rewrites a capture it agrees with.
///
/// # Examples
///
/// `refresh(Path::new("fixtures/codex/contract/manifest.json"))` answers `false` once the
/// committed digests describe the committed files.
///
/// # Errors
///
/// Returns an error when the manifest is absent, is not a JSON object, or when the files beside it
/// cannot be read.
pub fn refresh(manifest_path: &Path) -> Result<bool> {
    let directory = capture_directory(manifest_path)?;
    let mut members = manifest_members(manifest_path)?;
    let digests = Value::Object(digests(directory)?);
    if members.get(FILES) == Some(&digests) {
        return Ok(false);
    }
    members.insert(String::from(FILES), digests);
    super::write_json(manifest_path, &Value::Object(members))?;
    Ok(true)
}

/// Holds one committed manifest to the files beside it.
///
/// Every file in the capture directory has to be declared and has to hash to what the manifest
/// says, in both directions: a file nobody declared is as unclaimed as a digest nobody checked.
///
/// # Examples
///
/// `verify(Path::new("fixtures/claude/historical/contract/manifest.json"))` is how the committed
/// digests of a capture nobody can re-run are proved to still describe it.
///
/// # Errors
///
/// Returns [`Error::HostConfiguration`] naming every file that disagrees, with the digest the
/// manifest declared and the digest the file has.
pub fn verify(manifest_path: &Path) -> Result<()> {
    let directory = capture_directory(manifest_path)?;
    let declared = declared_digests(manifest_path)?;
    let present = digests(directory)?;

    let mut disagreements = Vec::new();
    for (name, digest) in &present {
        match declared.get(name) {
            None => disagreements.push(format!(
                "{name}: expected a declared digest, received an undeclared file hashing to {}",
                text_of(digest)
            )),
            Some(expected) if expected != digest => disagreements.push(format!(
                "{name}: expected {}, received {}",
                text_of(expected),
                text_of(digest)
            )),
            Some(_) => {}
        }
    }
    for name in declared.keys() {
        if !present.contains_key(name) {
            disagreements.push(format!(
                "{name}: expected the declared file, received no such file beside the manifest"
            ));
        }
    }
    if disagreements.is_empty() {
        return Ok(());
    }
    Err(Error::HostConfiguration {
        expected: "every captured file to match the digest its manifest declares",
        received: format!("{}: {}", manifest_path.display(), disagreements.join("; ")),
    })
}

/// The `files` member of a committed manifest.
fn declared_digests(manifest_path: &Path) -> Result<Map<String, Value>> {
    let members = manifest_members(manifest_path)?;
    members
        .get(FILES)
        .and_then(Value::as_object)
        .cloned()
        .ok_or_else(|| Error::HostConfiguration {
            expected: "a capture manifest declaring a files digest map",
            received: format!(
                "{} with members {}",
                manifest_path.display(),
                member_names(&members)
            ),
        })
}

fn manifest_members(manifest_path: &Path) -> Result<Map<String, Value>> {
    let document = read_file(manifest_path)?;
    let parsed: Value =
        serde_json::from_slice(&document).map_err(|error| Error::HostConfiguration {
            expected: "a capture manifest holding a JSON object",
            received: format!("{}: {error}", manifest_path.display()),
        })?;
    parsed
        .as_object()
        .cloned()
        .ok_or_else(|| Error::HostConfiguration {
            expected: "a capture manifest holding a JSON object",
            received: format!("{}: {}", manifest_path.display(), shape_of(&parsed)),
        })
}

fn capture_directory(manifest_path: &Path) -> Result<&Path> {
    manifest_path
        .parent()
        .ok_or_else(|| Error::HostConfiguration {
            expected: "a manifest inside a capture directory",
            received: manifest_path.display().to_string(),
        })
}

fn read_directory(directory: &Path) -> Result<Vec<std::fs::DirEntry>> {
    let entries = std::fs::read_dir(directory).map_err(|error| Error::HostConfiguration {
        expected: "a readable capture directory",
        received: format!("{}: {error}", directory.display()),
    })?;
    entries
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| Error::HostConfiguration {
            expected: "a readable capture directory",
            received: format!("{}: {error}", directory.display()),
        })
}

fn read_file(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|error| Error::HostConfiguration {
        expected: "a readable captured file",
        received: format!("{}: {error}", path.display()),
    })
}

/// A JSON string's text, and the value itself for anything else a manifest should not hold.
fn text_of(value: &Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), str::to_owned)
}

fn shape_of(value: &Value) -> String {
    match value {
        Value::Object(members) => format!("object with members {}", member_names(members)),
        Value::Array(items) => format!("array with {} item(s)", items.len()),
        Value::String(_) => String::from("string"),
        Value::Number(_) => String::from("number"),
        Value::Bool(_) => String::from("boolean"),
        Value::Null => String::from("null"),
    }
}

fn member_names(members: &Map<String, Value>) -> String {
    let mut names: Vec<&str> = members.keys().map(String::as_str).collect();
    names.sort_unstable();
    names.join(", ")
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::{Value, json};

    use super::{MANIFEST, digests, manifests, refresh, sha256, verify, write};

    /// The repository root, from this crate's own manifest.
    ///
    /// Joined one segment at a time and never canonicalised, for the reason
    /// `tests/field_inventory.rs` gives: a `\\?\` verbatim path on Windows does not normalise a
    /// forward slash underneath it.
    fn fixtures() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("fixtures")
    }

    fn temp_dir(label: &str) -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "mea-manifest-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("expected a temporary capture directory");
        path
    }

    /// NIST's own one-block vector, so the spelling in every manifest is pinned to SHA-256 rather
    /// than to whatever this crate happens to hash with.
    #[test]
    fn a_digest_is_sha256_of_the_bytes_with_the_algorithm_named() {
        assert_eq!(
            sha256(b"abc"),
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    /// The tree check. It names the four committed manifests rather than whatever a walk happens
    /// to find: a walk that finds nothing would otherwise pass while proving nothing.
    #[test]
    fn every_committed_capture_declares_the_digest_of_every_file_beside_it() {
        let root = fixtures();
        let found: Vec<String> = manifests(&root)
            .expect("expected the committed fixture tree to be readable")
            .iter()
            .map(|path| {
                path.strip_prefix(&root)
                    .unwrap_or(path)
                    .display()
                    .to_string()
                    .replace('\\', "/")
            })
            .collect();

        assert_eq!(
            found,
            vec![
                String::from("acp/opencode/contract/manifest.json"),
                String::from("claude/contract/manifest.json"),
                String::from("claude/historical/contract/manifest.json"),
                String::from("codex/contract/manifest.json"),
            ],
            "expected the four committed capture manifests, received {found:?}"
        );

        let failures: Vec<String> = manifests(&root)
            .expect("expected the committed fixture tree to be readable")
            .iter()
            .filter_map(|path| verify(path).err())
            .map(|error| error.to_string())
            .collect();
        assert!(
            failures.is_empty(),
            "expected every committed fixture to match its manifest, received:\n{}",
            failures.join("\n")
        );
    }

    /// The historical Claude capture also carries an aggregate `checksum`, written by whatever
    /// recorded the session in 2026-09. Nothing in this repository can recompute it — it is not
    /// the digest of the committed files in any order, of their digests, or of the manifest — so
    /// it is held to its shape and the per-file digests carry the integrity claim.
    #[test]
    fn the_historical_aggregate_checksum_keeps_a_sha256_shape() {
        let manifest = fixtures()
            .join("claude")
            .join("historical")
            .join("contract")
            .join(MANIFEST);
        let document = std::fs::read_to_string(&manifest).expect("expected the historical capture");
        let parsed: Value = serde_json::from_str(&document).expect("expected a JSON manifest");
        let checksum = parsed["checksum"]
            .as_str()
            .expect("expected an aggregate checksum");

        let hex = checksum
            .strip_prefix("sha256:")
            .unwrap_or_else(|| panic!("expected a sha256: prefix, received {checksum}"));
        assert!(
            hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "expected 64 hexadecimal digits, received {checksum}"
        );
    }

    #[test]
    fn a_changed_captured_byte_names_the_file_and_both_digests() {
        let directory = temp_dir("changed");
        std::fs::write(directory.join("version.json"), "{\"output\":\"1.0.0\"}\n")
            .expect("expected a captured file");
        write(&directory, &json!({"format": 1})).expect("expected a manifest");
        std::fs::write(directory.join("version.json"), "{\"output\":\"9.9.9\"}\n")
            .expect("expected an edited capture");

        let error = verify(&directory.join(MANIFEST))
            .expect_err("expected an edited fixture to be refused");

        let reported = error.to_string();
        assert!(
            reported.contains("version.json")
                && reported.contains(&sha256(b"{\"output\":\"1.0.0\"}\n"))
                && reported.contains(&sha256(b"{\"output\":\"9.9.9\"}\n")),
            "expected the file with its declared and actual digest, received {reported}"
        );
        std::fs::remove_dir_all(directory).expect("expected temporary capture cleanup");
    }

    #[test]
    fn a_file_no_manifest_declares_is_refused() {
        let directory = temp_dir("undeclared");
        std::fs::write(directory.join("version.json"), "{}\n").expect("expected a captured file");
        write(&directory, &json!({"format": 1})).expect("expected a manifest");
        std::fs::write(directory.join("extra.json"), "{}\n").expect("expected a stray file");

        let error = verify(&directory.join(MANIFEST))
            .expect_err("expected an undeclared file to be refused");

        assert!(
            error.to_string().contains("extra.json"),
            "expected the undeclared file to be named, received {error}"
        );
        std::fs::remove_dir_all(directory).expect("expected temporary capture cleanup");
    }

    #[test]
    fn a_declared_file_that_is_gone_is_refused() {
        let directory = temp_dir("missing");
        std::fs::write(directory.join("version.json"), "{}\n").expect("expected a captured file");
        write(&directory, &json!({"format": 1})).expect("expected a manifest");
        std::fs::remove_file(directory.join("version.json")).expect("expected a removed capture");

        let error = verify(&directory.join(MANIFEST))
            .expect_err("expected a missing capture to be refused");

        assert!(
            error.to_string().contains("version.json"),
            "expected the absent file to be named, received {error}"
        );
        std::fs::remove_dir_all(directory).expect("expected temporary capture cleanup");
    }

    /// `refresh` is what writes digests into a capture nobody can re-run, so it has to leave every
    /// other member of that manifest — and their order — exactly as captured.
    #[test]
    fn refreshing_rewrites_only_the_digests_and_leaves_the_capture_alone() {
        let directory = temp_dir("refresh");
        std::fs::write(directory.join("version.json"), "{}\n").expect("expected a captured file");
        let manifest = directory.join(MANIFEST);
        std::fs::write(
            &manifest,
            "{\n  \"set\": \"claude-cli\",\n  \"capturedAt\": \"2026-09-04\"\n}\n",
        )
        .expect("expected a manifest with no digests");

        assert!(refresh(&manifest).expect("expected a refresh"));
        assert_eq!(
            std::fs::read_to_string(&manifest).expect("expected the refreshed manifest"),
            format!(
                "{{\n  \"set\": \"claude-cli\",\n  \"capturedAt\": \"2026-09-04\",\n  \"files\": {{\n    \"version.json\": \"{}\"\n  }}\n}}\n",
                sha256(b"{}\n")
            )
        );
        assert!(
            !refresh(&manifest).expect("expected a second refresh"),
            "expected a current manifest to be left alone"
        );
        verify(&manifest).expect("expected a refreshed manifest to verify");
        std::fs::remove_dir_all(directory).expect("expected temporary capture cleanup");
    }

    #[test]
    fn a_nested_directory_beside_a_manifest_is_refused() {
        let directory = temp_dir("nested");
        std::fs::create_dir_all(directory.join("inner")).expect("expected a nested directory");

        let error = digests(&directory).expect_err("expected a nested capture to be refused");

        assert!(
            error.to_string().contains("inner"),
            "expected the nested directory to be named, received {error}"
        );
        std::fs::remove_dir_all(directory).expect("expected temporary capture cleanup");
    }
}
