"""Validate schemas, example manifests, the public file list (length limit, no editor leftovers), the CI workflow hardening and the Rust toolchain pin."""
import json
import re
import subprocess
import tomllib
from pathlib import Path

import yaml
from jsonschema import Draft202012Validator

# A remote action must name a full commit (`owner/repo[/path]@<40 hex>`) or a digest (`docker://image@sha256:<64 hex>`);
# a tag or branch can be moved after review. The comment after it names the version the commit stands for.
PINNED = re.compile(
    r"^(?:[A-Za-z0-9_.-]+/[A-Za-z0-9_./-]+@[0-9a-f]{40}|docker://\S+@sha256:[0-9a-f]{64})$"
)
VERSION_NOTE = re.compile(r"#\s*v?\d+(?:\.\d+)*")
USES = re.compile(r"^\s*(?:-\s+)?uses:\s*(\S+)(.*)$")
# A service or container image is named by its digest (`name:tag@sha256:<64 hex>`), the tag only says what it is.
IMAGE = re.compile(r"^\s*image:\s*(\S+)(.*)$")
DIGEST_IMAGE = re.compile(r"^[^@\s]+@sha256:[0-9a-f]{64}$")
# A tool version is a release (`1.98.1`), not a moving channel or a range.
EXACT_VERSION = re.compile(r"\d+\.\d+\.\d+")
# Text that must not appear in the code of a workflow (comments are ignored).
FORBIDDEN = (
    (re.compile(r"\bpull_request_target\b"), "`pull_request_target` runs fork code with the repository's token"),
    (re.compile(r"\bsecrets\."), "the public checks use no secrets"),
    (re.compile(r"\bGITHUB_TOKEN\b"), "the public checks use no token"),
    (re.compile(r"-latest\b"), "a floating runner image can break the build overnight"),
)
MAX_JOB_MINUTES = 120
# Editor and merge leftovers (`x.rs.bak`, `x.orig`, `x.rej`, `.x.swp`, `x~`) are public once they are not ignored: a backup of a test can hold stale code or values.
LEFTOVER = re.compile(r"(?:\.(?:bak|orig|rej|swp)|~)$")


def check_files():
    listing = subprocess.check_output(
        ["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"]
    )
    for entry in listing.split(b"\0"):
        if not entry:
            continue
        path = Path(entry.decode())
        if LEFTOVER.search(path.name):
            raise ValueError(f"{path}: an editor or merge leftover would be published — delete it (or ignore it)")
        if path == Path("Cargo.lock"):
            continue  # Cargo regenerates this file as one workspace-wide dependency graph.
        with path.open("rb") as source:
            if sum(1 for _ in source) > 600:
                raise ValueError(f"{path}: exceeds 600 lines")


def check_examples():
    validators = {}
    for path in Path("schemas/v1").glob("*.schema.json"):
        schema = json.loads(path.read_text())
        Draft202012Validator.check_schema(schema)
        validators[path.name.split(".")[0]] = Draft202012Validator(schema)
    assert set(validators) == {"profile", "agent", "env", "triggers", "tools"}
    examples = Path("examples/profiles")
    for path in examples.rglob("AGENT.md"):
        lines = path.read_text(encoding="utf-8").splitlines()
        meta = {}
        if lines[:1] == ["---"]:
            if "---" not in lines[1:]:
                raise ValueError(f"{path}: unclosed front matter")
            end = lines.index("---", 1)
            meta = yaml.safe_load("\n".join(lines[1:end]))
        validators["agent"].validate(meta)
    for path in examples.rglob("*.yaml"):
        validators[path.stem].validate(yaml.safe_load(path.read_text()))


