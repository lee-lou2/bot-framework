#!/usr/bin/env python3
"""Extracts the response-conformance schema subset from a pinned OpenAI OpenAPI spec into openai-subset.json.
Usage: extract_subset.py <openapi.yaml> [--write|--check]
Refuses to run if the input sha256 does not match pin.json."""
import hashlib
import json
import pathlib
import sys

import yaml

HERE = pathlib.Path(__file__).resolve().parent
PIN = HERE / "pin.json"
SUBSET = HERE / "openai-subset.json"
ROOTS = ["Model", "ListModelsResponse", "CreateChatCompletionResponse", "CreateChatCompletionStreamResponse", "ErrorResponse", "Error"]
NOISE = {"description", "title", "example", "examples", "deprecated", "default", "format"}


def refs(node):
    if isinstance(node, dict):
        for key, value in node.items():
            if key == "$ref" and isinstance(value, str):
                yield value.rsplit("/", 1)[-1]
            else:
                yield from refs(value)
    elif isinstance(node, list):
        for value in node:
            yield from refs(value)


def clean(node):
    """Strips non-validation metadata and x-* extensions while preserving property names under `properties`."""
    if isinstance(node, dict):
        out = {}
        for key, value in node.items():
            if key in NOISE or key.startswith("x-"):
                continue
            out[key] = {k: clean(v) for k, v in value.items()} if key == "properties" and isinstance(value, dict) else clean(value)
        return out
    if isinstance(node, list):
        return [clean(value) for value in node]
    return node


def request_nullable(schemas):
    """Returns top-level fields of CreateChatCompletionRequest that allow null directly or via $ref."""
    def resolve(node):
        while isinstance(node, dict) and "$ref" in node and not node.get("nullable"):
            node = schemas[node["$ref"].rsplit("/", 1)[-1]]
        return node

    def properties(schema):
        out = {}
        for part in schema.get("allOf") or [schema]:
            if "$ref" in part:
                out.update(properties(schemas[part["$ref"].rsplit("/", 1)[-1]]))
            out.update(part.get("properties") or {})
        return out

    return sorted(name for name, node in properties(schemas["CreateChatCompletionRequest"]).items() if isinstance(resolve(node), dict) and resolve(node).get("nullable") is True)


def build(raw, pin):
    digest = hashlib.sha256(raw).hexdigest()
    if digest != pin["openapi"]["sha256"]:
        sys.exit(f"source sha256 does not match pin.json: {digest} != {pin['openapi']['sha256']}")
    spec = yaml.load(raw.decode("utf-8"), Loader=getattr(yaml, "CSafeLoader", yaml.SafeLoader))
    schemas = spec["components"]["schemas"]
    wanted, todo = set(), list(ROOTS)
    while todo:
        name = todo.pop()
        if name not in wanted:
            wanted.add(name)
            todo += list(refs(schemas[name]))
    source = {"repo": pin["openapi"]["repo"], "commit": pin["openapi"]["commit"], "spec_sha256": digest, "openapi": spec["openapi"], "version": spec["info"]["version"], "roots": ROOTS, "request_nullable": request_nullable(schemas)}
    lines = ["{", f' "x-source": {json.dumps(source, ensure_ascii=False, sort_keys=True)},', ' "components": {"schemas": {']
    names = sorted(wanted)
    for i, name in enumerate(names):
        lines.append(f'  {json.dumps(name)}: {json.dumps(clean(schemas[name]), ensure_ascii=False, sort_keys=True)}{"," if i < len(names) - 1 else ""}')
    lines += [" }}", "}"]
    return ("\n".join(lines) + "\n").encode("utf-8")


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    mode = next((a for a in sys.argv[1:] if a.startswith("--")), "")
    pin = json.loads(PIN.read_text(encoding="utf-8"))
    out = build(pathlib.Path(args[0]).read_bytes(), pin)
    digest = hashlib.sha256(out).hexdigest()
    if mode == "--check":
        same = SUBSET.read_bytes() == out and pin["openapi"]["subset_sha256"] == digest
        sys.exit(0 if same else f"committed openai-subset.json differs from regenerated output (new sha256 {digest})")
    if mode == "--write":
        SUBSET.write_bytes(out)
        pin["openapi"]["subset_sha256"] = digest
        PIN.write_text(json.dumps(pin, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    print(digest)


if __name__ == "__main__":
    main()
