#!/usr/bin/env python3
"""Docs never send a reader to a developer console except under "Advanced: use your own app"
(TASK-service-apps-builtin.md, rule 5). Checks the Markdown, shell and YAML files git tracks;
a console address is allowed only inside a section whose heading says "Advanced: use your own app".
Exit 1 with the places otherwise."""
import re
import subprocess
import sys

CONSOLES = re.compile(
    r"developer\.spotify\.com|developer\.tidal\.com|console\.cloud\.google\.com|last\.fm/api/account"
    r"|devportal\.jamendo\.com|acoustid\.org/new-application|developers\.deezer\.com"
    r"|portal\.azure\.com|entra\.microsoft\.com"
)
HEADING = re.compile(r"^(#+)\s+(.*)$")


def main() -> int:
    files = subprocess.run(
        ["git", "ls-files", "*.md", "*.sh", "*.yaml", "*.yml"], capture_output=True, text=True, check=True
    ).stdout.split()
    bad = []
    for path in files:
        if path.startswith((".github/", "docs/archive/", "docs/adr/")):
            continue
        advanced_level = None
        with open(path, encoding="utf-8", errors="replace") as f:
            for n, line in enumerate(f, 1):
                h = HEADING.match(line) if path.endswith(".md") else None
                if h:
                    level = len(h.group(1))
                    if advanced_level is not None and level <= advanced_level:
                        advanced_level = None
                    if "Advanced: use your own app" in h.group(2):
                        advanced_level = level
                if advanced_level is None and CONSOLES.search(line):
                    bad.append(f"{path}:{n}: {line.strip()[:120]}")
    for b in bad:
        print(b)
    if bad:
        print(f'\n{len(bad)} developer-console link(s) outside an "Advanced: use your own app" section')
        return 1
    print(f"no developer-console links outside Advanced in {len(files)} files")
    return 0


if __name__ == "__main__":
    sys.exit(main())