def workflow_problems(text):
    """The hardening rules one workflow file breaks; an empty list means it keeps all of them."""
    problems = []
    for number, line in enumerate(text.splitlines(), 1):
        code = line.split("#", 1)[0]
        for pattern, why in FORBIDDEN:
            if pattern.search(code):
                problems.append(f"line {number}: {why}")
        image = IMAGE.match(line)
        if image is not None:
            ref, rest = image.group(1).strip("'\""), image.group(2)
            if not DIGEST_IMAGE.match(ref):
                problems.append(f"line {number}: `image: {ref}` is not pinned to a digest (`name:tag@sha256:…`)")
            elif not VERSION_NOTE.search(rest):
                problems.append(f"line {number}: `image: {ref}` needs a `# x.y` comment naming its version")
        uses = USES.match(line)
        if uses is None:
            continue
        ref, rest = uses.group(1).strip("'\""), uses.group(2)
        if ref.startswith("./"):
            continue  # an action that lives in this repository is reviewed with it
        if not PINNED.match(ref):
            problems.append(f"line {number}: `uses: {ref}` is not pinned to a full commit SHA")
        elif not VERSION_NOTE.search(rest):
            problems.append(f"line {number}: `uses: {ref}` needs a `# vX.Y.Z` comment naming its version")
    try:
        workflow = yaml.safe_load(text)
    except yaml.YAMLError:
        return problems + ["not valid YAML"]
    if not isinstance(workflow, dict):
        return problems + ["not a workflow mapping"]
    if workflow.get("permissions") != {"contents": "read"}:
        problems.append("top-level `permissions` must be exactly `contents: read`")
    jobs = workflow.get("jobs")
    for job_id, job in (jobs if isinstance(jobs, dict) else {}).items():
        if not isinstance(job, dict):
            problems.append(f"job {job_id}: not a mapping")
            continue
        permissions = job.get("permissions")
        if "permissions" in job and not (
            isinstance(permissions, dict) and set(permissions.values()) <= {"read", "none"}
        ):
            problems.append(f"job {job_id}: `permissions` may only narrow to `read` or `none`")
        minutes = job.get("timeout-minutes")
        if not (isinstance(minutes, int) and not isinstance(minutes, bool) and 0 < minutes <= MAX_JOB_MINUTES):
            problems.append(f"job {job_id}: `timeout-minutes` must be set (at most {MAX_JOB_MINUTES})")
        for step in job.get("steps") or []:
            used = step.get("uses") if isinstance(step, dict) else None
            options = step.get("with") if isinstance(step, dict) else None
            options = options if isinstance(options, dict) else {}
            if isinstance(used, str) and used.startswith("actions/checkout@"):
                if options.get("persist-credentials") is not False:
                    problems.append(f"job {job_id}: `actions/checkout` must set `persist-credentials: false`")
            if isinstance(used, str) and used.startswith("actions/setup-node@"):
                version = options.get("node-version")
                if not (isinstance(version, str) and EXACT_VERSION.fullmatch(version)):
                    problems.append(f"job {job_id}: `setup-node` needs an exact `node-version` (x.y.z)")
    return problems


def check_workflows():
    problems = []
    folder = Path(".github/workflows")
    for path in sorted([*folder.glob("*.yml"), *folder.glob("*.yaml")]):
        for problem in workflow_problems(path.read_text(encoding="utf-8")):
            problems.append(f"{path}: {problem}")
    if problems:
        raise ValueError("CI workflow hardening:\n" + "\n".join(problems))


def toolchain_problems(text):
    """What `rust-toolchain.toml` lacks to pin one compiler for CI and the release gate (empty when it is fine)."""
    try:
        toolchain = tomllib.loads(text).get("toolchain")
    except tomllib.TOMLDecodeError:
        return ["not valid TOML"]
    if not isinstance(toolchain, dict):
        return ["missing the [toolchain] table"]
    problems = []
    channel = toolchain.get("channel")
    if not (isinstance(channel, str) and EXACT_VERSION.fullmatch(channel)):
        problems.append("`channel` must be an exact release such as 1.98.1, not a moving channel")
    components = toolchain.get("components")
    missing = {"rustfmt", "clippy"} - set(components if isinstance(components, list) else [])
    if missing:
        problems.append(f"`components` must include {', '.join(sorted(missing))}")
    return problems


def check_toolchain():
    path = Path("rust-toolchain.toml")
    if not path.is_file():
        raise ValueError(f"{path}: missing — CI and the release gate must build with one pinned compiler")
    problems = toolchain_problems(path.read_text(encoding="utf-8"))
    if problems:
        raise ValueError(f"{path}:\n" + "\n".join(problems))


def main():
    check_files()
    check_examples()
    check_workflows()
    check_toolchain()


if __name__ == "__main__":
    main()
