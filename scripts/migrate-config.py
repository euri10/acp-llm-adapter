#!/usr/bin/env python3
"""Report every config that invokes acp-debug.sh, with its replacement.

The wrapper is going away, so anything launching an agent through it stops
working. This reports what to change rather than editing editor configs in
place: the correct replacement differs per entry, and a wrong automated rewrite
to a dotfile is worse than a list you apply yourself.

Three cases, and they are not interchangeable:

  * This project's own adapter          -> drop the wrapper entirely.
    The adapter logs its own sessions, so wrapping it would double-record.

  * A foreign ACP agent                 -> acp-proxy -- <command>
    claude-agent-acp, codex-acp, copilot and friends.

  * A --version or similar one-shot     -> drop the wrapper entirely.
    These open no session. Wrapping them is what filled the old state
    directory with hundreds of near-empty files.
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

WRAPPER = "acp-debug.sh"
ADAPTER_BINARY = "acp-llm-adapter"

# Directories that are caches or checkouts rather than configuration.
SKIP_PARTS = {".git", "node_modules", "target", ".cache", "undo"}

SEARCH_SUFFIXES = {".lua", ".json", ".jsonc", ".toml", ".yaml", ".yml", ".vim"}


def strip_wrapper_paths(text: str) -> str:
    """Remove the wrapper's own path from text before classifying it.

    The wrapper lives inside a directory named after this project, so its path
    contains the adapter's name. Left in, every reference would look like a
    reference to the adapter itself.
    """
    return " ".join(token for token in text.split() if WRAPPER not in token)


def classify(line: str, following: str) -> tuple[str, str]:
    """Return (kind, advice) for one wrapper reference.

    `following` is the nearby text where the wrapped command usually appears,
    since configs generally put `command` and `args` on separate lines.
    """
    context = strip_wrapper_paths(f"{line} {following}")

    if "--version" in context:
        return (
            "one-shot",
            "drop the wrapper: run the command directly. Version checks open "
            "no session, and wrapping them is what filled the old state "
            "directory with near-empty files.",
        )
    if ADAPTER_BINARY in context:
        return (
            "own adapter",
            f"drop the wrapper: run {ADAPTER_BINARY} directly. "
            "It writes its own per-session logs.",
        )
    return (
        "foreign agent",
        "replace the wrapper with acp-proxy and pass the agent after `--`.",
    )


def scan(roots: list[Path], context_lines: int) -> int:
    findings = 0

    for root in roots:
        if not root.exists():
            continue
        candidates = [root] if root.is_file() else sorted(root.rglob("*"))
        for path in candidates:
            if not path.is_file():
                continue
            if set(path.parts) & SKIP_PARTS:
                continue
            if path.is_file() and path.suffix not in SEARCH_SUFFIXES:
                continue
            try:
                lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
            except OSError:
                continue
            if not any(WRAPPER in line for line in lines):
                continue

            print(f"\n{path}")
            for index, line in enumerate(lines):
                if WRAPPER not in line:
                    continue
                findings += 1
                nearby = lines[index + 1 : index + 1 + context_lines]
                following = " ".join(nearby)
                kind, advice = classify(line, following)
                wrapped = next(
                    (text.strip() for text in nearby if "args" in text), ""
                )
                print(f"  line {index + 1}: [{kind}]")
                print(f"    {line.strip()}")
                if wrapped:
                    print(f"    {wrapped}")
                print(f"    -> {advice}")

    return findings


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Report configs that launch agents through acp-debug.sh."
    )
    parser.add_argument(
        "roots",
        nargs="*",
        type=Path,
        default=None,
        help="Files or directories to scan (default: common config locations).",
    )
    parser.add_argument(
        "--context-lines",
        type=int,
        default=4,
        help="Lines after a match to read the wrapped command from.",
    )
    args = parser.parse_args()

    roots = args.roots or default_roots()
    findings = scan(roots, args.context_lines)

    print()
    if findings == 0:
        print("No references to acp-debug.sh found. Nothing to change.")
        return 0

    print(f"{findings} reference(s) to {WRAPPER}.")
    print("Nothing was modified. Apply the changes above yourself, then delete")
    print("the wrapper.")
    return 0


def default_roots() -> list[Path]:
    home = Path.home()
    return [
        home / ".config",
        home / "dotfiles",
        home / ".local" / "share" / "nvim",
    ]


if __name__ == "__main__":
    raise SystemExit(main())
