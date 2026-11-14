#!/usr/bin/env python3
"""Reports which scripts/test-*.{sh,py} a workflow can actually run, for check-script-tests.sh.

Naming a test in a workflow file does not run it. The workflow is parsed as YAML and a test counts
as run only when every one of these holds:

- it is a command of a `run:` step, not a word in an `echo`, a comment or a `with:` value;
- the job is listed in the `needs` of the `gate` job, so a failure fails the required check;
- the job has no `if:` and neither job nor step sets `continue-on-error`;
- the step has no `if:`, or an `if:` that is one OS matrix test, `matrix.os == '<os>'`, naming an
  `os` the job's matrix really lists;
- no earlier line of the same `run:` block ends the shell with `exit`, and the command's own line
  does not swallow its status with `|| true`, `|| :` or `|| exit 0`;
- the workflow triggers on `pull_request`, and has a `gate` job.

Output, one tab-separated record per line:

    ok       <test>                  the test runs
    blocked  <test>  <reason>        the test is named but a rule above stops it
    workflow <reason>                the workflow itself breaks a rule above

What it does not follow: shell conditionals inside a `run:` block, `set +e`, matrix `include`
entries and expressions other than the single OS test above.

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
    rf"({TEST_NAME})(?=\s|$)(?P<tail>.*)$"
)
ANY_NAME = re.compile(rf"(?:^|[^A-Za-z0-9_-])({TEST_NAME})")
SWALLOWED_STATUS = re.compile(r"\|\|\s*(?:true|:|exit\s+0)\s*(?:$|[;&|])")
OS_TEST = re.compile(r"^matrix\.os\s*==\s*'([^']+)'$")


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
    listed = (job.get("strategy") or {}).get("matrix", {}).get("os", [])
    if match.group(1) not in listed:
        return (
            f"a step of job '{job_id}' has `if: {step['if']}` but the matrix os list is "
            f"{listed}, expected it to contain '{match.group(1)}'"
        )
    return None


def script_lines(run):
    """The non-blank, non-comment lines of a `run:` block."""
    for line in run.splitlines():
        stripped = line.strip()
        if stripped and not stripped.startswith("#"):
            yield stripped


def run_findings(run):
    """Yields (test, reason or None) for each test named in a `run:` block."""
    ended_by = None
    for line in script_lines(run):
        command = COMMAND.match(line)
        if command:
            name = command.group(1)
            if ended_by:
                yield name, f"it follows `{ended_by}` on an earlier line of the same run block"
            elif SWALLOWED_STATUS.search(command.group("tail")):
                yield name, "its line ends with `|| true`, `|| :` or `|| exit 0`"
            else:
                yield name, None
            continue
        for name in ANY_NAME.findall(line):
            yield name, f"`{line}` names it but does not start with it as a command"
        if re.match(r"^exit(?:\s|$)", line) and not ended_by:
            ended_by = line


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
            reason = blocker or step_blocker(job_id, job, step)
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
