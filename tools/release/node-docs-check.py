"""Checks a node download archive folder (made by node-docs.sh): every relative link in its
Markdown files that points at a file the archive is meant to carry (README.md, docs/deploy.md,
docs/guides/) resolves inside the folder. Links to the rest of the repo are left to GitHub.

Usage: python3 -I tools/release/node-docs-check.py <archive folder>
"""

import pathlib
import re
import sys

CARRIED = ("README.md", "docs/deploy.md", "docs/guides/")
LINK = re.compile(r"\]\(([^)\s]+)\)")


def carried(rel: str) -> bool:
    return any(rel == c or (c.endswith("/") and rel.startswith(c)) for c in CARRIED)


def main() -> int:
    root = pathlib.Path(sys.argv[1]).resolve()
    files = sorted(root.rglob("*.md"))
    if not (root / "README.md").is_file():
        print(f"{root}: no README.md", file=sys.stderr)
        return 1
    bad = []
    checked = 0
    for f in files:
        for target in LINK.findall(f.read_text(encoding="utf-8")):
            path = target.split("#", 1)[0]
            if not path or re.match(r"^[a-z][a-z0-9+.-]*:", path):
                continue
            resolved = (f.parent / path).resolve()
            try:
                rel = resolved.relative_to(root).as_posix()
            except ValueError:
                bad.append(f"{f.relative_to(root)}: {target} leaves the archive")
                continue
            if not carried(rel + ("/" if resolved.is_dir() else "")):
                continue
            checked += 1
            if not resolved.exists():
                bad.append(f"{f.relative_to(root)}: {target} is not in the archive")
    for b in bad:
        print(b, file=sys.stderr)
    print(f"{len(files)} files, {checked} links between them checked, {len(bad)} broken")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
