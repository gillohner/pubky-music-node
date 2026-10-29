"""Checks deploy/truenas/docker-compose.yaml against its own instructions: the header says how
many paths to edit, and that is how many host paths the volumes mount.

Usage: python3 -I tools/truenas-check.py [deploy/truenas/docker-compose.yaml]
"""

import pathlib
import re
import sys

WORDS = {"one": 1, "two": 2, "three": 3, "four": 4, "five": 5}


def main() -> int:
    path = pathlib.Path(sys.argv[1] if len(sys.argv) > 1 else "deploy/truenas/docker-compose.yaml")
    text = path.read_text(encoding="utf-8")
    said = re.search(r"Edit the (\w+) paths", text)
    if not said or said.group(1) not in WORDS:
        print(f"{path}: no 'Edit the <number> paths' line", file=sys.stderr)
        return 1
    host_paths = re.findall(r"^\s+- (/[^:\s]+):", text, re.M)
    if WORDS[said.group(1)] != len(host_paths):
        print(
            f"{path}: says 'Edit the {said.group(1)} paths' but mounts {len(host_paths)}: {host_paths}",
            file=sys.stderr,
        )
        return 1
    print(f"{path}: {len(host_paths)} paths to edit, as it says")
    return 0


if __name__ == "__main__":
    sys.exit(main())
