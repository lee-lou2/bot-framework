"""OpenAI 공개 OpenAPI(3.0, 고정 커밋) 대비 응답 적합성. 저장소에는 원본(4 MB) 대신 필요한 스키마만 뽑은 `schema/openai-subset.json`을 두고, 쓰기 전에 pin.json의 커밋·sha256과 맞는지 먼저 확인한다
(오프라인에서도 같다 — 고정과 다른 스키마로는 아무것도 검사하지 않는다). 3.0의 `nullable`(`$ref`·enum 옆 포함)은 JSON Schema로 옮겨 검사한다. 추가 필드는 막지 않는다(공식 스키마가 닫지 않는다)."""
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
    """OpenAPI 3.0 스키마 조각 → JSON Schema 2020-12. `nullable: true`는 형에 null을 더하고(enum이면 null도 허용), `$ref`·조합 옆이면 `anyOf [원래, null]`로 감싼다."""
    if isinstance(node, list):
        return [to_json_schema(value) for value in node]
    if not isinstance(node, dict):
        return node
    out = {}
    for key, value in node.items():
        out[key] = {name: to_json_schema(sub) for name, sub in value.items()} if key == "properties" and isinstance(value, dict) else to_json_schema(value)
    for key in ("exclusiveMinimum", "exclusiveMaximum"):  # 3.0의 불리언 꼴 → 2020-12의 수 꼴
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
    """변환 뒤에 남으면 안 되는 3.0 전용 꼴(검사가 조용히 느슨해지는 것을 막는다)."""
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
        assert pinned["commit"] == COMMIT and pinned["sha256"] == SPEC_SHA256, "pin.json의 원본 고정이 코드의 고정과 달라요"
        assert hashlib.sha256(raw).hexdigest() == pinned["subset_sha256"], "openai-subset.json이 pin.json의 sha256과 달라요(고정된 공식 스키마가 아니다)"
        self.doc = json.loads(raw)
        self.source = self.doc["x-source"]
        self.request_nullable = frozenset(self.source["request_nullable"])  # 요청에서 null이 «보내지 않음»과 같은 필드(공식 요청 스키마의 nullable)
        assert self.source["commit"] == COMMIT and self.source["spec_sha256"] == SPEC_SHA256 and self.source["openapi"].startswith("3.0"), self.source
        self.components = to_json_schema(self.doc["components"])
        assert not list(leftovers(self.components)), list(leftovers(self.components))
        self._validators = {}

    def validator(self, name):
        """`components/schemas/<name>`을 뿌리로 하는 검사기(`$ref`는 같은 문서 안에서 푼다)."""
        if name not in self._validators:
            root = {"$schema": "https://json-schema.org/draft/2020-12/schema", "components": self.components, "$ref": f"#/components/schemas/{name}"}
            Draft202012Validator.check_schema(root)
            self._validators[name] = Draft202012Validator(root)
        return self._validators[name]

    def errors(self, name, instance):
        """위반 목록(경로: 이유). 비어 있으면 적합하다."""
        found = sorted(self.validator(name).iter_errors(instance), key=lambda e: [str(p) for p in e.absolute_path])
        return [f"{'/'.join(str(p) for p in e.absolute_path) or '<root>'}: {e.message[:160]}" for e in found]

    def check(self, name, instance):
        problems = self.errors(name, instance)
        if problems:
            raise AssertionError(f"{name} 위반 {len(problems)}건 (스키마 {self.source['version']} @ {self.source['commit'][:12]}):\n  " + "\n  ".join(problems))
