#!/usr/bin/env python3
"""Convert acp-debug.sh log files into the adapter's NDJSON log layout.

The wrapper wrote one pair of freeform files per process, named by timestamp
and PID because it could not know anything better at exec time. The adapter and
`acp-proxy` write structured records keyed by session. This script converts the
old files to the new shape so history stays readable after the wrapper is gone.

Dry-run by default. Nothing is written or removed without --apply.

What it cannot recover, and says so in the output:

  * Per-record timestamps. The wrapper stamped the filename, not the lines, so
    every record from a file inherits that one time.
  * Interleaving between stdout and stderr. They were separate files with no
    shared ordering, so protocol frames are written before diagnostics.
  * Client-to-agent traffic. The wrapper only teed stdout, so half of every
    old conversation was never recorded and cannot be reconstructed.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import sys
from collections import defaultdict
from dataclasses import dataclass, field
from pathlib import Path

# <date>-<time>-<pid>-<binary>[-<label>]-<stream>.log
LEGACY_NAME = re.compile(
    r"^(?P<date>\d{8})-(?P<time>\d{6})-(?P<pid>\d+)-"
    r"(?P<program>.+)-(?P<stream>stderr|stdout-jsonrpc)\.log$"
)

# Programs that are this project's own adapter; everything else was a foreign
# agent and belongs under the proxy root, exactly as it would today.
ADAPTER_PREFIX = "acp-llm-adapter"

PROXY_DIR = "proxy"
SESSIONS_DIR = "sessions"
CONNECTIONS_DIR = "connections"
LEGACY_ARCHIVE_DIR = "legacy"
SESSION_LOG = "log.jsonl"

VALID_ID = re.compile(r"^[A-Za-z0-9_-]+$")

# The wrapper wrote the invocation into the first lines of its stderr file, so
# a version poll is identifiable exactly rather than guessed at by size.
ARGV_HEADER = re.compile(r"^argv:\s*(?P<argv>.*)$")
ARGV_HEADER_SCAN_LINES = 5


@dataclass
class Connection:
    """One wrapper invocation: a stdout file, a stderr file, or both."""

    key: str
    program: str
    timestamp: str
    files: dict[str, Path] = field(default_factory=dict)

    @property
    def is_adapter(self) -> bool:
        return self.program.startswith(ADAPTER_PREFIX)

    @property
    def total_bytes(self) -> int:
        return sum(path.stat().st_size for path in self.files.values())

    @property
    def argv(self) -> str | None:
        """The invocation the wrapper recorded in its stderr header."""
        stderr = self.files.get("stderr")
        if stderr is None:
            return None
        try:
            with stderr.open("r", encoding="utf-8", errors="replace") as handle:
                for _ in range(ARGV_HEADER_SCAN_LINES):
                    line = handle.readline()
                    if not line:
                        break
                    match = ARGV_HEADER.match(line.strip())
                    if match:
                        return match["argv"]
        except OSError:
            return None
        return None

    @property
    def looks_like_version_poll(self) -> bool:
        """True for `--version` invocations, which hold no session at all.

        These dominate the legacy directory: a version poll writes a header and
        one line of output, so converting them would move the old clutter into
        the new layout instead of leaving it behind.
        """
        argv = self.argv
        return argv is not None and "--version" in argv.split()


def parse_timestamp(date: str, time: str) -> str:
    """Rebuild an ISO 8601 stamp from the wrapper's filename fields."""
    return (
        f"{date[0:4]}-{date[4:6]}-{date[6:8]}"
        f"T{time[0:2]}:{time[2:4]}:{time[4:6]}.000Z"
    )


def scan(state_dir: Path) -> tuple[dict[str, Connection], list[Path]]:
    """Group legacy files into connections, returning unparseable ones too."""
    connections: dict[str, Connection] = {}
    unparsed: list[Path] = []

    for path in sorted(state_dir.iterdir()):
        if not path.is_file():
            continue
        match = LEGACY_NAME.match(path.name)
        if not match:
            unparsed.append(path)
            continue

        key = f"{match['date']}-{match['time']}-{match['pid']}"
        connection = connections.setdefault(
            key,
            Connection(
                key=key,
                program=match["program"],
                timestamp=parse_timestamp(match["date"], match["time"]),
            ),
        )
        connection.files[match["stream"]] = path

    return connections, unparsed


def session_id_in(frame: object) -> str | None:
    """Find a session id a frame names, if any.

    The live sniffer correlates a session/new response with the request that
    provoked it. That is impossible here: the wrapper never recorded the
    client-to-agent direction, so there are no requests to correlate against.
    Any result or params carrying a sessionId is taken at face value instead.
    """
    if not isinstance(frame, dict):
        return None
    for holder in ("result", "params"):
        section = frame.get(holder)
        if isinstance(section, dict):
            candidate = section.get("sessionId")
            if isinstance(candidate, str) and VALID_ID.match(candidate):
                return candidate
    return None


def record(timestamp: str, direction: str, kind: str, payload: object,
           session_id: str | None = None) -> dict:
    entry = {"timestamp": timestamp, "direction": direction, "kind": kind}
    if session_id is not None:
        entry["session_id"] = session_id
    entry["payload"] = payload
    return entry


