"""Validate schemas, example manifests, workspace layering and pins, the public file list, CI workflow hardening, the Rust toolchain pin, SDK pins, and docs parity."""
import hashlib
import importlib.util
import json
import re
import shlex
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
EXACT_PIN = re.compile(r"=\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?")
MIDDLE_CRATES = {"bot-profile", "bot-store", "bot-runtime", "bot-broker", "bot-gateway", "bot-admin"}
# Text that must not appear in the code of a workflow (YAML comments are ignored).
FORBIDDEN = (
    (re.compile(r"\bpull_request_target\b"), "`pull_request_target` runs fork code with the repository's token"),
    (re.compile(r"\bsecrets\."), "the public checks use no secrets"),
    (re.compile(r"\bGITHUB_TOKEN\b"), "the public checks use no token"),
    (re.compile(r"-latest\b"), "a floating runner image can break the build overnight"),
)
MAX_JOB_MINUTES = 120
# Editor and merge leftovers (`x.rs.bak`, `x.orig`, `x.rej`, `.x.swp`, `x~`) are public once they are not ignored: a backup of a test can hold stale code or values.
LEFTOVER = re.compile(r"(?:\.(?:bak|orig|rej|swp)|~)$")


def is_test_src(path):
    """Test-only Rust sources under `crates/*/src/` exempt from the `//!` module header rule."""
    parts = path.parts
    return (
        path.name == "tests.rs"
        or path.name.endswith("_tests.rs")
        or bool({"tests", "testing"} & set(parts[3:-1]))
    )


def needs_module_header(path):
    parts = path.parts
    return (
        len(parts) >= 4
        and parts[0] == "crates"
        and parts[2] == "src"
        and path.suffix == ".rs"
        and not is_test_src(path)
    )


def check_files():
    listing = subprocess.check_output(
        ["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"]
    )
    problems = []
    for entry in listing.split(b"\0"):
        if not entry:
            continue
        path = Path(entry.decode())
        if not path.exists():
            continue
        if LEFTOVER.search(path.name):
            problems.append(f"{path}: an editor or merge leftover would be published — delete it (or ignore it)")
            continue
        if path == Path("Cargo.lock") or not path.is_file():
            continue  # Cargo regenerates Cargo.lock as one workspace-wide dependency graph.
        with path.open("rb") as source:
            lines = source.readlines()
        if len(lines) > 600:
            problems.append(f"{path}: exceeds 600 lines")
        if needs_module_header(path) and (not lines or not lines[0].startswith(b"//!") or not lines[0][3:].strip()):
            problems.append(f"{path}: missing non-empty `//!` module responsibility header on line 1")
    if problems:
        raise ValueError("\n".join(problems))


def workspace_problems(root=Path(".")):
    """Enforce exact `=` pins on external dependencies and the crate layering rule (`bot-core` <- 6 middle crates <- `bot-cli`)."""
    problems = []
    try:
        ws = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError) as err:
        return [f"Cargo.toml: {err}"]
    members = (ws.get("workspace") or {}).get("members") or []
    member_names = {Path(m).name for m in members}
    for name, spec in ((ws.get("workspace") or {}).get("dependencies") or {}).items():
        if isinstance(spec, dict) and "path" in spec:
            continue
        version = spec if isinstance(spec, str) else spec.get("version") if isinstance(spec, dict) else None
        if not (isinstance(version, str) and EXACT_PIN.fullmatch(version)):
            problems.append(f"Cargo.toml [workspace.dependencies].{name}: `{version}` must use an exact `=x.y.z` pin")
    for member in members:
        crate = Path(member).name
        try:
            data = tomllib.loads((root / member / "Cargo.toml").read_text(encoding="utf-8"))
        except (OSError, tomllib.TOMLDecodeError) as err:
            problems.append(f"{member}/Cargo.toml: {err}")
            continue
        tables = [("", data)] + [(f"target.{k}.", v) for k, v in (data.get("target") or {}).items() if isinstance(v, dict)]
        for prefix, table in tables:
            for section in ("dependencies", "dev-dependencies", "build-dependencies"):
                for dep, spec in (table.get(section) or {}).items():
                    pkg = spec.get("package", dep) if isinstance(spec, dict) else dep
                    if section == "dependencies" and pkg in member_names:
                        if crate == "bot-core":
                            problems.append(f"{member}/Cargo.toml: `bot-core` must not depend on internal crate `{pkg}`")
                        elif crate in MIDDLE_CRATES and pkg != "bot-core":
                            problems.append(f"{member}/Cargo.toml: middle crate `{crate}` must not depend on `{pkg}` at runtime")
                    if isinstance(spec, dict) and (spec.get("workspace") or "path" in spec):
                        continue
                    version = spec if isinstance(spec, str) else spec.get("version") if isinstance(spec, dict) else None
                    if not (isinstance(version, str) and EXACT_PIN.fullmatch(version)):
                        problems.append(f"{member}/Cargo.toml [{prefix}{section}].{dep}: `{version}` must inherit `.workspace = true` or use an exact `=x.y.z` pin")
    return problems


