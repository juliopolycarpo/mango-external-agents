#!/usr/bin/env python3
"""Reduces OpenAI's app-server JSON Schema bundle to the inventory mango-agent-codex speaks.

Called by scripts/vendor-codex.sh; not a standalone tool. The broad inventory keeps field names,
enum values and method discriminators for the existing Rust contract tests. A separate explicit
projection records the types and alternatives the harness consumes, so same-name shape changes
are visible without treating unused schema members as consumed.

Usage: codex-schema-inventory.py <v2 bundle> <v1 bundle> <version>
"""

import json
import sys

# The definitions mango-agent-codex speaks, grouped the way src/protocol/ is.
WANTED = [
    # Handshake.
    "InitializeParams",
    "InitializeCapabilities",
    "InitializeResponse",
    "ClientInfo",
    "GatewayOAuthReadResponse",
    "GatewayOAuthStatus",
    # Threads.
    "ThreadStartParams",
    "ThreadStartResponse",
    "ThreadResumeParams",
    "ThreadResumeResponse",
    "ThreadReadParams",
    "ThreadReadResponse",
    "ThreadListParams",
    "ThreadListResponse",
    "ThreadSortKey",
    "SortDirection",
    "ThreadSourceKind",
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
    "CodexErrorInfo",
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
    "CommandExecutionOutputDeltaNotification",
    "McpToolCallProgressNotification",
    "FileChangePatchUpdatedNotification",
    "FileUpdateChange",
    # Approvals.
    "CommandExecutionRequestApprovalParams",
    "CommandExecutionRequestApprovalResponse",
    "CommandExecutionApprovalDecision",
    "FileChangeRequestApprovalParams",
    "FileChangeRequestApprovalResponse",
    "FileChangeApprovalDecision",
    "PermissionsRequestApprovalParams",
    "PermissionsRequestApprovalResponse",
    "PermissionGrantScope",
    "GrantedPermissionProfile",
    "ServerRequestResolvedNotification",
    # Questions and forms. The server blocks on each of these, so their shapes belong to the
    # contract even where the answer this harness sends is the native refusal.
    "ToolRequestUserInputParams",
    "ToolRequestUserInputQuestion",
    "ToolRequestUserInputOption",
    "ToolRequestUserInputResponse",
    "ToolRequestUserInputAnswer",
    "McpServerElicitationRequestParams",
    "McpServerElicitationRequestResponse",
    "McpServerElicitationAction",
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
    "GetAccountRateLimitsResponse",
    "CreditsSnapshot",
    "SpendControlLimitSnapshot",
    "RateLimitResetCreditsSummary",
    "RateLimitResetCredit",
    # Account and models.
    "Account",
    "GetAccountParams",
    "GetAccountResponse",
    "ModelListParams",
    "ModelListResponse",
    "Model",
    "ReasoningEffortOption",
    "PermissionProfileListParams",
    "PermissionProfileListResponse",
    "PermissionProfileSummary",
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
        "thread/read",
        "thread/list",
        "turn/start",
        "turn/steer",
        "turn/interrupt",
        "review/start",
        "model/list",
        "account/read",
        "account/gatewayOAuth/read",
        "account/rateLimits/read",
        "permissionProfile/list",
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
        "item/commandExecution/outputDelta",
        "item/mcpToolCall/progress",
        "item/fileChange/patchUpdated",
        "thread/tokenUsage/updated",
        "account/rateLimits/updated",
        "serverRequest/resolved",
        "error",
    ],
}


