#!/usr/bin/env python3
"""OpenAI 공개 OpenAPI(고정 커밋)에서 응답 적합성 시험에 필요한 스키마만 뽑아 `openai-subset.json`으로 쓴다. 원본은 4 MB라 저장소에 두지 않고, 커밋 SHA와 sha256을 `pin.json`에 고정한다.
사용: extract_subset.py <openapi.yaml> [--write|--check]
  (없음)  뽑은 결과의 sha256만 찍는다   --write  openai-subset.json과 pin.json의 subset_sha256을 쓴다   --check  저장소의 것과 한 글자도 다르지 않아야 한다(CI)
원본 sha256이 pin.json과 다르면 아무것도 하지 않고 멈춘다. 필요한 것: PyYAML(시험 의존성 잠금에 있다)."""
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
    """시험에 쓰지 않는 설명·예시·`x-*` 확장을 걷어 낸다. 속성 이름이 `description`·`default`·`format`인 것은 지키려고 `properties` 아래 이름은 그대로 둔다."""
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
    """`CreateChatCompletionRequest`의 최상위 필드 중 값이 null일 수 있는 것(자기 자신이 `nullable: true`이거나 `$ref` 대상이 그렇다). 요청에서 null은 이 필드에 한해 «보내지 않음»과 같다."""
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
        sys.exit(f"원본 sha256이 고정값과 달라요: {digest} != {pin['openapi']['sha256']}")
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
        sys.exit(0 if same else f"저장소의 openai-subset.json이 고정 원본에서 다시 뽑은 것과 달라요(새로 뽑은 sha256 {digest})")
    if mode == "--write":
        SUBSET.write_bytes(out)
        pin["openapi"]["subset_sha256"] = digest
        PIN.write_text(json.dumps(pin, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    print(digest)


if __name__ == "__main__":
    main()
