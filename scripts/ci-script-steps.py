#!/usr/bin/env python3
"""Reports which scripts/test-*.{sh,py} a workflow can actually run, for check-script-tests.sh.

Naming a test in a workflow file does not run it. The workflow is parsed as YAML and a test counts
as run only when every one of these holds:

- it is a command of a `run:` step, not a word in an `echo`, a comment or a `with:` value;
- the job is listed in the `needs` of the `gate` job, so a failure fails the required check;
- the job has no `if:` and neither job nor step sets `continue-on-error`;
- the step has no `if:`, or an `if:` that is one OS matrix test, `matrix.os == '<os>'`, naming an
  `os` that some combination of the job's expanded matrix (lists, `exclude`, `include`) has;
- the `run:` block reaches it and its failure fails the step. The block is split into commands at
  newlines, `;`, `&&`, `||`, `|` and `&` (quotes, `$(...)`, comments and here-documents respected)
  and a test command counts only when it is joined to its neighbours by a newline or `;`, sits in no
  `if`, loop, `case`, function, group or subshell, follows no `exit` that can end the block and no
  `set +e`, and is not followed by `|| true`, `|| :`, `|| exit 0` or any other `||` handler;
- the step runs under no `shell:` or under `bash` or `sh`, the shells that stop at a failing
  command, whether set on the step or by `defaults.run`;
- the workflow triggers on `pull_request`, and has a `gate` job.

Output, one tab-separated record per line:

    ok       <test>                  the test runs
    blocked  <test>  <reason>        the test is named but a rule above stops it
    workflow <reason>                the workflow itself breaks a rule above

A construct the analysis does not follow is a reason to block, never a silent count. What it still
does not follow: expressions other than the single OS test above, `trap` and `exec` earlier in the
block, and anything a command does after it starts (a test script that exits 0 on its own).

Usage: ci-script-steps.py WORKFLOW
"""
import re
import sys

try:
    import yaml
except ImportError as error:
    sys.exit(
        "(ci-script-steps): expected PyYAML importable by python3 (apt: python3-yaml) | "
        f"received {error}"
    )

TEST_NAME = r"test-[A-Za-z0-9_-]+\.(?:sh|py)"
# A command is optional `VAR=value` assignments, an optional interpreter, then the script path.
COMMAND = re.compile(
    r"^(?:[A-Za-z_][A-Za-z0-9_]*=\S*\s+)*(?:(?:python3|bash|sh)\s+)?(?:\./)?scripts/"
    rf"({TEST_NAME})(?=\s|$)"
)
ANY_NAME = re.compile(rf"(?:^|[^A-Za-z0-9_-])({TEST_NAME})")
OS_TEST = re.compile(r"^matrix\.os\s*==\s*'([^']+)'$")

# The operators that join two commands without conditioning one on the other.
SEQUENTIAL = (None, "\n", ";", ";;", ";&")
HEREDOC = re.compile(r"<<-?\s*(['\"]?)([A-Za-z_][A-Za-z0-9_]*)\1")
# Words that open or continue a shell construct at the start of a command.
CONTINUATION = re.compile(r"^(?:then|else|do|elif)(?:\s+|$)(.*)$", re.S)
CONDITION_OPENER = re.compile(r"^(if|while|until)(?:\s+|$)(.*)$", re.S)
LIST_OPENER = re.compile(r"^(for|select|case)(?:\s+|$)(.*)$", re.S)
CLOSER = re.compile(r"^(?:fi|done|esac)(?:\s+|$)(.*)$", re.S)
FUNCTION_HEAD = re.compile(
    r"^(?:function\s+[A-Za-z_][\w-]*\s*(?:\(\s*\))?|[A-Za-z_][\w-]*\s*\(\s*\))\s*(?:\{(?:\s+|$))?(.*)$",
    re.S,
)
# A `case` arm's pattern list, `a|b)`, ahead of its command.
CASE_ARM = re.compile(r"^\(?[^()\s]+(?:\|[^()\s]+)*\)\s*(.*)$", re.S)
EXIT = re.compile(r"^exit(?:\s|$)")
FAILING_EXIT = re.compile(r"^exit(?:\s+[1-9][0-9]*)?\s*(?:$|[>&])")
SWALLOWING_HANDLER = re.compile(r"^(?:true|:|exit\s+0)(?:\s|$)")