# Explicit vendor fields and alternatives the Rust protocol types read or write. These are not
# validation rules: unknown incoming members and alternatives keep the runtime's existing fallbacks.
# A list selects object fields; a tagged map selects fields per discriminator. "$enum" selects
# the values this harness handles, and "$objects" selects externally tagged response shapes.
CONSUMED = {
    # Handshake.
    "InitializeParams": ("v2", "clientInfo capabilities".split()),
    "InitializeCapabilities": ("v2", "explicitGatewayOauth optOutNotificationMethods".split()),
    "InitializeResponse": ("v1", "userAgent platformOs".split()),
    "ClientInfo": ("v2", "name title version".split()),
    "GatewayOAuthReadResponse": ("v2", "required status".split()),
    "GatewayOAuthStatus": ("v2", {"$enum": "notReady started succeeded failed".split()}),
    # thread/resume is decoded as ThreadStartResponse but has its own vendor schema.
    "ThreadStartParams": ("v2", "cwd model approvalPolicy sandbox approvalsReviewer config".split()),
    "ThreadResumeParams": ("v2", ("threadId cwd model approvalPolicy sandbox approvalsReviewer "
                                  "config excludeTurns").split()),
    "ThreadStartResponse": ("v2", "thread model reasoningEffort approvalPolicy approvalsReviewer".split()),
    "ThreadResumeResponse": ("v2", "thread model reasoningEffort approvalPolicy approvalsReviewer".split()),
    "ThreadReadParams": ("v2", "threadId includeTurns".split()),
    "ThreadReadResponse": ("v2", ["thread"]),
    "ThreadListParams": ("v2", "cursor limit cwd sortKey sortDirection sourceKinds archived".split()),
    "ThreadListResponse": ("v2", "data nextCursor".split()),
    "ThreadSortKey": ("v2", {"$enum": "created_at updated_at recency_at".split()}),
    "SortDirection": ("v2", {"$enum": "asc desc".split()}),
    "ThreadSourceKind": ("v2", {"$enum": "cli vscode exec appServer".split()}),
    "Thread": ("v2", "id preview name cwd updatedAt recencyAt".split()),
    "ThreadStartedNotification": ("v2", ["thread"]),
    # Turns and inputs.
    "TurnStartParams": ("v2", ("threadId input model effort approvalPolicy sandboxPolicy "
                               "approvalsReviewer").split()),
    "TurnStartResponse": ("v2", ["turn"]),
    "TurnSteerParams": ("v2", "threadId input expectedTurnId".split()),
    "TurnSteerResponse": ("v2", ["turnId"]),
    "TurnInterruptParams": ("v2", "threadId turnId".split()),
    "Turn": ("v2", "id status error".split()),
    "TurnStatus": ("v2", {"$enum": "inProgress completed interrupted failed".split()}),
    "TurnError": ("v2", "message additionalDetails codexErrorInfo".split()),
    "TurnStartedNotification": ("v2", "threadId turn".split()),
    "TurnCompletedNotification": ("v2", "threadId turn".split()),
    "UserInput": ("v2", {
        "text": "type text text_elements".split(),
        "image": "type url".split(),
    }),
    # The fallback accepts other item families without granting them typed ownership.
    "ThreadItem": ("v2", {
        "agentMessage": "type id text".split(),
        "reasoning": "type id summary content".split(),
        "commandExecution": "type id command cwd status aggregatedOutput exitCode".split(),
        "fileChange": "type id changes status".split(),
        "mcpToolCall": "type id server tool status".split(),
        "webSearch": "type id query".split(),
        "plan": "type id text".split(),
        "subAgentActivity": "type id agentThreadId kind".split(),
        "enteredReviewMode": "type id review".split(),
        "exitedReviewMode": "type id review".split(),
        "contextCompaction": "type id".split(),
        # The fallback reads only these common fields for other known families.
        "userMessage": "type id status".split(),
        "hookPrompt": "type id status".split(),
        "functionCallOutput": "type id status".split(),
        "collabAgentToolCall": "type id status".split(),
        "dynamicToolCall": "type id status".split(),
        "imageGeneration": "type id status".split(),
        "imageView": "type id status".split(),
        "sleep": "type id status".split(),
    }),
    "ItemStartedNotification": ("v2", "threadId turnId item".split()),
    "ItemCompletedNotification": ("v2", "threadId turnId item".split()),
    "AgentMessageDeltaNotification": ("v2", "threadId turnId itemId delta".split()),
    "ReasoningTextDeltaNotification": ("v2", "threadId turnId itemId delta".split()),
    "ReasoningSummaryTextDeltaNotification": ("v2", "threadId turnId itemId delta".split()),
    "CommandExecutionStatus": ("v2", {"$enum": "inProgress completed failed declined".split()}),
    "PatchApplyStatus": ("v2", {"$enum": "inProgress completed failed declined".split()}),
    "McpToolCallStatus": ("v2", {"$enum": "inProgress completed failed declined".split()}),
    "CollabAgentToolCallStatus": ("v2", {"$enum": "inProgress completed failed declined".split()}),
    "DynamicToolCallStatus": ("v2", {"$enum": "inProgress completed failed declined".split()}),
    "CommandExecutionOutputDeltaNotification": ("v2", "threadId turnId itemId delta".split()),
    "McpToolCallProgressNotification": ("v2", "threadId turnId itemId message".split()),
    "FileChangePatchUpdatedNotification": ("v2", "threadId turnId itemId changes".split()),
    "FileUpdateChange": ("v2", "path diff".split()),
    # Approval requests and the responses the broker can send. Opaque payloads stay opaque.
    "CommandExecutionRequestApprovalParams": ("v1", (
        "threadId turnId itemId approvalId command cwd reason networkApprovalContext "
        "proposedExecpolicyAmendment proposedNetworkPolicyAmendments").split()),
    "NetworkApprovalContext": ("v1", "host protocol".split()),
    "NetworkApprovalProtocol": ("v2", {"$enum": "http https socks5Tcp socks5Udp".split()}),
    "CommandExecutionRequestApprovalResponse": ("v1", ["decision"]),
    "CommandExecutionApprovalDecision": ("v1", {
        "$enum": "accept acceptForSession decline cancel".split(),
        "$objects": {
            "acceptWithExecpolicyAmendment": ["execpolicy_amendment"],
            "applyNetworkPolicyAmendment": ["network_policy_amendment"],
        },
    }),
    # The broker reads these values from otherwise opaque amendment JSON.
    "NetworkPolicyAmendment": ("v1", "host action".split()),
    "NetworkPolicyRuleAction": ("v1", {"$enum": "allow deny".split()}),
    "FileChangeRequestApprovalParams": ("v1", "threadId turnId itemId reason grantRoot".split()),
    "FileChangeRequestApprovalResponse": ("v1", ["decision"]),
    "FileChangeApprovalDecision": ("v1", {"$enum": "accept acceptForSession decline cancel".split()}),
    "PermissionsRequestApprovalParams": ("v1", "threadId turnId itemId cwd permissions reason".split()),
    "PermissionsRequestApprovalResponse": ("v1", "permissions scope".split()),
    "PermissionGrantScope": ("v1", {"$enum": "turn session".split()}),
    "ServerRequestResolvedNotification": ("v2", "threadId requestId".split()),
    "ToolRequestUserInputParams": ("v1", "threadId turnId itemId isBlocking questions".split()),
    "ToolRequestUserInputQuestion": ("v1", "id header question options isOther isSecret".split()),
    "ToolRequestUserInputOption": ("v1", "label description".split()),
    "ToolRequestUserInputResponse": ("v1", ["answers"]),
    "ToolRequestUserInputAnswer": ("v1", ["answers"]),
    "McpServerElicitationRequestParams": ("v1", {
        "$tag": "mode", "$fields": "threadId turnId".split(),
        "form": ["mode"], "openai/form": ["mode"], "openaiForm": ["mode"],
        "url": "mode elicitationId".split(),
    }),
    "McpServerElicitationRequestResponse": ("v1", ["action"]),
    "McpServerElicitationAction": ("v1", {"$enum": ["decline"]}),
    # Observed approval and reviewer fallbacks discard unknown values. The outgoing subset is strict.
    "AskForApproval": ("v2", {"$enum": "untrusted on-request never".split()}),
    "ApprovalsReviewer": ("v2", {"$enum": "user auto_review guardian_subagent".split()}),
    "SandboxMode": ("v2", {"$enum": "read-only workspace-write danger-full-access".split()}),
    "SandboxPolicy": ("v2", {
        "readOnly": "type networkAccess".split(),
        "workspaceWrite": "type writableRoots networkAccess excludeTmpdirEnvVar excludeSlashTmp".split(),
        "dangerFullAccess": ["type"],
    }),
    # Usage, quota, account, models and review.
    "ThreadTokenUsage": ("v2", "total last modelContextWindow".split()),
    "TokenUsageBreakdown": ("v2", ("inputTokens cachedInputTokens cacheWriteInputTokens "
                                   "outputTokens reasoningOutputTokens totalTokens").split()),
    "ThreadTokenUsageUpdatedNotification": ("v2", "threadId turnId tokenUsage".split()),
    "RateLimitSnapshot": ("v2", ("primary secondary planType credits individualLimit "
                                 "spendControlReached").split()),
    "RateLimitWindow": ("v2", "usedPercent windowDurationMins resetsAt".split()),
    "AccountRateLimitsUpdatedNotification": ("v2", ["rateLimits"]),
    "GetAccountRateLimitsResponse": ("v2", "rateLimits rateLimitResetCredits".split()),
    "CreditsSnapshot": ("v2", "hasCredits unlimited balance".split()),
    "SpendControlLimitSnapshot": ("v2", "limit used remainingPercent resetsAt".split()),
    "RateLimitResetCreditsSummary": ("v2", "availableCount credits".split()),
    "RateLimitResetCredit": ("v2", "id resetType status grantedAt expiresAt title description".split()),
    "Account": ("v2", {
        "apiKey": ["type"], "chatgpt": "type planType email".split(),
        "amazonBedrock": ["type"], "gcp": ["type"],
    }),
    "GetAccountParams": ("v2", []),
    "GetAccountResponse": ("v2", "account requiresOpenaiAuth".split()),
    "ModelListParams": ("v2", "cursor limit".split()),
    "ModelListResponse": ("v2", "data nextCursor".split()),
    "Model": ("v2", ("id displayName description hidden isDefault supportedReasoningEfforts "
                     "defaultReasoningEffort").split()),
    "ReasoningEffortOption": ("v2", "reasoningEffort description".split()),
    "PermissionProfileListParams": ("v2", "cursor cwd".split()),
    "PermissionProfileListResponse": ("v2", "data nextCursor".split()),
    "PermissionProfileSummary": ("v2", "id allowed".split()),
    "ReviewStartParams": ("v2", "threadId target".split()),
    "ReviewStartResponse": ("v2", "turn reviewThreadId".split()),
    "ReviewTarget": ("v2", {
        "uncommittedChanges": ["type"], "baseBranch": "type branch".split(),
        "commit": "type sha title".split(), "custom": "type instructions".split(),
    }),
    "ErrorNotification": ("v2", "threadId turnId error willRetry".split()),
    # These referenced values are consumed as strings/IDs or opaque JSON. Track their outer
    # type without adopting the vendor's unbounded values or the contents of opaque objects.
    "AbsolutePathBuf": ("v2", {"$shape": True}),
    "LegacyAppPathString": ("v2", {"$shape": True}),
    "PlanType": ("v2", {"$shape": True}),
    "RateLimitResetCreditStatus": ("v2", {"$shape": True}),
    "RateLimitResetType": ("v2", {"$shape": True}),
    "ReasoningEffort": ("v2", {"$shape": True}),
    "RequestId": ("v2", {"$shape": True}),
    "RequestPermissionProfile": ("v2", {"$shape": True}),
    "GrantedPermissionProfile": ("v1", {"$shape": True}),
    "SubAgentActivityKind": ("v2", {"$shape": True}),
    "ThreadListCwdFilter": ("v2", {"$shape": True}),
    "CodexErrorInfo": ("v2", {"$shape": True}),
}


