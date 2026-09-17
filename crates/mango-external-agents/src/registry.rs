//! Which harnesses a host linked, by kind.
//!
//! Immutable once built, and it refuses a duplicate rather than letting one registration win:
//! two harnesses claiming the same kind is a wiring mistake, and the half that loses would be the
//! half nobody notices is missing.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::harness::{Harness, HarnessKind};

/// Every harness a host can dispatch to.
#[derive(Clone, Default)]
pub struct HarnessRegistry {
    harnesses: BTreeMap<HarnessKind, Arc<dyn Harness>>,
}

impl std::fmt::Debug for HarnessRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HarnessRegistry")
            .field("kinds", &self.kinds())
            .finish()
    }
}

impl HarnessRegistry {
    /// Registers each harness under the kind its descriptor names.
    ///
    /// # Errors
    ///
    /// [`Error::HostConfiguration`] when two harnesses claim the same kind.
    pub fn new(harnesses: Vec<Arc<dyn Harness>>) -> Result<Self> {
        let mut registered: BTreeMap<HarnessKind, Arc<dyn Harness>> = BTreeMap::new();
        for harness in harnesses {
            let kind = harness.descriptor().kind.clone();
            if registered.contains_key(&kind) {
                return Err(Error::HostConfiguration {
                    expected: "one harness per kind",
                    received: format!("{kind} registered twice"),
                });
            }
            registered.insert(kind, harness);
        }
        Ok(Self {
            harnesses: registered,
        })
    }

    /// Every registered kind, in a stable order.
    pub fn kinds(&self) -> Vec<&HarnessKind> {
        self.harnesses.keys().collect()
    }

    /// One harness, if it was registered.
    pub fn get(&self, kind: &HarnessKind) -> Option<&Arc<dyn Harness>> {
        self.harnesses.get(kind)
    }

    /// One harness, or a typed refusal naming what is registered.
    ///
    /// # Errors
    ///
    /// [`Error::HostConfiguration`] when nothing is registered under `kind`.
    pub fn require(&self, kind: &HarnessKind) -> Result<&Arc<dyn Harness>> {
        self.get(kind).ok_or_else(|| Error::HostConfiguration {
            expected: "a registered harness kind",
            received: format!("{kind}, with {:?} registered", self.kind_names()),
        })
    }

    /// How many are registered.
    pub fn len(&self) -> usize {
        self.harnesses.len()
    }

    /// Whether nothing is registered.
    pub fn is_empty(&self) -> bool {
        self.harnesses.is_empty()
    }

    fn kind_names(&self) -> Vec<String> {
        self.harnesses.keys().map(HarnessKind::to_string).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::HarnessRegistry;
    use crate::discovery::Discovery;
    use crate::error::{Error, Result};
    use crate::harness::{Capabilities, Harness, HarnessDescriptor, HarnessKind, VendorInfo};
    use crate::host::HostContext;
    use crate::permission::{ConfigurationVerdict, PermissionMatrix};
    use crate::session::{OpenSession, Session};
    use crate::transport::TransportKind;
    use std::sync::Arc;

    /// A harness that answers for one kind and does nothing else.
    struct NamedHarness {
        descriptor: HarnessDescriptor,
    }

    impl NamedHarness {
        fn arc(kind: HarnessKind) -> Arc<dyn Harness> {
            Arc::new(Self {
                descriptor: HarnessDescriptor {
                    kind,
                    vendor: VendorInfo {
                        company: "Example",
                        terms_url: "https://example.com/terms",
                        privacy_url: "https://example.com/privacy",
                        skills_are_slash_commands: false,
                    },
                    capabilities: Capabilities::none(),
                    transports: &[TransportKind::Stdio],
                    vendor_environment_keys: &[],
                },
            })
        }
    }

    #[async_trait::async_trait]
    impl Harness for NamedHarness {
        fn descriptor(&self) -> &HarnessDescriptor {
            &self.descriptor
        }

        fn permission_matrix(&self) -> PermissionMatrix {
            PermissionMatrix::build(|_, _| ConfigurationVerdict::supported())
        }

        async fn probe(&self, _host: &HostContext) -> Result<Discovery> {
            Ok(Discovery::not_installed())
        }

        async fn open_session(
            &self,
            _host: &HostContext,
            _request: OpenSession,
        ) -> Result<Box<dyn Session>> {
            Err(Error::Closed { subject: "session" })
        }
    }

    #[test]
    fn registers_each_kind_once_and_lists_them_in_a_stable_order() {
        let registry = HarnessRegistry::new(vec![
            NamedHarness::arc(HarnessKind::Codex),
            NamedHarness::arc(HarnessKind::Claude),
        ])
        .expect("expected a registry, received a refusal");

        assert_eq!(registry.len(), 2);
        assert_eq!(
            registry.kinds(),
            vec![&HarnessKind::Claude, &HarnessKind::Codex]
        );
    }

    #[test]
    fn refuses_two_harnesses_claiming_the_same_kind() {
        let error = HarnessRegistry::new(vec![
            NamedHarness::arc(HarnessKind::Claude),
            NamedHarness::arc(HarnessKind::Claude),
        ])
        .expect_err("expected a refusal, received a registry");

        assert!(
            matches!(
                error,
                Error::HostConfiguration {
                    expected: "one harness per kind",
                    ..
                }
            ),
            "expected a duplicate-kind refusal, received {error:?}"
        );
    }

    #[test]
    fn an_unregistered_kind_is_refused_with_what_is_registered() {
        let registry = HarnessRegistry::new(vec![NamedHarness::arc(HarnessKind::Claude)])
            .expect("expected a registry");

        assert!(registry.get(&HarnessKind::Codex).is_none());
        let Err(error) = registry.require(&HarnessKind::Codex) else {
            panic!("expected a refusal, received a harness");
        };
        assert!(
            matches!(
                &error,
                Error::HostConfiguration {
                    expected: "a registered harness kind",
                    received,
                } if received.contains("claude")
            ),
            "expected the raw configuration data to name what is registered, received {error:?}"
        );
        assert!(
            error.to_string().contains("invalid host configuration"),
            "expected a safe host diagnostic, received {error}"
        );
    }

    #[test]
    fn an_empty_registry_is_a_registry() {
        let registry = HarnessRegistry::new(Vec::new()).expect("expected a registry");
        assert!(registry.is_empty());
        assert!(registry.kinds().is_empty());
    }
}