def convert(connection: Connection, root: Path, apply: bool) -> tuple[int, set[str]]:
    """Convert one connection, returning records written and sessions found."""
    connection_path = root / CONNECTIONS_DIR / f"{connection.key}.jsonl"
    buffers: dict[Path, list[dict]] = defaultdict(list)
    sessions: set[str] = set()
    bound: str | None = None

    buffers[connection_path].append(
        record(
            connection.timestamp,
            "internal",
            "migrated",
            {
                "source": sorted(path.name for path in connection.files.values()),
                "program": connection.program,
                "note": (
                    "Converted from acp-debug.sh output. Timestamps are the "
                    "process start time, not per-record; stdout and stderr "
                    "were separate files so their interleaving is lost; the "
                    "client-to-agent direction was never recorded."
                ),
            },
        )
    )

    # Protocol frames first: they are what establishes the session routing.
    stdout = connection.files.get("stdout-jsonrpc")
    if stdout is not None:
        for line in read_lines(stdout):
            try:
                payload = json.loads(line)
            except json.JSONDecodeError:
                payload = line

            found = session_id_in(payload)
            if found is not None and bound is None:
                bound = found
                sessions.add(found)
                buffers[connection_path].append(
                    record(connection.timestamp, "internal", "session-bound",
                           found, session_id=found)
                )

            destination = (
                root / SESSIONS_DIR / bound / SESSION_LOG
                if bound is not None
                else connection_path
            )
            buffers[destination].append(
                record(connection.timestamp, "agent_to_client", "frame",
                       payload, session_id=bound)
            )

    stderr = connection.files.get("stderr")
    if stderr is not None:
        destination = (
            root / SESSIONS_DIR / bound / SESSION_LOG
            if bound is not None
            else connection_path
        )
        for line in read_lines(stderr):
            buffers[destination].append(
                record(connection.timestamp, "internal", "stderr", line,
                       session_id=bound)
            )

    written = sum(len(entries) for entries in buffers.values())
    if apply:
        for path, entries in buffers.items():
            path.parent.mkdir(parents=True, exist_ok=True)
            with path.open("a", encoding="utf-8") as handle:
                for entry in entries:
                    handle.write(json.dumps(entry, ensure_ascii=False) + "\n")

    return written, sessions


def read_lines(path: Path):
    with path.open("r", encoding="utf-8", errors="replace") as handle:
        for line in handle:
            line = line.rstrip("\n")
            if line.strip():
                yield line


def human(size: int) -> str:
    for unit in ("B", "KB", "MB", "GB"):
        if size < 1024 or unit == "GB":
            return f"{size:.0f}{unit}" if unit == "B" else f"{size / 1:.0f}{unit}"
        size /= 1024
    return f"{size:.0f}GB"


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Convert acp-debug.sh logs into the adapter's log layout."
    )
    parser.add_argument(
        "--state-dir",
        type=Path,
        default=None,
        help="State directory holding the legacy files (default: XDG state).",
    )
    parser.add_argument(
        "--apply",
        action="store_true",
        help="Write the converted records. Without this, nothing is written.",
    )
    parser.add_argument(
        "--archive",
        action="store_true",
        help="Move converted legacy files into a legacy/ subdirectory.",
    )
    parser.add_argument(
        "--keep-version-polls",
        action="store_true",
        help="Convert tiny --version invocations too (skipped by default).",
    )
    args = parser.parse_args()

    state_dir = args.state_dir or default_state_dir()
    if state_dir is None:
        print("error: set XDG_STATE_HOME or HOME, or pass --state-dir",
              file=sys.stderr)
        return 2
    if not state_dir.is_dir():
        print(f"error: {state_dir} is not a directory", file=sys.stderr)
        return 2

    connections, unparsed = scan(state_dir)
    if not connections:
        print(f"No legacy acp-debug.sh files found in {state_dir}")
        return 0

    proxy_root = state_dir / PROXY_DIR
    converted = skipped = records = 0
    sessions: set[str] = set()
    source_bytes = 0

    for connection in connections.values():
        if connection.looks_like_version_poll and not args.keep_version_polls:
            skipped += 1
            continue
        root = state_dir if connection.is_adapter else proxy_root
        count, found = convert(connection, root, args.apply)
        records += count
        sessions |= found
        converted += 1
        source_bytes += connection.total_bytes

        if args.archive and args.apply:
            archive = state_dir / LEGACY_ARCHIVE_DIR
            archive.mkdir(parents=True, exist_ok=True)
            for path in connection.files.values():
                shutil.move(str(path), str(archive / path.name))

    mode = "converted" if args.apply else "would convert"
    print(f"State directory : {state_dir}")
    print(f"Connections     : {len(connections)} found")
    print(f"  {mode:<14}: {converted}")
    print(f"  skipped       : {skipped} (version polls; --keep-version-polls to include)")
    if unparsed:
        print(f"  unrecognised  : {len(unparsed)} (left untouched)")
        for path in unparsed[:5]:
            print(f"      {path.name}")
    print(f"Records         : {records}")
    print(f"Sessions found  : {len(sessions)}")
    print(f"Source size     : {human(source_bytes)}")
    print(f"Adapter logs -> : {state_dir}/{{{CONNECTIONS_DIR},{SESSIONS_DIR}}}")
    print(f"Proxied logs -> : {proxy_root}/{{{CONNECTIONS_DIR},{SESSIONS_DIR}}}")

    if not args.apply:
        print()
        print("Dry run. Re-run with --apply to write, and --archive to move")
        print("the legacy files aside once you are satisfied.")

    return 0


def default_state_dir() -> Path | None:
    xdg = os.environ.get("XDG_STATE_HOME")
    if xdg:
        return Path(xdg) / "acp-llm-adapter"
    home = os.environ.get("HOME")
    if home:
        return Path(home) / ".local" / "state" / "acp-llm-adapter"
    return None


if __name__ == "__main__":
    raise SystemExit(main())
