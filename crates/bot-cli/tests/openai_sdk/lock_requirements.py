#!/usr/bin/env python3
"""Regenerates requirements.lock.txt by resolving pinned top-level packages in a temp venv and fetching PyPI sha256 wheel hashes for CPython 3.12 (Linux x86_64, macOS arm64) and pure-Python wheels.
Usage: python3 lock_requirements.py > requirements.lock.txt"""
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
    print("# Official openai Python SDK test environment: exact versions + hashes. Install: pip install --require-hashes --no-deps -r requirements.lock.txt")
    print("# Allows CPython 3.12 Linux x86_64 and macOS arm64 wheels only. Regenerate: python3 lock_requirements.py > requirements.lock.txt")
    for item in sorted(listed, key=lambda i: i["name"].lower()):
        name, version = item["name"], item["version"]
        if name.lower() in ("pip", "setuptools", "wheel"):
            continue
        files = [f for f in pypi(name, version)["urls"] if f["packagetype"] == "bdist_wheel" and not f["yanked"] and WHEELS.search(f["filename"])]
        if not files:
            raise SystemExit(f"{name}=={version}: no compatible wheel found")
        hashes = sorted({f["digests"]["sha256"] for f in files})
        print(f"{name.lower().replace('_', '-')}=={version} \\")
        print(" \\\n".join(f"    --hash=sha256:{h}" for h in hashes))


main()