def load_workflow(path):
    """Parses the workflow; exits with the received value when it is not a YAML mapping."""
    with open(path, encoding="utf-8") as handle:
        try:
            workflow = yaml.safe_load(handle)
        except yaml.YAMLError as error:
            sys.exit(f"(ci-script-steps): expected {path} to be valid YAML | received {error}")
    if not isinstance(workflow, dict) or not isinstance(workflow.get("jobs"), dict):
        sys.exit(
            f"(ci-script-steps): expected {path} to be a mapping with a `jobs` mapping | "
            f"received {type(workflow).__name__}"
        )
    return workflow


def triggers(workflow):
    """The event names in `on:`; YAML 1.1 reads the bare key `on` as the boolean True."""
    declared = workflow.get("on", workflow.get(True))
    if isinstance(declared, str):
        return {declared}
    if isinstance(declared, list):
        return set(declared)
    if isinstance(declared, dict):
        return set(declared)
    return set()


def gate_needs(workflow):
    """The job ids the `gate` job waits for, or None when there is no `gate` job."""
    gate = workflow["jobs"].get("gate")
    if not isinstance(gate, dict):
        return None
    needs = gate.get("needs", [])
    return {needs} if isinstance(needs, str) else set(needs)


def shell_blocker(workflow, job, step):
    """Why the step's shell may not stop on a failing command, or None.

    Without `shell:` GitHub runs `bash -e`; `bash` adds pipefail and `sh` runs `sh -e`. Any other
    shell (`pwsh`, `python`, `cmd`, a custom template) has no errexit the check knows about.
    """
    shell = step.get("shell")
    for scope in (job, workflow):
        default = ((scope.get("defaults") or {}).get("run") or {}).get("shell")
        shell = shell if shell is not None else default
    if shell is None or shell in ("bash", "sh"):
        return None
    return f"its step runs under `shell: {shell}`, expected bash or sh, which stop at a failing command"


def is_truthy(value):
    """`continue-on-error` is a boolean or an expression string; only a literal false is safe."""
    return value not in (None, False, "false")


def job_blocker(job_id, job, needs):
    """Why no step of this job can fail the gate, or None."""
    if job_id not in needs:
        return f"job '{job_id}' is not in the needs of the gate job"
    if "if" in job:
        return f"job '{job_id}' has `if: {job['if']}`"
    if is_truthy(job.get("continue-on-error")):
        return f"job '{job_id}' sets `continue-on-error: {job['continue-on-error']}`"
    return None


def step_blocker(job_id, job, step):
    """Why this step may not run or may not fail its job, or None."""
    if is_truthy(step.get("continue-on-error")):
        return f"a step of job '{job_id}' sets `continue-on-error: {step['continue-on-error']}`"
    if "if" not in step:
        return None
    condition = str(step["if"]).strip()
    if condition.startswith("${{") and condition.endswith("}}"):
        condition = condition[3:-2].strip()
    match = OS_TEST.match(condition)
    if not match:
        return (
            f"a step of job '{job_id}' has `if: {step['if']}`, "
            "expected none or one `matrix.os == '<os>'` test"
        )
    values, problem = matrix_os_values((job.get("strategy") or {}).get("matrix"))
    if problem:
        return f"a step of job '{job_id}' has `if: {step['if']}` but its matrix cannot be expanded: {problem}"
    if match.group(1) not in values:
        return (
            f"a step of job '{job_id}' has `if: {step['if']}` but the matrix os values are "
            f"{values}, expected it to contain '{match.group(1)}'"
        )
    return None


def matrix_combinations(matrix):
    """Expands a `strategy.matrix` the way GitHub does: (combinations, None) or (None, problem).

    The cross product of the list-valued keys, minus the combinations an `exclude` entry matches,
    then each `include` entry merged into every combination it does not contradict on an original
    key, or added as a combination of its own when it fits none.
    """
    if matrix is None:
        return [{}], None
    if not isinstance(matrix, dict):
        return None, f"strategy.matrix is {matrix!r}, expected a mapping"
    include, exclude = matrix.get("include", []), matrix.get("exclude", [])
    dimensions = {key: v for key, v in matrix.items() if key not in ("include", "exclude")}
    for name, values in dimensions.items():
        if not isinstance(values, list):
            return None, f"matrix.{name} is {values!r}, expected a list"
    for name, entries in (("include", include), ("exclude", exclude)):
        if not isinstance(entries, list) or not all(isinstance(entry, dict) for entry in entries):
            return None, f"matrix.{name} is {entries!r}, expected a list of mappings"
    combinations = [{}] if dimensions else []
    for name, values in dimensions.items():
        combinations = [{**combo, name: value} for combo in combinations for value in values]
    combinations = [
        combo
        for combo in combinations
        if not any(all(combo.get(k) == v for k, v in entry.items()) for entry in exclude)
    ]
    originals = list(combinations)
    for entry in include:
        fits = [
            combo
            for combo in originals
            if all(combo[key] == value for key, value in entry.items() if key in dimensions)
        ]
        for combo in fits:
            combo.update({key: v for key, v in entry.items() if key not in dimensions})
        if not fits:
            combinations.append(dict(entry))
    return combinations, None


