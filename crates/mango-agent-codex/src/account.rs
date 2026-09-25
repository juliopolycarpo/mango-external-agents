//! A stable, host-keyed identity for the signed-in Codex account, without the address.
//!
//! A host that keeps a Codex conversation across restarts has to notice when the account behind
//! it changed: a continuation resumed under somebody else's sign-in is a different person's
//! thread, and a quota cached under the old account is the wrong quota. The only non-secret
//! account identity the app-server reports is the email `account/read` returns for a ChatGPT
//! sign-in, and an email is personal data that has no reason to leave this library.
//!
//! So the address is read once, from the documented `account/read` answer, and used for one thing:
//! the input to an HMAC-SHA256 under a key the host supplies and keeps on its own machine. What
//! crosses to the host is the digest. A plain hash would not do — anyone holding it could test a
//! guessed address against it offline — while a keyed one is stable and comparable for the host
//! that computed it and meaningless to anybody else. The address is never stored in a value this
//! crate returns, never formatted, and never forwarded; no credential is read at all.
//!
//! The digest is `hex(HMAC-SHA256(key, "codex:" + email))`, cut to 32 hex characters: the value
//! the TypeScript adapter produced, so a host migrating from it keeps matching its stored ones.

use std::fmt;

use mango_external_agents::error::{Error, Result};
use mango_external_agents::normalize::{self, TextLimit};

/// The domain the digest input is prefixed with, so the same key digests another vendor's
/// account to a different value.
const DOMAIN: &str = "codex:";

/// How many hex characters of the digest are kept.
const FINGERPRINT_HEX_CHARS: usize = 32;

/// The host's own key for account fingerprints.
///
/// Opaque bytes the host derives and keeps on its machine. `Debug` never prints them.
///
/// # Example
///
/// ```
/// use mango_agent_codex::account::AccountFingerprintKey;
///
/// let key = AccountFingerprintKey::new(b"host-local-key").expect("a non-empty key");
/// assert!(!format!("{key:?}").contains("host-local-key"));
/// ```
#[derive(Clone, PartialEq, Eq)]
pub struct AccountFingerprintKey(Vec<u8>);

impl AccountFingerprintKey {
    /// Wraps the host's key.
    ///
    /// # Errors
    ///
    /// [`Error::HostConfiguration`] for an empty key, which would make the digest a plain hash of
    /// the address.
    pub fn new(key: impl AsRef<[u8]>) -> Result<Self> {
        let key = key.as_ref();
        if key.is_empty() {
            return Err(Error::HostConfiguration {
                expected: "a non-empty account fingerprint key",
                received: String::from("an empty key"),
            });
        }
        Ok(Self(key.to_vec()))
    }

    /// The fingerprint of one account address under this key.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_agent_codex::account::AccountFingerprintKey;
    ///
    /// let key = AccountFingerprintKey::new(b"host-local-key").unwrap();
    /// assert_eq!(key.fingerprint("a@example.com"), key.fingerprint("a@example.com"));
    /// assert_ne!(key.fingerprint("a@example.com"), key.fingerprint("b@example.com"));
    /// ```
    #[must_use]
    pub fn fingerprint(&self, email: &str) -> AccountFingerprint {
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &self.0);
        let tag = ring::hmac::sign(&key, format!("{DOMAIN}{email}").as_bytes());
        let hex: String = tag
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        AccountFingerprint(hex[..FINGERPRINT_HEX_CHARS].to_owned())
    }
}

impl fmt::Debug for AccountFingerprintKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AccountFingerprintKey")
            .field("bytes", &self.0.len())
            .finish()
    }
}

/// A keyed digest that identifies one account to the host that holds the key.
///
/// Compare two for equality to tell whether the account changed; the text is for storage.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct AccountFingerprint(String);

impl AccountFingerprint {
    /// The digest as 32 lowercase hex characters.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for AccountFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AccountFingerprint(..)")
    }
}

/// What discovery learned about the signed-in Codex account, beyond whether one is signed in.
#[derive(Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct CodexAccount {
    /// The ChatGPT plan the vendor named, bounded as a label.
    pub plan_type: Option<String>,
    /// The account's keyed fingerprint, when the sign-in reported an address.
    pub fingerprint: Option<AccountFingerprint>,
}

impl fmt::Debug for CodexAccount {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CodexAccount")
            .field("has_plan_type", &self.plan_type.is_some())
            .field("has_fingerprint", &self.fingerprint.is_some())
            .finish()
    }
}

