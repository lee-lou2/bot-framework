#!/usr/bin/env python3
"""requirements.lock.txt를 다시 만든다: 고정한 최상위 패키지(openai·jsonschema·PyYAML)를 임시 venv에 설치해 닫힘을 풀고, 각 패키지의 CPython 3.12 휠(CI의
Linux x86_64·로컬 macOS arm64)과 순수 파이썬 휠의 sha256을 PyPI JSON에서 읽어 `--hash`로 적는다. 올릴 때만 돌린다(네트워크 필요).
사용: python3 lock_requirements.py > requirements.lock.txt   (PyYAML은 공식 스키마 뽑기·검증에만 쓴다)"""
import json
import re
import subprocess
import sys
import tempfile
import urllib.request

TOP = ["openai==2.29.0", "jsonschema==4.25.1", "PyYAML==6.0.2"]
WHEELS = re.compile(
    r"(py3|py2\.py3)-none-any\.whl$"
    r"|cp312-(cp312|abi3)-(manylinux[\w.]*x86_64|macosx_[\w]*arm64|macosx_[\w]*universal2)\.whl$"
    r"|cp3(9|10|11|12)-abi3-(manylinux[\w.]*x86_64|macosx_[\w]*arm64|macosx_[\w]*universal2)\.whl$"
)


def pypi(name, version):
    url = f"https://pypi.org/pypi/{name}/{version}/json"
    with urllib.request.urlopen(urllib.request.Request(url, headers={"Accept": "application/json"}), timeout=30) as reply:
        return json.load(reply)


def main():
    with tempfile.TemporaryDirectory() as folder:
        subprocess.run([sys.executable, "-m", "venv", folder], check=True)
        python = f"{folder}/bin/python"
        subprocess.run([python, "-m", "pip", "install", "--quiet", "--disable-pip-version-check", *TOP], check=True)
        listed = json.loads(subprocess.run([python, "-m", "pip", "list", "--format=json"], check=True, capture_output=True, text=True).stdout)
    print("# 공식 openai SDK(Python) 시험 환경: 정확 판본 + 해시. 설치: pip install --require-hashes --no-deps -r requirements.lock.txt")
    print("# CPython 3.12의 Linux x86_64·macOS arm64 휠만 허용한다. 다시 만들기: python3 lock_requirements.py > requirements.lock.txt")
    for item in sorted(listed, key=lambda i: i["name"].lower()):
        name, version = item["name"], item["version"]
        if name.lower() in ("pip", "setuptools", "wheel"):
            continue
        files = [f for f in pypi(name, version)["urls"] if f["packagetype"] == "bdist_wheel" and not f["yanked"] and WHEELS.search(f["filename"])]
        if not files:
            raise SystemExit(f"{name}=={version}: 쓸 수 있는 휠이 없다")
        hashes = sorted({f["digests"]["sha256"] for f in files})
        print(f"{name.lower().replace('_', '-')}=={version} \\")
        print(" \\\n".join(f"    --hash=sha256:{h}" for h in hashes))


main()