def matrix_os_values(matrix):
    """The `os` values of the expanded matrix as (values, None), or (None, problem)."""
    combinations, problem = matrix_combinations(matrix)
    if problem:
        return None, problem
    values = []
    for combo in combinations:
        value = combo.get("os")
        if isinstance(value, str) and "${{" in value:
            return None, f"a matrix entry sets os to {value!r}, expected a literal"
        if "os" in combo and value not in values:
            values.append(value)
    return values, None


def split_commands(run):
    """Splits a `run:` block into [text, operator_after, is_here_document] entries.

    The operators are newline, `;`, `;;`, `;&`, `&&`, `||`, `|`, `|&` and `&`. Quotes, `$(...)`,
    backticks, comments, `2>&1`-style redirections and here-document bodies never split: a body is
    its own entry with is_here_document set, since the check cannot tell whether it is executed.
    """
    text = run.replace("\\\n", " ")
    entries, current, delimiters = [], [], []
    quote, depth, backtick, i = None, 0, False, 0

    def flush(operator):
        command = "".join(current).strip()
        current.clear()
        if command:
            entries.append([command, operator, False])

    while i < len(text):
        char, pair = text[i], text[i : i + 2]
        if quote == "'":
            quote = None if char == "'" else quote
        elif quote == '"':
            if char == "\\" and pair:
                current.append(pair)
                i += 2
                continue
            quote = None if char == '"' else quote
        elif char == "\\":
            current.append(pair)
            i += 2
            continue
        elif char in "'\"":
            quote = char
        elif char == "`":
            backtick = not backtick
        elif pair == "$(":
            depth += 1
            current.append(pair)
            i += 2
            continue
        elif depth and char in "()":
            depth += 1 if char == "(" else -1
        elif depth or backtick:
            pass
        elif char == "#" and (i == 0 or text[i - 1] in " \t\n;&|("):
            while i < len(text) and text[i] != "\n":
                i += 1
            continue
        elif text.startswith("<<<", i):
            current.append("<<<")
            i += 3
            continue
        elif char == "<" and pair == "<<" and HEREDOC.match(text, i):
            heredoc = HEREDOC.match(text, i)
            delimiters.append(heredoc.group(2))
            current.append(heredoc.group(0))
            i = heredoc.end()
            continue
        elif char == "\n":
            flush("\n")
            i += 1
            for delimiter in delimiters:
                body = []
                while i < len(text):
                    end = text.find("\n", i)
                    end = len(text) if end < 0 else end
                    line, i = text[i:end], min(end + 1, len(text))
                    if line.strip() == delimiter:
                        break
                    body.append(line)
                if body:
                    entries.append(["\n".join(body), "\n", True])
            delimiters = []
            continue
        else:
            operator = next((o for o in (";;", ";&", "&&", "||", "|&") if text.startswith(o, i)), None)
            if operator is None and char in ";|":
                operator = char
            if operator is None and char == "&" and text[i - 1 : i] not in "<>" and text[i + 1 : i + 2] != ">":
                operator = char
            if operator:
                flush(operator)
                i += len(operator)
                continue
        current.append(char)
        i += 1
    flush("\n")
    return entries


def enter_constructs(command, stack):
    """Strips leading shell keywords off a command, keeping `stack` of the open constructs.

    Returns (command, can_run, closes): `can_run` is False for the head of a `for`, `select` or
    `case`, whose remainder is a word list, not a command; `closes` is True when the command ends
    the subshell it sits in, which the caller pops once the command is judged.
    """
    can_run = True
    while True:
        if match := CONTINUATION.match(command):
            command = match.group(1)
        elif match := CONDITION_OPENER.match(command):
            stack.append(match.group(1))
            command = match.group(2)
        elif match := LIST_OPENER.match(command):
            stack.append(match.group(1))
            command, can_run = match.group(2), False
        elif match := CLOSER.match(command):
            if stack:
                stack.pop()
            command = match.group(1)
        elif stack and stack[-1] == "case" and (match := CASE_ARM.match(command)):
            command = match.group(1)
        elif match := FUNCTION_HEAD.match(command):
            stack.append("function")
            command = match.group(1)
        elif command.startswith("{") and command[1:2] in ("", " ", "\t"):
            stack.append("{")
            command = command[1:].strip()
        elif command.startswith("}"):
            if stack:
                stack.pop()
            command = command[1:].strip()
        elif command.startswith("("):
            stack.append("(")
            command = command[1:].strip()
        elif command.startswith(")"):
            if stack:
                stack.pop()
            command = command[1:].strip()
        else:
            closes = bool(stack) and stack[-1] == "(" and command.count(")") > command.count("(")
            if closes:
                command = command[: command.rindex(")")]
            return command.strip(), can_run, closes


