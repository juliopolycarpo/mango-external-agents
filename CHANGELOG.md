# Changelog

All notable changes to this project are documented in this file.
The format is based on [Keep a Changelog](https://keepachangelog.com), and this
project adheres to [Semantic Versioning](https://semver.org).

## [0.1.0] - 2026-09-14

### 🚀 Features

- **(mea)** Add diagnostics and reproducible public contract capture
- **(core)** Finalize discovery and approval policy before 0.1
- **(acp)** The Agent Client Protocol harness, with per-agent profiles (#5)
- **(codex)** Codex app-server harness (#4)
- **(claude)** The Claude Code harness (#3)
- **(core)** Traits, events, host ports, transports and testing fakes (#2)

### 🐛 Bug Fixes

- **(mea)** Allocate capture workspaces without clock collisions
- **(ci)** Isolate vendor execution from drift issue publication
- **(core)** Retain rejected MCP configuration counts
- **(core)** Match Windows environment keys without case sensitivity
- **(core)** Refuse unrepresentable approval deadlines
- **(core)** Allow fallback resume through capability validation
- **(core)** Bound permission matrix vendor values during discovery
- **(mea)** Clean up owned capture workspaces
- **(mea)** Keep captured ACP profiles in separate fixture directories
- **(claude)** Preserve eligible permission modes during discovery
- **(ci)** Verify pinned archives and clean up schema captures
- **(mea)** Let the event consumer own terminal approval decisions
- **(release)** Validate tag dispatch and verify every package
- **(core)** Launch installed PowerShell CLI entrypoints on Windows

### 🏗️ Build

- Release pipeline with trusted publishing

### 🎨 Styling

- **(mea)** Format capture dispatch

### 🧹 Miscellaneous

- **(ci)** Document deliberate cleanup trap expansion
- Bootstrap the workspace

### 📚 Documentation

- Document host adoption and first-release validation
- Point at crates, not at plan files (#1)
- Repo rules, compliance skeleton, adopt guide

### 👷 CI

- Run release and drift script regressions on pull requests
- Verify pinned vendor contracts and report weekly drift
- Fmt, clippy, tests, deny, msrv on three OSes