def check_workspace():
    problems = workspace_problems(Path("."))
    if problems:
        raise ValueError("workspace manifest contracts:\n" + "\n".join(problems))


def check_examples():
    """Validate example YAML manifests and AGENT.md/SKILL.md front matter against schemas/v1/*.schema.json
    (plus SKILL.md folder-name match and unclosed `---` fence). Full YAML 1.2 strict parsing, BOM/control-char
    checks, duplicate-key/anchor bans, 64 KiB skill limits, and cross-file references are enforced by the Rust
    `bot-profile` tests (`acceptance`, `team_example`, `front_matter`, `triggers_schema`) during `cargo test`."""
    validators = {}
    for path in Path("schemas/v1").glob("*.schema.json"):
        schema = json.loads(path.read_text())
        Draft202012Validator.check_schema(schema)
        validators[path.name.split(".")[0]] = Draft202012Validator(schema)
    assert set(validators) == {"profile", "agent", "skill", "env", "triggers", "tools"}
    examples = Path("examples/profiles")
    for filename, key in (("AGENT.md", "agent"), ("SKILL.md", "skill")):
        for path in examples.rglob(filename):
            lines = path.read_text(encoding="utf-8").splitlines()
            meta = {}
            if lines[:1] == ["---"]:
                if "---" not in lines[1:]:
                    raise ValueError(f"{path}: unclosed front matter")
                end = lines.index("---", 1)
                meta = yaml.safe_load("\n".join(lines[1:end]))
            validators[key].validate(meta)
            if key == "skill" and isinstance(meta, dict) and "name" in meta and meta["name"] != path.parent.name:
                raise ValueError(f"{path}: front matter name {meta['name']!r} != folder {path.parent.name!r}")
    for path in examples.rglob("*.yaml"):
        if path.name.endswith(".lock.yaml"):
            continue
        validators[path.stem].validate(yaml.safe_load(path.read_text()))


def strip_yaml_comment(line):
    """Strip a YAML comment (`#` at line start or after whitespace outside quotes) without truncating quoted `#` or `${#var}`."""
    quote = None
    for i, ch in enumerate(line):
        if quote is None:
            if ch in ("'", '"'):
                quote = ch
            elif ch == "#" and (i == 0 or line[i - 1].isspace()):
                return line[:i]
        elif ch == quote and (quote == "'" or line[i - 1] != "\\"):
            quote = None
    return line


def workflow_problems(text):
    """The hardening rules one workflow file breaks; an empty list means it keeps all of them."""
    problems = []
    for number, line in enumerate(text.splitlines(), 1):
        code = strip_yaml_comment(line)
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
            isinstance(permissions, dict)
            and set(permissions.keys()) <= {"contents"}
            and set(permissions.values()) <= {"read", "none"}
        ):
            problems.append(f"job {job_id}: `permissions` may only narrow `contents` to `read` or `none`")
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
            run = step.get("run") if isinstance(step, dict) else None
            if isinstance(run, str):
                problems += [f"job {job_id}: {problem}" for problem in command_problems(run)]
    return problems