def errexit_change(command):
    """'off' for `set +e`, `set +eu`, `set +o errexit`; 'on' for the `-` forms; else None."""
    words = command.split()
    if not words or words[0] != "set":
        return None
    change = None
    for index, word in enumerate(words[1:], start=1):
        if re.fullmatch(r"[-+][A-Za-z]+", word) and "e" in word[1:]:
            change = "on" if word[0] == "-" else "off"
        if word in ("-o", "+o") and words[index + 1 : index + 2] == ["errexit"]:
            change = "on" if word == "-o" else "off"
    return change


def command_blocker(entries, index, stack, ended_by, errexit_off):
    """Why the test command in entries[index] may not run or may not fail the step, or None."""
    if ended_by:
        return ended_by
    if stack:
        return f"it is inside a shell `{stack[-1]}` block, which the check does not follow"
    if errexit_off:
        return f"it follows `{errexit_off}`, which turns off errexit"
    before = entries[index - 1][1] if index else None
    if before not in SEQUENTIAL:
        return f"it is joined to the command before it by `{before}`, which the check does not follow"
    after = entries[index][1]
    if after in SEQUENTIAL:
        return None
    if after == "||" and index + 1 < len(entries):
        handler = entries[index + 1][0]
        if SWALLOWING_HANDLER.match(handler):
            return "its line ends with `|| true`, `|| :` or `|| exit 0`"
        if FAILING_EXIT.match(handler):
            return None
        return f"its failure is handled by `|| {handler}`, which the check does not follow"
    return f"it is joined to the command after it by `{after}`, which the check does not follow"


def run_findings(run):
    """Yields (test, reason or None) for each test named in a `run:` block."""
    entries = split_commands(run)
    stack, ended_by, errexit_off, closes = [], None, None, False
    for index, (text, after, is_here_document) in enumerate(entries):
        if closes:
            stack.pop()
            closes = False
        if is_here_document:
            for name in ANY_NAME.findall(text):
                yield name, "it appears in a here-document"
            continue
        command, can_run, closes = enter_constructs(text, stack)
        before = entries[index - 1][1] if index else None
        unconditional = not stack and before in SEQUENTIAL and after in SEQUENTIAL
        match = COMMAND.match(command) if can_run else None
        if match:
            yield match.group(1), command_blocker(entries, index, stack, ended_by, errexit_off)
            continue
        for name in ANY_NAME.findall(command):
            yield name, f"`{command}` names it but does not start with it as a command"
        change = errexit_change(command)
        if change == "off":
            errexit_off = errexit_off or command
        elif change == "on" and unconditional:
            errexit_off = None
        if EXIT.match(command) and not ended_by:
            if unconditional:
                ended_by = f"it follows `{command}` on an earlier line of the same run block"
            elif not FAILING_EXIT.match(command) and not (before == "||" and command == "exit"):
                ended_by = f"it follows `{command}`, which a condition can reach to end the run block early"


def report(workflow):
    """Yields the tab-separated records documented in the module docstring."""
    if "pull_request" not in triggers(workflow):
        yield ("workflow", "the workflow does not trigger on pull_request")
    needs = gate_needs(workflow)
    if needs is None:
        yield ("workflow", "the workflow has no `gate` job")
        needs = set()
    for job_id, job in workflow["jobs"].items():
        steps = job.get("steps") if isinstance(job, dict) else None
        if not isinstance(steps, list):
            continue
        blocker = job_blocker(job_id, job, needs)
        for step in steps:
            run = step.get("run") if isinstance(step, dict) else None
            if not isinstance(run, str):
                continue
            reason = blocker or step_blocker(job_id, job, step) or shell_blocker(workflow, job, step)
            for name, line_reason in run_findings(run):
                if reason or line_reason:
                    yield ("blocked", name, reason or line_reason)
                else:
                    yield ("ok", name)


def main(argv):
    if len(argv) != 2:
        sys.exit("(ci-script-steps): expected usage: ci-script-steps.py WORKFLOW | received "
                 f"{len(argv) - 1} arguments")
    for record in report(load_workflow(argv[1])):
        print("\t".join(record))


if __name__ == "__main__":
    main(sys.argv)
