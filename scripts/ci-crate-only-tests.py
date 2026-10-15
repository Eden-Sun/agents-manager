#!/usr/bin/env python3
"""Recognise explicit crate-only test modules; unrecognised Rust falls back to daemon checks."""
import re
import sys
from pathlib import Path

GATE = re.compile(r'#\[cfg\(all\(test,\s*not\(feature\s*=\s*"daemon-test-harness"\)\)\)\]')
RAW = re.compile(r'(?:[bc])?r(#{0,255})"')
CHAR = re.compile(r"'(?:\\(?:u\{[^}]+\}|x[0-9a-fA-F]{2}|.)|[^'\\\n])'")


def mask_rust(source: str) -> str:
    """Blank strings/comments without shifting offsets, so fixture text cannot act as a gate."""
    masked = list(source)
    pos = 0
    while pos < len(source):
        end = None
        if source.startswith("//", pos):
            newline = source.find("\n", pos)
            end = len(source) if newline < 0 else newline
        elif source.startswith("/*", pos):
            depth, end = 1, pos + 2
            while depth and end < len(source):
                if source.startswith("/*", end):
                    depth, end = depth + 1, end + 2
                elif source.startswith("*/", end):
                    depth, end = depth - 1, end + 2
                else:
                    end += 1
            if depth:
                raise ValueError("unterminated comment")
        elif match := RAW.match(source, pos):
            close = source.find('"' + match[1], match.end())
            if close < 0:
                raise ValueError("unterminated raw string")
            end = close + 1 + len(match[1])
        elif source[pos] == '"':
            end = pos + 1
            while end < len(source):
                if source[end] == "\\":
                    end += 2
                elif source[end] == '"':
                    end += 1
                    break
                else:
                    end += 1
            else:
                raise ValueError("unterminated string")
        elif match := CHAR.match(source, pos):
            end = match.end()
        if end is None:
            pos += 1
        else:
            for index in range(pos, end):
                if source[index] != "\n":
                    masked[index] = " "
            pos = end
    return "".join(masked)


def crate_only(source: str) -> bool:
    masked = mask_rust(source)
    retained = list(masked)
    found = False
    for gate in GATE.finditer(source):
        if not masked.startswith("#[cfg", gate.start()):
            continue
        declaration = re.match(r'\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+\w+\s*([;{])', masked[gate.end():])
        if not declaration:
            return False
        end = gate.end() + declaration.end()
        if declaration[1] == "{":
            depth = 1
            while depth and end < len(masked):
                depth += (masked[end] == "{") - (masked[end] == "}")
                end += 1
            if depth:
                return False
        retained[gate.start():end] = " " * (end - gate.start())
        found = True
    # Other test attributes or module mounts might contain App-backed cases; never suppress them.
    return found and not re.search(r'#\[(?:tokio::)?test\b|\bmod\s+\w+\s*[;{]', "".join(retained))


def owned_path(path: Path) -> bool:
    if crate_only(path.read_text()):
        return True
    # External test files inherit the cfg on their parent module declaration.
    for parent in (path.parent / "mod.rs", path.parent.with_suffix(".rs")):
        if not parent.is_file() or parent == path:
            continue
        source = parent.read_text()
        masked = mask_rust(source)
        for gate in GATE.finditer(source):
            if not masked.startswith("#[cfg", gate.start()):
                continue
            declaration = re.match(r'\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*;', masked[gate.end():])
            if declaration and path.resolve() in {
                (parent.parent / (declaration[1] + ".rs")).resolve(),
                (parent.parent / declaration[1] / "mod.rs").resolve(),
            }:
                return True
    return False


if __name__ == "__main__":
    try:
        owned = owned_path(Path(sys.argv[1]))
    except (OSError, ValueError, IndexError):
        owned = False
    sys.exit(0 if owned else 1)