impl CodexAccount {
    /// The account facts from one raw `account/read` answer, for a ChatGPT sign-in only.
    ///
    /// The address is borrowed from `response` for the digest and goes nowhere else. `None` for
    /// an API-key or Bedrock account, or no account, which have no address to fingerprint.
    #[must_use]
    pub fn from_account_read(
        response: &serde_json::Value,
        key: &AccountFingerprintKey,
    ) -> Option<Self> {
        let account = response.get("account")?;
        if account.get("type").and_then(serde_json::Value::as_str) != Some("chatgpt") {
            return None;
        }
        let plan_type = account
            .get("planType")
            .and_then(serde_json::Value::as_str)
            .filter(|plan| !plan.is_empty())
            .map(|plan| normalize::bound_text(plan, TextLimit::AccountLabel).text);
        let fingerprint = account
            .get("email")
            .and_then(serde_json::Value::as_str)
            .filter(|email| !email.is_empty())
            .map(|email| key.fingerprint(email));
        Some(Self {
            plan_type,
            fingerprint,
        })
    }
}

/// A discovery, together with the account facts only a Codex probe can report.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct CodexDiscovery {
    /// The same bounded discovery [`Harness::discover`](mango_external_agents::Harness::discover)
    /// returns.
    pub discovery: mango_external_agents::Discovery,
    /// The signed-in ChatGPT account, when there is one and the probe reached the app-server.
    pub account: Option<CodexAccount>,
}

#[cfg(test)]
mod tests {
    use super::{AccountFingerprintKey, CodexAccount};
    use serde_json::json;

    fn key() -> AccountFingerprintKey {
        AccountFingerprintKey::new(b"host-local-key").expect("expected a key")
    }

    /// The value the TypeScript adapter computed for the same key and address:
    /// `createHmac('sha256', key).update('codex:' + email).digest('hex').slice(0, 32)`.
    #[test]
    fn the_fingerprint_matches_the_digest_the_typescript_adapter_stored() {
        assert_eq!(
            key().fingerprint("user@example.com").as_str(),
            "bcd4e5c63495974573261faadb33d8be"
        );
    }

    #[test]
    fn a_different_key_gives_the_same_address_a_different_fingerprint() {
        let other = AccountFingerprintKey::new(b"another-host").expect("expected a key");
        assert_ne!(
            key().fingerprint("user@example.com"),
            other.fingerprint("user@example.com")
        );
    }

    #[test]
    fn an_empty_key_is_refused_because_it_would_be_a_plain_hash() {
        let refused = AccountFingerprintKey::new(b"");
        assert!(
            matches!(
                refused,
                Err(mango_external_agents::Error::HostConfiguration { .. })
            ),
            "expected an empty key to be refused, received {refused:?}"
        );
    }

    /// The address is digest input only: it survives into no value and no diagnostic.
    #[test]
    fn a_chatgpt_account_reports_its_plan_and_fingerprint_but_never_its_address() {
        let account = CodexAccount::from_account_read(
            &json!({"account": {"type": "chatgpt", "email": "user@example.com",
                                "planType": "plus"}, "requiresOpenaiAuth": true}),
            &key(),
        )
        .expect("expected a ChatGPT account");
        assert_eq!(account.plan_type.as_deref(), Some("plus"));
        assert_eq!(
            account
                .fingerprint
                .as_ref()
                .map(super::AccountFingerprint::as_str),
            Some("bcd4e5c63495974573261faadb33d8be")
        );
        let rendered = format!("{account:?} {:?}", account.fingerprint);
        assert!(
            !rendered.contains("example.com") && !rendered.contains("bcd4e5"),
            "expected no address or digest in diagnostics, received {rendered}"
        );
    }

    #[test]
    fn an_account_without_an_address_has_no_fingerprint() {
        let account = CodexAccount::from_account_read(
            &json!({"account": {"type": "chatgpt", "email": null, "planType": "pro"}}),
            &key(),
        )
        .expect("expected a ChatGPT account");
        assert_eq!(account.fingerprint, None);
        assert_eq!(account.plan_type.as_deref(), Some("pro"));
    }

    #[test]
    fn an_api_key_account_or_no_account_has_nothing_to_fingerprint() {
        for response in [
            json!({"account": {"type": "apiKey"}}),
            json!({"account": null, "requiresOpenaiAuth": true}),
            json!(null),
        ] {
            assert_eq!(
                CodexAccount::from_account_read(&response, &key()),
                None,
                "expected no account facts for {response}"
            );
        }
    }
}
