"""Validates OpenAI responses against the pinned OpenAPI 3.0 subset (schema/openai-subset.json), verifying pin.json hashes before compiling Draft 2020-12 validators."""
import hashlib
import json
import pathlib

from jsonschema import Draft202012Validator

SCHEMA_DIR = pathlib.Path(__file__).resolve().parent.parent / "schema"
SUBSET = SCHEMA_DIR / "openai-subset.json"
PIN = SCHEMA_DIR / "pin.json"
COMMIT = "498c71ddf6f1c45b983f972ccabca795da211a3e"
SPEC_SHA256 = "6a6c681b182002de4ad335fa80214158f2d8f383a418f865c87876c3ed67cad9"


def to_json_schema(node):
    """Converts OpenAPI 3.0 schema nodes to JSON Schema 2020-12 (nullable types/enums/$refs and boolean exclusive bounds)."""
    if isinstance(node, list):
        return [to_json_schema(value) for value in node]
    if not isinstance(node, dict):
        return node
    out = {}
    for key, value in node.items():
        out[key] = {name: to_json_schema(sub) for name, sub in value.items()} if key == "properties" and isinstance(value, dict) else to_json_schema(value)
    for key in ("exclusiveMinimum", "exclusiveMaximum"):
        if out.get(key) is True:
            out[key] = out.pop("minimum" if key == "exclusiveMinimum" else "maximum")
        elif out.get(key) is False:
            del out[key]
    if out.pop("nullable", False) is True:
        if "enum" in out and None not in out["enum"]:
            out["enum"] = out["enum"] + [None]
        if "type" in out and not any(k in out for k in ("$ref", "allOf", "anyOf", "oneOf")):
            kinds = out["type"] if isinstance(out["type"], list) else [out["type"]]
            out["type"] = kinds + ["null"]
        else:
            out = {"anyOf": [out, {"type": "null"}]}
    return out


def leftovers(node, path=""):
    """Yields remaining OpenAPI 3.0-only keywords that would silently loosen Draft 2020-12 validation."""
    if isinstance(node, dict):
        for key, value in node.items():
            if key in ("nullable",) or (key in ("exclusiveMinimum", "exclusiveMaximum") and isinstance(value, bool)):
                yield f"{path}/{key}"
            yield from leftovers(value, f"{path}/{key}")
    elif isinstance(node, list):
        for i, value in enumerate(node):
            yield from leftovers(value, f"{path}[{i}]")


class Schema:
    def __init__(self, subset=SUBSET, pin=PIN):
        raw = pathlib.Path(subset).read_bytes()
        pinned = json.loads(pathlib.Path(pin).read_text(encoding="utf-8"))["openapi"]
        assert pinned["commit"] == COMMIT and pinned["sha256"] == SPEC_SHA256, "pin.json does not match hardcoded spec pin"
        assert hashlib.sha256(raw).hexdigest() == pinned["subset_sha256"], "openai-subset.json does not match pin.json subset_sha256"
        self.doc = json.loads(raw)
        self.source = self.doc["x-source"]
        self.request_nullable = frozenset(self.source["request_nullable"])
        assert self.source["commit"] == COMMIT and self.source["spec_sha256"] == SPEC_SHA256 and self.source["openapi"].startswith("3.0"), self.source
        self.components = to_json_schema(self.doc["components"])
        assert not list(leftovers(self.components)), list(leftovers(self.components))
        self._validators = {}

    def validator(self, name):
        if name not in self._validators:
            root = {"$schema": "https://json-schema.org/draft/2020-12/schema", "components": self.components, "$ref": f"#/components/schemas/{name}"}
            Draft202012Validator.check_schema(root)
            self._validators[name] = Draft202012Validator(root)
        return self._validators[name]

    def errors(self, name, instance):
        found = sorted(self.validator(name).iter_errors(instance), key=lambda e: [str(p) for p in e.absolute_path])
        return [f"{'/'.join(str(p) for p in e.absolute_path) or '<root>'}: {e.message[:160]}" for e in found]

    def check(self, name, instance):
        problems = self.errors(name, instance)
        if problems:
            raise AssertionError(f"{len(problems)} {name} violation(s) (schema {self.source['version']} @ {self.source['commit'][:12]}):\n  " + "\n  ".join(problems))