def load_bundle(path):
    with open(path, encoding="utf-8") as handle:
        return json.load(handle)


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


def variants(schema):
    """Keep ThreadItem field ownership instead of guessing from its property union.

    Fail on a new schema layout: losing ownership silently would restore the ambiguity
    this inventory is meant to remove. No property schemas or descriptions are copied.
    """
    branches = schema.get("oneOf")
    if not isinstance(branches, list) or not branches:
        raise ValueError(f"expected a nonempty ThreadItem oneOf, received {branches!r}")
    result = {}
    for branch in branches:
        properties = branch.get("properties", {})
        tag = properties.get("type", {})
        names = tag.get("enum", [tag.get("const")])
        if len(names) != 1 or not isinstance(names[0], str):
            raise ValueError(f"expected one string type discriminator, received {tag!r}")
        name = names[0]
        if name in result:
            raise ValueError(f"expected unique variants, received duplicate {name!r}")
        result[name] = {
            "properties": sorted(properties),
            "required": sorted(branch.get("required", [])),
        }
    return result

def declared_names(shape):
    """Property names and enum or const values present in a projected shape."""
    found = set()
    if isinstance(shape, dict):
        found.update(shape.get("properties", {}))
        for key in ("enum", "const"):
            value = shape.get(key)
            found.update(value if isinstance(value, list) else [value])
        for child in shape.values():
            found |= declared_names(child)
    elif isinstance(shape, list):
        for child in shape:
            found |= declared_names(child)
    return {name for name in found if isinstance(name, str)}