def check_workflows():
    problems = []
    folder = Path(".github/workflows")
    for path in sorted([*folder.glob("*.yml"), *folder.glob("*.yaml")]):
        for problem in workflow_problems(path.read_text(encoding="utf-8")):
            problems.append(f"{path}: {problem}")
    if problems:
        raise ValueError("CI workflow hardening:\n" + "\n".join(problems))


SDK_JOB = "openai-sdk"
SDK_RUNNERS = ("sdk_run.py", "run_stub.sh", "run_node_stub.sh", "check_runners.sh")
RAW_RUNNER = re.compile(r"-m\s+unittest\b|\bnode\s+--test\b|\bpytest\b")
SWALLOW = re.compile(r"\|\||(?<!\|)\|(?!\|)|;\s*(?:true\b|:)")


def active_run_lines(step):
    return [line for line in str(step.get("run", "")).splitlines() if not line.lstrip().startswith("#")]


def sdk_job_problems(text):
    """The official-SDK job must run the source check (`sdk_run.py manifest`) and the three runners — the ones that refuse skipped, missing and untouched tests — through steps that cannot be skipped or have their failure ignored:
    no `if`/`continue-on-error`/`shell` on the job or on those steps, no pipe or `|| true` after a runner, and no way around the runners (raw `unittest`, `node --test`, `pytest`)."""
    try:
        workflow = yaml.safe_load(text)
    except yaml.YAMLError:
        return ["not valid YAML"]
    job = (workflow.get("jobs") or {}).get(SDK_JOB) if isinstance(workflow, dict) else None
    if not isinstance(job, dict):
        return [f"job `{SDK_JOB}` is missing"]
    problems = []
    for key in ("if", "continue-on-error", "defaults"):
        if key in job:
            problems.append(f"job {SDK_JOB}: `{key}` could skip the job or ignore its failure")
    steps = [step for step in job.get("steps") or [] if isinstance(step, dict)]
    for runner in SDK_RUNNERS:
        owners = [step for step in steps if any(runner in line for line in active_run_lines(step))]
        if not owners:
            problems.append(f"job {SDK_JOB}: no step runs `{runner}`")
        for step in owners:
            for key in ("if", "continue-on-error", "shell"):
                if key in step:
                    problems.append(f"job {SDK_JOB}: the step running `{runner}` has `{key}`")
            for line in active_run_lines(step):
                if runner in line and SWALLOW.search(line):
                    problems.append(f"job {SDK_JOB}: `{line.strip()[:70]}` swallows the runner's exit code")
    if any(RAW_RUNNER.search(line) for step in steps for line in active_run_lines(step)):
        problems.append(f"job {SDK_JOB}: run the SDK tests only through the runners, not `unittest`, `node --test` or `pytest` directly")
    return problems


def check_sdk_job():
    folder = Path(".github/workflows")
    holders = []
    for path in sorted([*folder.glob("*.yml"), *folder.glob("*.yaml")]):
        text = path.read_text(encoding="utf-8")
        try:
            workflow = yaml.safe_load(text)
        except yaml.YAMLError:
            continue
        if isinstance(workflow, dict) and SDK_JOB in (workflow.get("jobs") or {}):
            holders.append((path, text))
    if len(holders) != 1:
        raise ValueError(f"exactly one workflow must hold the `{SDK_JOB}` job (found {len(holders)})")
    problems = sdk_job_problems(holders[0][1])
    if problems:
        raise ValueError(f"{holders[0][0]}:\n" + "\n".join(problems))


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


