# Agent Client Protocol harness (`mango-agent-acp`)

Stub; plan 005 fills this in. The page will state:

- the vendor executable, the minimum version gate and how the version is read;
- the exact documented surface driven (ACP v1 over the official `agent-client-protocol` crate, one profile per agent), with a link to the vendor document each flag and
  message follows;
- the session model and how turns, steering, cancel and close map to the vendor;
- the permission matrix cells (level × routing) the vendor supports and why the rest do not;
- what discovery can report about auth state without reading a credential;
- the transport kinds supported and the typed error for the rest;
- known caveats and deliberate behaviour changes from the mangostudio port.

Compliance posture: see [compliance.md](compliance.md).