def with_undeclared(selected_names, shape):
    """Record allowlisted names the bundle does not declare instead of dropping them.

    A pin that lacks a selected field or value stays valid, but the gap is part of the
    projection, so a typo in the allowlist or a later vendor addition changes the output.
    """
    missing = sorted(set(selected_names) - declared_names(shape))
    if missing:
        shape["undeclared"] = missing
    return shape


class SchemaProjection:
    """Resolve references in their original bundle before comparing mirrored v2 definitions."""

    def __init__(self, v1_bundle, v2_bundle):
        self.comparing = set()
        self.recording = set()
        self.referenced = {}
        self.roots = {"v1": v1_bundle, "v2": v2_bundle}
        self.definition_keys = {
            root: "definitions" if bundle.get("definitions") or "$defs" not in bundle else "$defs"
            for root, bundle in self.roots.items()
        }

    def resolve(self, root, reference):
        if reference == "#":
            return self.roots[root]
        if not isinstance(reference, str) or not reference.startswith("#/"):
            raise ValueError(f"expected a local JSON pointer, received {reference!r}")
        node = self.roots[root]
        try:
            for part in reference[2:].split("/"):
                part = part.replace("~1", "/").replace("~0", "~")
                node = node[int(part)] if isinstance(node, list) else node[part]
        except (KeyError, TypeError, ValueError, IndexError) as error:
            raise ValueError(f"expected {root} to contain {reference!r}, received no target") from error
        return node

    def reference(self, root, pointer):
        target = self.resolve(root, pointer)
        identity = root + pointer
        if identity in self.recording:
            return identity
        prefix = "#/" + self.definition_keys["v1"] + "/v2/"
        if root == "v1" and pointer.startswith(prefix):
            flat_pointer = "#/" + self.definition_keys["v2"] + "/" + pointer[len(prefix):]
            try:
                flat = self.resolve("v2", flat_pointer)
            except ValueError:
                flat = None
            if flat is not None:
                # Recursive references are compared coinductively; any differing field outside
                # the cycle still causes the enclosing comparison to fail.
                if pointer in self.comparing:
                    return "v2" + flat_pointer
                original_pointer = pointer
                self.comparing.add(original_pointer)
                try:
                    name = pointer[len(prefix):].replace("~1", "/").replace("~0", "~")
                    entry = CONSUMED.get(name)
                    if entry is not None and entry[0] == "v2":
                        left = self.project_node(target, root, entry[1])
                        right = self.project_node(flat, "v2", entry[1])
                    else:
                        left = self.shape(target, root, literals=False)
                        right = self.shape(flat, "v2", literals=False)
                    if left == right:
                        root, pointer, target = "v2", flat_pointer, flat
                        identity = root + pointer
                finally:
                    self.comparing.remove(original_pointer)
        self.record_reference(root, pointer, target)
        return identity

    def record_reference(self, root, pointer, target):
        """Retain shapes for referenced definitions without a matching selected owner."""
        identity = root + pointer
        if identity in self.recording:
            return
        prefix = "#/" + self.definition_keys[root] + "/"
        suffix = pointer[len(prefix):] if pointer.startswith(prefix) else ""
        name = suffix.replace("~1", "/").replace("~0", "~") if "/" not in suffix else ""
        entry = CONSUMED.get(name)
        if entry is not None and entry[0] == root:
            return
        if not name and root == "v1" and suffix.startswith("v2/"):
            name = suffix[3:].replace("~1", "/").replace("~0", "~")
            entry = CONSUMED.get(name)
        self.recording.add(identity)
        try:
            selected = entry[1] if entry is not None else {"$shape": True}
            self.referenced[identity] = self.project_node(target, root, selected)
        finally:
            self.recording.remove(identity)

    def shape(self, node, root, selected=None, literals=True):
        """Project only selected properties while retaining their schema structure."""
        if isinstance(node, bool):
            return node
        if not isinstance(node, dict):
            raise ValueError(f"expected a schema object or boolean, received {node!r}")
        result = {}
        keys = ("type", "format", "minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum",
                "minLength", "maxLength", "pattern", "minItems", "maxItems")
        for key in (*keys, "const", "enum") if literals else keys:
            if key in node:
                value = node[key]
                result[key] = sorted(value) if isinstance(value, list) else value
        if not literals:
            values = node.get("enum", [])
            if "const" in node:
                values = [*values, node["const"]]
            kinds = set()
            for value in values:
                if value is None:
                    kinds.add("null")
                elif isinstance(value, bool):
                    kinds.add("boolean")
                elif isinstance(value, str):
                    kinds.add("string")
                elif isinstance(value, int):
                    kinds.add("integer")
                elif isinstance(value, float):
                    kinds.add("number")
                elif isinstance(value, list):
                    kinds.add("array")
                else:
                    kinds.add("object")
            if kinds:
                result["literalTypes"] = sorted(kinds)
        if "$ref" in node:
            result["$ref"] = self.reference(root, node["$ref"])
        fields = {} if selected is None else (
            {name: None for name in selected} if isinstance(selected, list) else selected
        )
        properties = node.get("properties", {})
        chosen = {}
        for name, nested in sorted(fields.items()):
            if name in properties:
                chosen[name] = {
                    "required": name in node.get("required", []),
                    "schema": self.shape(properties[name], root, nested, literals),
                }
        if chosen:
            result["properties"] = chosen
        for keyword in ("oneOf", "anyOf", "allOf"):
            if keyword in node:
                branches = []
                for branch in node[keyword]:
                    # A union may describe extra object alternatives unrelated to a selected field
                    # (for example UserInput.image.fileId). Do not project those alternatives.
                    if fields and isinstance(branch, dict) and "properties" in branch:
                        if not (fields.keys() & branch["properties"].keys()):
                            continue
                    branches.append(self.shape(branch, root, selected, literals))
                if branches:
                    by_shape = {json.dumps(item, sort_keys=True): item for item in branches}
                    result[keyword] = [by_shape[key] for key in sorted(by_shape)]
        if "items" in node:
            result["items"] = self.shape(node["items"], root, literals=literals)
        if "additionalProperties" in node:
            result["additionalProperties"] = self.shape(node["additionalProperties"], root, literals=literals)
        return result

    def enum_shape(self, node, root, allowed):
        """Keep handled scalar alternatives, including their type and union ownership."""
        if not isinstance(node, dict):
            return {}
        result = {}
        if "enum" in node:
            values = [value for value in node["enum"] if value in allowed]
            if values:
                result = self.shape({**node, "enum": values}, root)
        elif node.get("const") in allowed:
            result = self.shape(node, root)
        else:
            for keyword in ("oneOf", "anyOf", "allOf"):
                branches = [self.enum_shape(branch, root, allowed) for branch in node.get(keyword, [])]
                branches = [branch for branch in branches if branch]
                if branches:
                    result[keyword] = sorted(branches, key=lambda item: json.dumps(item, sort_keys=True))
        return result

    def project_node(self, node, root, selected):
        if isinstance(selected, list):
            result = with_undeclared(selected, self.shape(node, root, selected))
        elif "$shape" in selected:
            result = self.shape(node, root, literals=False)
        elif "$enum" in selected:
            result = with_undeclared(
                selected["$enum"], self.enum_shape(node, root, set(selected["$enum"]))
            )
            if "$objects" in selected:
                choices = {}
                for object_name, fields in selected["$objects"].items():
                    matches = [branch for branch in node.get("oneOf", [])
                               if object_name in branch.get("properties", {})]
                    if len(matches) > 1:
                        raise ValueError(f"expected unique {root} choice, received duplicate {object_name!r}")
                    if matches:
                        choices[object_name] = with_undeclared(
                            fields, self.shape(matches[0], root, {object_name: fields})
                        )
                result["objects"] = choices
        else:
            tag = selected.get("$tag", "type")
            variants = {}
            wanted = {key: value for key, value in selected.items() if not key.startswith("$")}
            for branch in node.get("oneOf", []):
                discriminator = branch.get("properties", {}).get(tag, {})
                values = discriminator.get("enum", [discriminator.get("const")])
                if len(values) != 1 or values[0] not in wanted:
                    continue
                variant = values[0]
                if variant in variants:
                    raise ValueError(f"expected unique {root} variant, received duplicate {variant!r}")
                variants[variant] = with_undeclared(
                    wanted[variant], self.shape(branch, root, wanted[variant])
                )
            result = with_undeclared(
                selected.get("$fields", []),
                self.shape({key: value for key, value in node.items() if key != "oneOf"}, root,
                           selected.get("$fields", [])),
            )
            result["variants"] = variants
        return result

    def project(self, root, name, selected):
        escaped_name = name.replace("~", "~0").replace("/", "~1")
        pointer = "#/" + self.definition_keys[root] + "/" + escaped_name
        node = self.resolve(root, pointer)
        return {"source": root + pointer, "schema": self.project_node(node, root, selected)}


def main():
    if len(sys.argv) != 4:
        sys.exit(__doc__)
    v2, v1, version = sys.argv[1], sys.argv[2], sys.argv[3]
    v1_bundle = load_bundle(v1)
    v2_bundle = load_bundle(v2)
    v1_definitions = v1_bundle.get("definitions") or v1_bundle.get("$defs") or {}
    v2_definitions = v2_bundle.get("definitions") or v2_bundle.get("$defs") or {}
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
        if name == "ThreadItem":
            entry["variants"] = variants(schema)
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

    projection = SchemaProjection(v1_bundle, v2_bundle)
    consumed = {
        name: projection.project(root, name, selected)
        for name, (root, selected) in CONSUMED.items()
    }
    consumed["$referenced"] = projection.referenced

    json.dump(
        {
            "codexVersion": version,
            "note": "Generated by scripts/vendor-codex.sh from `codex app-server "
            "generate-json-schema`. Do not edit by hand.",
            "definitions": inventory,
            "consumed": consumed,
            "methods": methods,
        },
        sys.stdout,
        indent=2,
        sort_keys=True,
    )
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