# ---- Test-environment supply chain (the official OpenAI SDKs used by the end-to-end tests) --------------------------------------------------------------------
SDK_DIR = Path("crates/bot-cli/tests/openai_sdk")
HEX40, HEX64 = re.compile(r"[0-9a-f]{40}"), re.compile(r"[0-9a-f]{64}")
NPM_EXACT = re.compile(r"\d+\.\d+\.\d+")
PIP_REQUIREMENT = re.compile(r"^([A-Za-z0-9][A-Za-z0-9._-]*)==(\d+(?:\.\d+)+)(?:\s*\\)?\s*$")
PIP_HASH = re.compile(r"^\s+--hash=sha256:[0-9a-f]{64}(?:\s*\\)?\s*$")
PIP_EXACT = re.compile(r"[A-Za-z0-9._-]+(?:\[[A-Za-z0-9,._-]+\])?==\d+(?:\.\d+)*")
NPM_PACKAGE_EXACT = re.compile(r"(?:@[a-z0-9._-]+/)?[a-z0-9._-]+@\d+\.\d+\.\d+")
INSTALL_HOOKS = {"preinstall", "install", "postinstall", "prepublish", "preprepare", "prepare", "postprepare"}
PIPE_TO_SHELL = re.compile(r"\b(?:curl|wget)\b[^\n|]*\|\s*(?:sudo\s+)?(?:ba|z|da)?sh\b")
PIP_VALUE_OPTIONS = {"-i", "--index-url", "--extra-index-url", "-f", "--find-links", "-c", "--constraint", "-e", "--editable", "--trusted-host"}
PIP_GLOBAL_VALUE_OPTIONS = {"--python", "--proxy", "--retries", "--timeout", "--cache-dir", "--cert", "--client-cert", "--log"}
NPM_VALUE_OPTIONS = {"--prefix", "--loglevel", "--cache", "--registry"}
ENV_ASSIGN = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*=")


def python_lock_problems(text, openai_version):
    """A pip lock names every package as `name==x.y.z` with at least one `--hash=sha256:…`: no URL, VCS, range, index or editable install."""
    problems, name, hashes, seen = [], None, 0, {}
    for number, line in enumerate(text.splitlines(), 1):
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        requirement = PIP_REQUIREMENT.match(line)
        if requirement:
            if name is not None and hashes == 0:
                problems.append(f"requirements lock: `{name}` has no --hash=sha256")
            name, hashes = requirement.group(1).lower().replace("_", "-"), 0
            seen[name] = requirement.group(2)
        elif PIP_HASH.match(line) and name is not None:
            hashes += 1
        else:
            problems.append(f"requirements lock line {number}: `{line.strip()[:50]}` is not `name==x.y.z \\ --hash=sha256:<64 hex>` (no URL, range, option or editable)")
    if name is not None and hashes == 0:
        problems.append(f"requirements lock: `{name}` has no --hash=sha256")
    if openai_version is not None and seen.get("openai") != openai_version:
        problems.append(f"requirements lock: openai must be {openai_version}, not {seen.get('openai')}")
    return problems


def node_lock_problems(package_text, lock_text, openai_version):
    """package.json names exact versions and no install hooks; the lock gives every package an integrity hash, a registry URL and no install script."""
    try:
        package, lock = json.loads(package_text), json.loads(lock_text)
    except ValueError:
        return ["package.json or package-lock.json is not valid JSON"]
    problems = []
    for group in ("dependencies", "devDependencies", "optionalDependencies"):
        for name, version in (package.get(group) or {}).items():
            if not (isinstance(version, str) and NPM_EXACT.fullmatch(version)):
                problems.append(f"package.json {group}.{name}: `{version}` must be an exact x.y.z (no range, tag, URL or path)")
    for hook in sorted(INSTALL_HOOKS & set(package.get("scripts") or {})):
        problems.append(f"package.json scripts.{hook}: install hooks are not allowed")
    if (package.get("dependencies") or {}).get("openai") != openai_version:
        problems.append(f"package.json: openai must be {openai_version}")
    if not (isinstance(lock.get("lockfileVersion"), int) and lock["lockfileVersion"] >= 3):
        problems.append("package-lock.json: lockfileVersion must be 3 or newer")
    packages = lock.get("packages") if isinstance(lock.get("packages"), dict) else {}
    for path, entry in packages.items():
        if path == "":
            continue
        where = f"package-lock.json {path}"
        if not (isinstance(entry.get("integrity"), str) and entry["integrity"].startswith("sha512-")):
            problems.append(f"{where}: needs a sha512 integrity")
        if not str(entry.get("resolved", "")).startswith("https://registry.npmjs.org/"):
            problems.append(f"{where}: must resolve from https://registry.npmjs.org/")
        if entry.get("hasInstallScript") or entry.get("link"):
            problems.append(f"{where}: install scripts and links are not allowed")
        if not NPM_EXACT.fullmatch(str(entry.get("version", ""))):
            problems.append(f"{where}: needs an exact version")
    if (packages.get("node_modules/openai") or {}).get("version") != openai_version:
        problems.append(f"package-lock.json: node_modules/openai must be {openai_version}")
    return problems


