# Security Policy

Report vulnerabilities privately through
[GitHub Security Advisories](https://github.com/juliopolycarpo/mango-external-agents/security/advisories/new).
Do not open a public issue for a security concern.

Only the latest published minor of each crate receives fixes. In scope even when no consumer is
affected yet: any path by which a vendor credential could be read, copied or forwarded; a vendor
tool call reaching the host's tools; an unbounded line or event buffer; an environment variable
leaking past the allowlist; credential-shaped text surviving stderr redaction.
