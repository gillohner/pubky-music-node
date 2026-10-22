#!/usr/bin/env python3
"""No client secret, private key or keystore in the repository or in a built binary
(STEWARD-SHARED-APPS.md: only public values ship; client secrets never do).

    python3 -I tools/secret-scan.py [--repo DIR] [--binary FILE ...] [--values ENVFILE ...]

* --repo (default .): every tracked file (git ls-files) is checked for secret-shaped values:
  a non-empty `client_secret` / `*_SECRET` / `password` assignment, a PEM private key, and
  keystore files.
* --binary: the same patterns over a built binary's printable strings.
* --values: KEY=VALUE files (e.g. the owner's tidal.env). Every value whose key names a secret
  (SECRET, PASSWORD, PRIVATE, TOKEN) must appear in neither the repository nor the binaries.
  Values are never printed: a hit names the key and the file only.
"""
import argparse
import os
import re
import subprocess
import sys

# A secret-looking assignment: the key names a secret and the value is a literal of 12+ characters.
ASSIGN = re.compile(
    rb"""(?ix)
    \b([a-z0-9_]*(?:client_secret|_secret|secret_key|api_secret|password))\b
    \s*[:=]\s*
    ["']([^"'\s$<>{}]{12,})["']
    """
)
PEM = re.compile(rb"-----BEGIN (?:RSA |EC |OPENSSH |ENCRYPTED )?PRIVATE KEY-----")
KEYSTORE_EXT = (".jks", ".keystore", ".p12", ".pfx")
# Literal values that are clearly placeholders in docs and tests.
PLACEHOLDER = re.compile(rb"(?i)^(?:x+|\*+|changeme|example|placeholder|your[-_].*|<.*>|test[-_].*|.*-test|ci-.*|dummy.*|sec(?:ret)?|redacted)$")
# Files where secret-shaped literals are test fixtures, not secrets.
FIXTURE_DIRS = ("tests/fixtures/",)


def tracked(repo):
    out = subprocess.run(["git", "-C", repo, "ls-files", "-z"], check=True, capture_output=True).stdout
    return [p.decode() for p in out.split(b"\0") if p]


def strings(data, minimum=8):
    return b"\n".join(re.findall(rb"[\x20-\x7e]{%d,}" % minimum, data))


def env_values(paths):
    vals = []
    for p in paths:
        with open(p, "rb") as f:
            for line in f:
                line = line.strip()
                if not line or line.startswith(b"#") or b"=" not in line:
                    continue
                k, v = line.split(b"=", 1)
                k = k.replace(b"export ", b"").strip()
                v = v.strip().strip(b"'\"")
                if re.search(rb"(?i)secret|password|private|token", k) and len(v) >= 8:
                    vals.append((k.decode(errors="replace"), os.path.basename(p), v))
    return vals


def scan_text(name, data, problems):
    for m in ASSIGN.finditer(data):
        if not PLACEHOLDER.match(m.group(2)):
            line = data.count(b"\n", 0, m.start()) + 1
            problems.append(f"{name}:{line}: a literal value for {m.group(1).decode()}")
    if PEM.search(data):
        problems.append(f"{name}: a PEM private key")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--repo", default=".")
    ap.add_argument("--binary", nargs="*", default=[])
    ap.add_argument("--values", nargs="*", default=[])
    a = ap.parse_args()
    problems = []
    secrets = env_values(a.values)
    files = tracked(a.repo)
    for rel in files:
        if rel.lower().endswith(KEYSTORE_EXT):
            problems.append(f"{rel}: a keystore file is tracked")
            continue
        path = os.path.join(a.repo, rel)
        if not os.path.isfile(path) or any(d in rel for d in FIXTURE_DIRS):
            continue
        with open(path, "rb") as f:
            data = f.read()
        if b"\0" not in data[:4096]:
            scan_text(rel, data, problems)
        for key, src, v in secrets:
            if v in data:
                problems.append(f"{rel}: holds the value of {key} from {src}")
    for b in a.binary:
        with open(b, "rb") as f:
            data = f.read()
        scan_text(os.path.basename(b), strings(data), problems)
        for key, src, v in secrets:
            if v in data:
                problems.append(f"{os.path.basename(b)}: holds the value of {key} from {src}")
    for p in problems:
        print("FAIL", p)
    if problems:
        print(f"{len(problems)} problems")
        return 1
    what = f"{len(files)} tracked files" + (f" and {len(a.binary)} binaries" if a.binary else "")
    print(f"no secrets in {what}" + (f" (checked {len(secrets)} known values)" if secrets else ""))
    return 0


if __name__ == "__main__":
    sys.exit(main())