def sdk_pin_problems(pin_text, python_lock, package_text, node_lock_text, subset_bytes):
    """`pin.json` names the official OpenAPI commit/hash and both SDK versions; the locks and the committed schema subset must agree with it."""
    try:
        pin = json.loads(pin_text)
    except ValueError:
        return ["pin.json: not valid JSON"]
    spec, sdk = pin.get("openapi") or {}, pin.get("sdk") or {}
    problems = []
    if not HEX40.fullmatch(str(spec.get("commit", ""))):
        problems.append("pin.json: openapi.commit must be a full 40-hex commit SHA")
    for key in ("sha256", "subset_sha256"):
        if not HEX64.fullmatch(str(spec.get(key, ""))):
            problems.append(f"pin.json: openapi.{key} must be 64 hex digits")
    if hashlib.sha256(subset_bytes).hexdigest() != spec.get("subset_sha256"):
        problems.append("openai-subset.json does not match pin.json openapi.subset_sha256 (regenerate it from the pinned spec)")
    python = re.fullmatch(r"openai==(\d+\.\d+\.\d+)", str(sdk.get("python", "")))
    node = re.fullmatch(r"openai@(\d+\.\d+\.\d+)", str(sdk.get("node", "")))
    if not (python and node):
        problems.append("pin.json: sdk.python must be `openai==x.y.z` and sdk.node `openai@x.y.z`")
    problems += python_lock_problems(python_lock, python.group(1) if python else None)
    problems += node_lock_problems(package_text, node_lock_text, node.group(1) if node else None)
    return problems


def check_sdk_pins():
    folder = SDK_DIR
    names = ("schema/pin.json", "requirements.lock.txt", "node/package.json", "node/package-lock.json", "schema/openai-subset.json", "expected.json", "sdk_run.py", "check_runners.sh")
    missing = [name for name in names if not (folder / name).is_file()]
    if missing:
        raise ValueError(f"{folder}: missing {', '.join(missing)} — the end-to-end SDK tests must be pinned")
    text = lambda name: (folder / name).read_text(encoding="utf-8")
    problems = sdk_pin_problems(text("schema/pin.json"), text("requirements.lock.txt"), text("node/package.json"), text("node/package-lock.json"), (folder / "schema/openai-subset.json").read_bytes())
    if problems:
        raise ValueError(f"{folder} pins:\n" + "\n".join(problems))


def strip_cmd_prefix(tokens):
    """Strip leading `VAR=val`, `env`, and `sudo` wrappers so prefixed `pip`/`npm`/`npx` commands are still inspected."""
    i = 0
    while i < len(tokens):
        tok = tokens[i]
        if ENV_ASSIGN.match(tok):
            i += 1
        elif tok in ("sudo", "env"):
            i += 1
            while i < len(tokens) and tokens[i].startswith("-"):
                flag = tokens[i]
                i += 2 if flag in ("-u", "-g", "-C") else 1
        else:
            break
    return tokens[i:]


def command_problems(run):
    """Install commands in a workflow step: pip needs exact versions or a hash lock, npm needs `--ignore-scripts` and exact packages, nothing pipes a download to a shell."""
    joined = re.sub(r"\\\n\s*", " ", run)
    active = [line for line in joined.splitlines() if not line.lstrip().startswith("#")]
    problems = [f"`{match.group(0)[:50]}…` pipes a download to a shell" for line in active for match in PIPE_TO_SHELL.finditer(line)]
    for line in active:
        for part in re.split(r"\s*(?:&&|\|\||;|\|)\s*", line):
            try:
                tokens = shlex.split(part)
            except ValueError:
                tokens = part.split()
            problems += pip_problems(tokens) + npm_problems(tokens)
    return problems


