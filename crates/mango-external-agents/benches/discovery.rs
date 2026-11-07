mod support;

use mango_external_agents::discovery::Model;
use mango_external_agents::permission::{
    ApprovalRouting, ConfigurationVerdict, PermissionLevel, PermissionMatrix, UnsupportedReason,
};
use mango_external_agents::{
    Capabilities, Capability, CapabilityCeiling, Discovery, EnvSource, Error, Harness,
    HarnessDescriptor, HarnessIdentity, HostContext, LaunchSpec, ManagedProcess, OpenSession,
    ProcessLauncher, Result, Session, VendorInfo,
};
use std::sync::Arc;
use support::{Bench, Unit};

struct NoIoLauncher;
#[async_trait::async_trait]
impl ProcessLauncher for NoIoLauncher {
    async fn spawn(&self, spec: LaunchSpec) -> Result<ManagedProcess> {
        Err(Error::Launch {
            program: spec.program().unwrap_or_default().to_owned(),
            message: String::from("discovery benchmark does not launch processes"),
        })
    }
}

/// In-memory probe traffic, never a vendor discovery measurement.
struct ProbeHarness {
    descriptor: HarnessDescriptor,
    declaration: PermissionMatrix,
    found: Discovery,
}
#[async_trait::async_trait]
impl Harness for ProbeHarness {
    fn descriptor(&self) -> &HarnessDescriptor {
        &self.descriptor
    }
    fn permission_matrix(&self) -> PermissionMatrix {
        self.declaration.clone()
    }
    async fn probe(&self, _host: &HostContext) -> Result<Discovery> {
        Ok(self.found.clone())
    }
    async fn open_session(
        &self,
        _host: &HostContext,
        _request: OpenSession,
    ) -> Result<Box<dyn Session>> {
        Err(Error::NotSupported {
            capability: Capability::Resume,
        })
    }
}

fn main() {
    let host = HostContext::builder()
        .launcher(Arc::new(NoIoLauncher))
        .cwd(std::env::temp_dir())
        .client_info("discovery-benchmark", "0.0.0")
        .environment(EnvSource::empty())
        .build()
        .expect("in-memory benchmark host");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let refused = runtime.block_on(NoIoLauncher.spawn(LaunchSpec {
        argv: vec![String::from("never-launched")],
        cwd: std::env::temp_dir(),
        env: Default::default(),
        stdin: false,
        hide_window: true,
    }));
    assert!(matches!(refused, Err(Error::Launch { .. })));
    let bench = Bench::new("discovery normalization");
    for (name, count, with_text, hostile, catalog) in [
        ("discover/vendor-metadata", 1000, true, false, false),
        ("discover/no-permission-text", 1000, false, false, false),
        ("discover/hostile-metadata", 100, true, true, false),
        ("discover/model-catalog", 100, true, false, true),
    ] {
        let declaration = PermissionMatrix::build(|_, _| ConfigurationVerdict::Supported {
            vendor_id: Some(String::from("declared-default")),
        });
        let matrix = PermissionMatrix::build(|level, routing| {
            let vendor_id = with_text.then(|| {
                if hostile {
                    String::from("invalid\u{202e}probe-id")
                } else {
                    format!("vendor-{level:?}-{routing:?}")
                }
            });
            if level == PermissionLevel::FullAccess || routing == ApprovalRouting::AutoReview {
                ConfigurationVerdict::Unsupported {
                    vendor_id,
                    reason: UnsupportedReason::Other(if hostile {
                        format!("{} Bearer private-benchmark-canary", "policy ".repeat(2500))
                    } else if with_text {
                        String::from("disabled by the configured policy")
                    } else {
                        String::new()
                    }),
                }
            } else {
                ConfigurationVerdict::Supported { vendor_id }
            }
        });
        let models = if catalog {
            (0..100).map(|n| Model::new(format!("model-{n}"))).collect()
        } else {
            Vec::new()
        };
        let found = Discovery {
            permission_matrix: matrix,
            models,
            ..Discovery::not_installed()
        };
        let descriptor = HarnessDescriptor {
            identity: HarnessIdentity::claude(),
            vendor: VendorInfo {
                company: "Benchmark",
                terms_url: "https://example.invalid/terms",
                privacy_url: "https://example.invalid/privacy",
                skills_are_slash_commands: false,
            },
            capabilities: CapabilityCeiling::new(Capabilities::none()),
            transports: &[],
            vendor_environment_keys: &[],
        };
        let expected = found
            .clone()
            .normalized()
            .bounded_by(&descriptor.capabilities, &declaration);
        let harness = ProbeHarness {
            descriptor,
            declaration,
            found,
        };
        let actual = runtime
            .block_on(harness.discover(&host))
            .expect("in-memory discovery");
        assert_eq!(actual.permission_matrix, expected.permission_matrix);
        assert_eq!(actual.models.len(), expected.models.len());
        for (actual, expected) in actual.models.iter().zip(&expected.models) {
            assert_eq!(actual.id, expected.id);
        }
        assert_eq!(actual.permission_matrix.cells().len(), 6);
        assert_eq!(actual.models.len(), if catalog { 100 } else { 0 });
        assert!(
            runtime
                .block_on(harness.open_session(&host, OpenSession::new("unused")))
                .is_err()
        );
        bench.run(
            name,
            Unit::new(count, "discovery"),
            || (),
            |()| {
                runtime.block_on(async {
                    for _ in 0..count {
                        drop(std::hint::black_box(
                            harness
                                .discover(std::hint::black_box(&host))
                                .await
                                .expect("in-memory discovery"),
                        ));
                    }
                });
            },
        );
    }
}
