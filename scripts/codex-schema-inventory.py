#!/usr/bin/env python3
"""Reduces OpenAI's app-server JSON Schema bundle to the inventory mango-agent-codex speaks.

Called by scripts/vendor-codex.sh; not a standalone tool. The bundle is ~600 definitions and
580 KB, almost all of it describing surfaces this harness never touches. What is kept is one
entry per definition the harness deserialises or sends: its field names, and its enum values
where it is an enum. It also keeps the method discriminators the harness uses in each JSON-RPC
direction. That is the smallest artifact a rename upstream cannot pass through.

Usage: codex-schema-inventory.py <v2 bundle> <v1 bundle> <version>
"""

import json
import sys

# The definitions mango-agent-codex speaks, grouped the way src/protocol/ is.
WANTED = [
    # Handshake.
    "InitializeParams",
    "InitializeResponse",
    "ClientInfo",
    # Threads.
    "ThreadStartParams",
    "ThreadStartResponse",
    "ThreadResumeParams",
    "ThreadResumeResponse",
    "ThreadListParams",
    "ThreadListResponse",
    "Thread",
    "ThreadStartedNotification",
    # Turns.
    "TurnStartParams",
    "TurnStartResponse",
    "TurnSteerParams",
    "TurnSteerResponse",
    "TurnInterruptParams",
    "Turn",
    "TurnStatus",
    "TurnError",
    "TurnStartedNotification",
    "TurnCompletedNotification",
    "UserInput",
    # Items.
    "ThreadItem",
    "ItemStartedNotification",
    "ItemCompletedNotification",
    "AgentMessageDeltaNotification",
    "ReasoningTextDeltaNotification",
    "ReasoningSummaryTextDeltaNotification",
    "CommandExecutionStatus",
    "PatchApplyStatus",
    "McpToolCallStatus",
    # Approvals.
    "CommandExecutionRequestApprovalParams",
    "CommandExecutionRequestApprovalResponse",
    "CommandExecutionApprovalDecision",
    "FileChangeRequestApprovalParams",
    "FileChangeRequestApprovalResponse",
    "FileChangeApprovalDecision",
    "ServerRequestResolvedNotification",
    # Permissions as configuration.
    "AskForApproval",
    "SandboxMode",
    "SandboxPolicy",
    "ApprovalsReviewer",
    # Usage and quota.
    "ThreadTokenUsage",
    "TokenUsageBreakdown",
    "ThreadTokenUsageUpdatedNotification",
    "RateLimitSnapshot",
    "RateLimitWindow",
    "AccountRateLimitsUpdatedNotification",
    # Account and models.
    "Account",
    "GetAccountParams",
    "GetAccountResponse",
    "ModelListParams",
    "ModelListResponse",
    "Model",
    "ReasoningEffortOption",
    # Review.
    "ReviewStartParams",
    "ReviewStartResponse",
    "ReviewTarget",
    # Errors.
    "ErrorNotification",
]

# Methods mango-agent-codex reads or writes, grouped by JSON-RPC direction. Keep these narrow:
# the inventory guards this harness's contract, not every API the CLI exposes.
WANTED_METHODS = {
    "ClientRequest": [
        "initialize",
        "thread/start",
        "thread/resume",
        "thread/list",
        "turn/start",
        "turn/steer",
        "turn/interrupt",
        "review/start",
        "model/list",
        "account/read",
        "account/rateLimits/read",
    ],
    "ClientNotification": ["initialized"],
    "ServerRequest": [
        "item/commandExecution/requestApproval",
        "item/fileChange/requestApproval",
        "item/tool/call",
        "item/tool/requestUserInput",
        "mcpServer/elicitation/request",
        "item/permissions/requestApproval",
        "account/chatgptAuthTokens/refresh",
        "attestation/generate",
        "execCommandApproval",
        "applyPatchApproval",
    ],
    "ServerNotification": [
        "thread/started",
        "turn/started",
        "turn/completed",
        "item/started",
        "item/completed",
        "item/agentMessage/delta",
        "item/reasoning/textDelta",
        "item/reasoning/summaryTextDelta",
        "thread/tokenUsage/updated",
        "account/rateLimits/updated",
        "serverRequest/resolved",
        "error",
    ],
}


def definitions(path):
    with open(path, encoding="utf-8") as handle:
        bundle = json.load(handle)
    return bundle.get("definitions") or bundle.get("$defs") or {}


def fields(schema):
    """Field names, enum values and method discriminators a schema declares."""
    names = set()
    values = set()
    methods = set()

    def walk(node):
        if not isinstance(node, dict):
            return
        for name, subschema in node.get("properties", {}).items():
            names.add(name)
            # An internally-tagged variant spells its discriminator as a const (or a one-value
            # enum) under the tag property, so the variant names live one level down.
            if name == "type" and isinstance(subschema, dict):
                if isinstance(subschema.get("const"), str):
                    values.add(subschema["const"])
                for value in subschema.get("enum", []) or []:
                    if isinstance(value, str):
                        values.add(value)
            if name == "method" and isinstance(subschema, dict):
                if isinstance(subschema.get("const"), str):
                    methods.add(subschema["const"])
                for value in subschema.get("enum", []) or []:
                    if isinstance(value, str):
                        methods.add(value)
        for value in node.get("enum", []) or []:
            if isinstance(value, str):
                values.add(value)
        if isinstance(node.get("const"), str):
            values.add(node["const"])
        for key in ("oneOf", "anyOf", "allOf"):
            for branch in node.get(key, []) or []:
                walk(branch)

    walk(schema)
    return sorted(names), sorted(values), sorted(methods)


def main():
    if len(sys.argv) != 4:
        sys.exit(__doc__)
    v2, v1, version = sys.argv[1], sys.argv[2], sys.argv[3]
    v1_definitions = definitions(v1)
    v2_definitions = definitions(v2)
    defs = {**v1_definitions, **v2_definitions}

    inventory = {}
    missing = []
    for name in WANTED:
        schema = defs.get(name)
        if schema is None:
            missing.append(name)
            continue
        properties, values, _ = fields(schema)
        entry = {}
        if properties:
            entry["properties"] = properties
        if values:
            entry["values"] = values
        inventory[name] = entry

    if missing:
        sys.stderr.write(
            f"expected every wanted definition in the bundle, received none for: {missing}\n"
        )
        sys.exit(1)

    methods = {}
    for direction, wanted in WANTED_METHODS.items():
        declared = set()
        for schema_definitions in (v1_definitions, v2_definitions):
            schema = schema_definitions.get(direction)
            if schema is not None:
                _, _, names = fields(schema)
                declared.update(names)
        missing = sorted(set(wanted) - declared)
        if missing:
            sys.stderr.write(
                f"expected {direction} to declare every method this harness uses, "
                f"received none for: {missing}\\n"
            )
            sys.exit(1)
        methods[direction] = sorted(wanted)

    json.dump(
        {
            "codexVersion": version,
            "note": "Generated by scripts/vendor-codex.sh from `codex app-server "
            "generate-json-schema`. Do not edit by hand.",
            "definitions": inventory,
            "methods": methods,
        },
        sys.stdout,
        indent=2,
        sort_keys=True,
    )
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