def pip_install_args(tokens):
    """Return the args after `pip ... install` (supporting global pip flags and `python -m pip`), or None."""
    cmd = strip_cmd_prefix(tokens)
    if not cmd:
        return None
    exe = Path(cmd[0]).name
    if exe in ("pip", "pip3") or exe.startswith("pip3."):
        rest = cmd[1:]
    elif exe.startswith("python") and len(cmd) >= 3 and cmd[1] == "-m" and cmd[2] in ("pip", "pip3"):
        rest = cmd[3:]
    else:
        return None
    i = 0
    while i < len(rest) and rest[i].startswith("-"):
        i += 2 if rest[i] in PIP_GLOBAL_VALUE_OPTIONS else 1
    if i < len(rest) and rest[i] == "install":
        return rest[i + 1:]
    return None


def pip_problems(tokens):
    args = pip_install_args(tokens)
    if args is None:
        return []
    problems, skip = [], False
    for arg in args:
        if skip:
            skip = False
        elif arg in ("-r", "--requirement"):
            skip = True
            if "--require-hashes" not in args:
                problems.append("`pip install -r` needs `--require-hashes`")
        elif arg.startswith("--requirement=") or (arg.startswith("-r") and len(arg) > 2):
            if "--require-hashes" not in args:
                problems.append("`pip install -r` needs `--require-hashes`")
        elif arg in PIP_VALUE_OPTIONS:
            skip = True
            problems.append(f"`pip install {arg}` can pull unpinned code from elsewhere")
        elif any(arg.startswith(opt + "=") for opt in PIP_VALUE_OPTIONS if opt.startswith("--")) or any(
            arg.startswith(opt) and len(arg) > 2 for opt in PIP_VALUE_OPTIONS if len(opt) == 2
        ):
            problems.append(f"`pip install {arg.split('=', 1)[0]}` can pull unpinned code from elsewhere")
        elif not arg.startswith("-") and not PIP_EXACT.fullmatch(arg):
            problems.append(f"`pip install {arg}` must name an exact version (`name==x.y.z`)")
    return problems


def npm_problems(tokens):
    cmd = strip_cmd_prefix(tokens)
    if not cmd:
        return []
    exe = Path(cmd[0]).name
    if exe == "npx":
        return ["`npx` runs unpinned code — install an exact version first"]
    if exe != "npm":
        return []
    args = cmd[1:]
    command, cmd_idx, skip = "", -1, False
    for idx, arg in enumerate(args):
        if skip:
            skip = False
        elif arg in NPM_VALUE_OPTIONS:
            skip = True
        elif not arg.startswith("-"):
            command, cmd_idx = arg, idx
            break
    if command == "exec":
        return ["`npm exec` runs unpinned code — install an exact version first"]
    if command not in {"install", "i", "add", "ci", "install-test", "it", "update", "up"}:
        return []
    problems, skip = [], False
    if "--ignore-scripts" not in args:
        problems.append(f"`npm {command}` needs `--ignore-scripts`")
    if any(a == "--registry" or a.startswith("--registry=") for a in args):
        problems.append(f"`npm {command} --registry` can pull packages from another registry")
    for arg in args[cmd_idx + 1:]:
        if skip:
            skip = False
        elif arg in NPM_VALUE_OPTIONS:
            skip = True
        elif not arg.startswith("-") and not NPM_PACKAGE_EXACT.fullmatch(arg):
            problems.append(f"`npm {command} {arg}` must name an exact version (`name@x.y.z`)")
    return problems


def check_docs():
    """Documents must name what the code defines and cite only paths that exist (the rules live in docs_contract.py next to this file; `-I` drops this folder from `sys.path`, so load it by path)."""
    for name in ("docs_contract", "drift_guard"):
        spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(f"{name}.py"))
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        module.check()


def main():
    check_files()
    check_workspace()
    check_examples()
    check_workflows()
    check_toolchain()
    check_sdk_pins()
    check_sdk_job()
    check_docs()


if __name__ == "__main__":
    main()
