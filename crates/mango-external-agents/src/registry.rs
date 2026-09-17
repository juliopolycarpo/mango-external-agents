//! Which harnesses a host linked, by id.
//!
//! Immutable once built, and it refuses a duplicate rather than letting one registration win:
//! two harnesses claiming the same id is a wiring mistake, and the half that loses would be the
//! half nobody notices is missing.
//!
//! The key is a [`HarnessId`], which is a validated string rather than an arm of an enum this
//! crate owns. A host with a native harness of its own registers it here under an id it chose, and
//! nothing about that requires a release of this library — which is the whole point of
//! [`HarnessIdentity::custom`](crate::HarnessIdentity::custom).

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::harness::Harness;
use crate::identity::HarnessId;

/// Every harness a host can dispatch to.
#[derive(Clone, Default)]
pub struct HarnessRegistry {
    harnesses: BTreeMap<HarnessId, Arc<dyn Harness>>,
}

impl std::fmt::Debug for HarnessRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HarnessRegistry")
            .field("ids", &self.ids())
            .finish()
    }
}

impl HarnessRegistry {
    /// Registers each harness under the id its descriptor names.
    ///
    /// # Errors
    ///
    /// [`Error::HostConfiguration`] when two harnesses claim the same id.
    pub fn new(harnesses: Vec<Arc<dyn Harness>>) -> Result<Self> {
        let mut registered: BTreeMap<HarnessId, Arc<dyn Harness>> = BTreeMap::new();
        for harness in harnesses {
            let id = harness.descriptor().identity.id.clone();
            if registered.contains_key(&id) {
                return Err(Error::HostConfiguration {
                    expected: "one harness per id",
                    received: format!("{id} registered twice"),
                });
            }
            registered.insert(id, harness);
        }
        Ok(Self {
            harnesses: registered,
        })
    }

    /// Every registered id, in a stable order.
    pub fn ids(&self) -> Vec<&HarnessId> {
        self.harnesses.keys().collect()
    }

    /// One harness, if it was registered.
    pub fn get(&self, id: &HarnessId) -> Option<&Arc<dyn Harness>> {
        self.harnesses.get(id)
    }

    /// One harness, or a typed refusal naming what is registered.
    ///
    /// # Errors
    ///
    /// [`Error::HostConfiguration`] when nothing is registered under `id`.
    pub fn require(&self, id: &HarnessId) -> Result<&Arc<dyn Harness>> {
        self.get(id).ok_or_else(|| Error::HostConfiguration {
            expected: "a registered harness id",
            received: format!("{id}, with {:?} registered", self.id_names()),
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

    fn id_names(&self) -> Vec<String> {
        self.harnesses.keys().map(HarnessId::to_string).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::HarnessRegistry;
    use crate::discovery::Discovery;
    use crate::error::{Error, Result};
    use crate::harness::{CapabilityCeiling, Harness, HarnessDescriptor, VendorInfo};
    use crate::host::HostContext;
    use crate::identity::{HarnessId, HarnessIdentity};
    use crate::permission::{ConfigurationVerdict, PermissionMatrix};
    use crate::session::{OpenSession, Session};
    use crate::transport::TransportKind;
    use std::sync::Arc;

    /// A harness that answers for one id and does nothing else.
    struct NamedHarness {
        descriptor: HarnessDescriptor,
    }

    impl NamedHarness {
        fn arc(identity: HarnessIdentity) -> Arc<dyn Harness> {
            Arc::new(Self {
                descriptor: HarnessDescriptor {
                    identity,
                    vendor: VendorInfo {
                        company: "Example",
                        terms_url: "https://example.com/terms",
                        privacy_url: "https://example.com/privacy",
                        skills_are_slash_commands: false,
                    },
                    capabilities: CapabilityCeiling::none(),
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
    fn registers_each_id_once_and_lists_them_in_a_stable_order() {
        let registry = HarnessRegistry::new(vec![
            NamedHarness::arc(HarnessIdentity::codex()),
            NamedHarness::arc(HarnessIdentity::claude()),
        ])
        .expect("expected a registry, received a refusal");

        assert_eq!(registry.len(), 2);
        assert_eq!(
            registry.ids(),
            vec![&HarnessId::claude(), &HarnessId::codex()]
        );
    }

    /// The seam the whole identity split exists for: a host with a native harness of its own
    /// registers it under an id it chose, and nothing about that needs a release of this crate.
    #[test]
    fn a_harness_this_crate_has_never_heard_of_registers_alongside_the_built_in_ones() {
        let custom = HarnessIdentity::custom("acme-agent", "acme-rpc", None)
            .expect("expected a valid identity");
        let registry = HarnessRegistry::new(vec![
            NamedHarness::arc(HarnessIdentity::claude()),
            NamedHarness::arc(custom.clone()),
        ])
        .expect("expected a registry");

        assert_eq!(registry.len(), 2);
        assert!(registry.get(&custom.id).is_some());
        assert_eq!(
            registry
                .require(&custom.id)
                .expect("expected the custom harness")
                .descriptor()
                .identity
                .protocol
                .as_str(),
            "acme-rpc"
        );
    }

    /// An id nobody could have registered is refused where it is built, not where it is looked up.
    #[test]
    fn an_invalid_identifier_is_refused_deterministically_before_it_can_be_registered() {
        for bad in ["", "Acme Agent", "acme::agent", "-acme"] {
            assert!(
                HarnessId::new(bad).is_err(),
                "expected {bad:?} to be refused, received an id"
            );
            assert!(HarnessIdentity::custom(bad, "acme-rpc", None).is_err());
        }
    }

    #[test]
    fn refuses_two_harnesses_claiming_the_same_id() {
        let error = HarnessRegistry::new(vec![
            NamedHarness::arc(HarnessIdentity::claude()),
            NamedHarness::arc(HarnessIdentity::claude()),
        ])
        .expect_err("expected a refusal, received a registry");

        assert!(
            matches!(
                error,
                Error::HostConfiguration {
                    expected: "one harness per id",
                    ..
                }
            ),
            "expected a duplicate-id refusal, received {error:?}"
        );
        assert!(
            error.to_string().contains("claude registered twice"),
            "expected the colliding id in the diagnostic, received {error}"
        );
    }

    #[test]
    fn an_unregistered_id_is_refused_with_what_is_registered() {
        let registry = HarnessRegistry::new(vec![NamedHarness::arc(HarnessIdentity::claude())])
            .expect("expected a registry");

        assert!(registry.get(&HarnessId::codex()).is_none());
        let Err(error) = registry.require(&HarnessId::codex()) else {
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
            error.to_string().contains("claude"),
            "expected the diagnostic to name what is registered, received {error}"
        );
    }

    #[test]
    fn an_empty_registry_is_a_registry() {
        let registry = HarnessRegistry::new(Vec::new()).expect("expected a registry");
        assert!(registry.is_empty());
        assert!(registry.ids().is_empty());
    }
}
