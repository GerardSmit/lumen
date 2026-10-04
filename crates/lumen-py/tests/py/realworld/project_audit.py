"""`project_audit`: a command-line tool that audits a source tree.

It walks the tree, counts code by language, collects TODO/FIXME markers, summarises a request
log by hour and status, writes CSV reports, makes a filtered backup copy and runs external
commands. The demo builds its own project in a temporary directory.
"""

import collections
import csv
import functools
import io
import itertools
import os
import re
import shutil
import subprocess
import sys
import tempfile
from datetime import datetime, timedelta, timezone
from pathlib import Path

LANGUAGES = {".py": "Python", ".js": "JavaScript", ".rs": "Rust", ".md": "Markdown", ".toml": "TOML"}
MARKER_RE = re.compile(r"(?P<kind>TODO|FIXME|XXX)(?:\((?P<owner>[\w.-]+)\))?:?\s*(?P<text>.*)$")
LOG_RE = re.compile(
    r'^(?P<ip>\d{1,3}(?:\.\d{1,3}){3}) - - \[(?P<ts>[^\]]+)\] "(?P<method>[A-Z]+) (?P<path>\S+) HTTP/1\.[01]" '
    r"(?P<status>\d{3}) (?P<size>\d+|-)$"
)

PROJECT = {
    "README.md": "# demo\n\nTODO: write docs\n",
    "pyproject.toml": "[project]\nname = 'demo'\nversion = '0.3.1'\n",
    "src/demo/__init__.py": "from .core import run\n",
    "src/demo/core.py": (
        "import os\n\n\ndef run(argv):\n    # TODO(anna): validate argv\n    total = 0\n"
        "    for a in argv:\n        total += len(a)  # FIXME: unicode width\n    return total\n"
    ),
    "src/demo/util.py": "def clamp(x, lo, hi):\n    return max(lo, min(hi, x))\n\n\n# XXX(bob.k) remove after 1.0\nLEGACY = True\n",
    "web/app.js": "// TODO: debounce\nexport function main() {\n  return 42;\n}\n",
    "native/lib.rs": "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
    "build/out.js": "// generated\n",
    "node_modules/left-pad/index.js": "module.exports = 1;\n",
    "logs/access.log": "\n".join([
        '10.0.0.1 - - [12/Mar/2024:09:58:02 +0100] "GET /index.html HTTP/1.1" 200 5120',
        '10.0.0.2 - - [12/Mar/2024:09:59:40 +0100] "GET /api/items HTTP/1.1" 200 734',
        '10.0.0.1 - - [12/Mar/2024:10:01:13 +0100] "POST /api/items HTTP/1.1" 201 88',
        '10.0.0.3 - - [12/Mar/2024:10:15:00 +0100] "GET /missing HTTP/1.0" 404 -',
        'garbage line that should be skipped',
        '10.0.0.2 - - [12/Mar/2024:10:44:59 +0100] "DELETE /api/items/7 HTTP/1.1" 500 31',
        '10.0.0.4 - - [12/Mar/2024:11:00:00 +0100] "GET /index.html HTTP/1.1" 304 0',
    ]) + "\n",
}

IGNORED_DIRS = {"build", "node_modules", ".git", "__pycache__"}


@functools.total_ordering
class Marker:
    __slots__ = ("path", "line", "kind", "owner", "text")
    SEVERITY = {"FIXME": 0, "XXX": 1, "TODO": 2}

    def __init__(self, path, line, kind, owner, text):
        self.path, self.line, self.kind, self.owner, self.text = path, line, kind, owner, text

    def _key(self):
        return (self.SEVERITY[self.kind], self.path, self.line)

    def __eq__(self, other):
        return self._key() == other._key()

    def __lt__(self, other):
        return self._key() < other._key()

    def __repr__(self):
        return f"Marker({self.kind} {self.path}:{self.line})"


def walk_sources(root: Path):
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = sorted(d for d in dirnames if d not in IGNORED_DIRS)
        for name in sorted(filenames):
            path = Path(dirpath, name)
            if path.suffix in LANGUAGES:
                yield path


@functools.lru_cache(maxsize=None)
def language_of(suffix: str) -> str:
    return LANGUAGES.get(suffix, "Other")


def count_lines(path: Path) -> collections.Counter:
    stats = collections.Counter()
    comment = "#" if path.suffix in (".py", ".toml") else "//"
    for line in path.read_text().splitlines():
        stripped = line.strip()
        if not stripped:
            stats["blank"] += 1
        elif stripped.startswith(comment):
            stats["comment"] += 1
        else:
            stats["code"] += 1
    return stats


def find_markers(root: Path, path: Path):
    with path.open(encoding="utf-8") as fh:
        for lineno, line in enumerate(fh, 1):
            m = MARKER_RE.search(line)
            if m:
                yield Marker(path.relative_to(root).as_posix(), lineno, m["kind"], m["owner"], m["text"].strip())


def parse_log(lines):
    for line in lines:
        m = LOG_RE.match(line.rstrip("\n"))
        if not m:
            continue
        rec = m.groupdict()
        rec["ts"] = datetime.strptime(rec["ts"], "%d/%b/%Y:%H:%M:%S %z")
        rec["status"] = int(rec["status"])
        rec["size"] = 0 if rec["size"] == "-" else int(rec["size"])
        yield rec


def main() -> int:
    os.environ["AUDIT_USER"] = "ci-bot"
    os.environ.setdefault("AUDIT_LEVEL", "full")
    print("user:", os.environ["AUDIT_USER"], os.environ.get("AUDIT_LEVEL"), "AUDIT_MISSING" in os.environ)

    with tempfile.TemporaryDirectory(prefix="audit-") as tmp:
        root = Path(tmp) / "project"
        for rel, text in PROJECT.items():
            p = root / rel
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_text(text)
        fixed = datetime(2024, 3, 12, 8, 30, tzinfo=timezone.utc).timestamp()
        for i, p in enumerate(sorted(root.rglob("*"))):
            if p.is_file():
                os.utime(p, (fixed, fixed + i * 3600))

        print("tree:")
        for p in sorted(root.rglob("*")):
            rel = p.relative_to(root)
            print("  " * (len(rel.parts) - 1) + ("+ " if p.is_dir() else "- ") + p.name)

        per_lang = collections.defaultdict(collections.Counter)
        files = list(walk_sources(root))
        for f in files:
            per_lang[language_of(f.suffix)] += count_lines(f)
        print(f"{'language':<12}{'files':>6}{'code':>6}{'comment':>8}{'blank':>6}")
        by_lang = {k: len(list(g)) for k, g in itertools.groupby(sorted(files, key=lambda f: language_of(f.suffix)), key=lambda f: language_of(f.suffix))}
        for lang, stats in sorted(per_lang.items(), key=lambda kv: (-kv[1]["code"], kv[0])):
            print(f"{lang:<12}{by_lang[lang]:>6}{stats['code']:>6}{stats['comment']:>8}{stats['blank']:>6}")
        total = functools.reduce(lambda a, b: a + b, per_lang.values(), collections.Counter())
        print("total:", dict(sorted(total.items())), "cache:", language_of.cache_info().hits, "hits")

        markers = sorted(itertools.chain.from_iterable(find_markers(root, f) for f in files))
        print("markers:", markers)
        owners = collections.Counter(m.owner or "-" for m in markers)
        print("owners:", owners.most_common())

        newest = max(files, key=lambda f: f.stat().st_mtime)
        mtime = datetime.fromtimestamp(newest.stat().st_mtime, tz=timezone.utc)
        print("newest:", newest.relative_to(root).as_posix(), mtime.isoformat(), f"{newest.stat().st_size} bytes")

        with (root / "logs" / "access.log").open() as fh:
            records = list(parse_log(fh))
        print("requests:", len(records), "bytes:", sum(r["size"] for r in records))
        for hour, group in itertools.groupby(records, key=lambda r: r["ts"].replace(minute=0, second=0)):
            group = list(group)
            statuses = collections.Counter(r["status"] // 100 * 100 for r in group)
            print(f"  {hour:%Y-%m-%d %H:%M %Z}", len(group), dict(sorted(statuses.items())))
        span = records[-1]["ts"] - records[0]["ts"]
        print("span:", span, span // timedelta(minutes=1), "min", records[0]["ts"].astimezone(timezone.utc).strftime("%a %d %b %Y %I:%M %p"))
        errors = [r for r in records if r["status"] >= 400]
        print("errors:", [(r["method"], r["path"], r["status"]) for r in errors])

        report = root / "reports" / "markers.csv"
        report.parent.mkdir()
        with report.open("w", newline="") as fh:
            writer = csv.DictWriter(fh, fieldnames=["path", "line", "kind", "owner", "text"], quoting=csv.QUOTE_MINIMAL)
            writer.writeheader()
            for m in markers:
                writer.writerow({"path": m.path, "line": m.line, "kind": m.kind, "owner": m.owner or "", "text": m.text})
            writer.writerow({"path": "notes, misc.txt", "line": 0, "kind": "TODO", "owner": "", "text": 'say "hi"\nthen leave'})
        raw = report.read_bytes()
        print("csv bytes:", len(raw), raw.count(b"\r\n"), "CRLF")
        with report.open(newline="") as fh:
            rows = list(csv.DictReader(fh))
        print("csv rows:", len(rows), rows[-1])
        buf = io.StringIO()
        csv.writer(buf, delimiter=";", lineterminator="\n").writerows([["a;b", 1, None, 2.5], ["x", "", "y\"z", True]])
        print(buf.getvalue(), end="")
        print(list(csv.reader(["one,'two, three',four"], quotechar="'")))
        print(csv.Sniffer().sniff("a|b|c\n1|2|3\n").delimiter)

        backup = Path(tmp) / "backup"
        shutil.copytree(root, backup, ignore=shutil.ignore_patterns("node_modules", "build", "*.log"))
        copied = sorted(p.relative_to(backup).as_posix() for p in backup.rglob("*") if p.is_file())
        print("backup:", copied)
        print("mtime kept:", (backup / "README.md").stat().st_mtime == (root / "README.md").stat().st_mtime)
        shutil.move(str(backup / "reports"), str(Path(tmp) / "reports-archive"))
        shutil.rmtree(backup / "web")
        print("after move:", sorted(p.name for p in backup.iterdir()), (Path(tmp) / "reports-archive" / "markers.csv").exists())

        echo = shutil.which("echo")
        print("which echo:", echo is not None and os.access(echo, os.X_OK))
        out = subprocess.run(["echo", "audit", "done", "for", root.name], capture_output=True, text=True, check=True)
        print("echo:", repr(out.stdout), out.returncode)
        env = {**os.environ, "GREETING": "hello from env"}
        out = subprocess.run(["sh", "-c", 'echo "$GREETING; user=$AUDIT_USER"; echo oops >&2; exit 3'],
                             capture_output=True, text=True, env=env, cwd=root)
        print("sh:", out.stdout.strip(), "|", out.stderr.strip(), "| exit", out.returncode)
        try:
            subprocess.run(["sh", "-c", "exit 2"], check=True)
        except subprocess.CalledProcessError as e:
            print("CalledProcessError:", e)
        p = subprocess.Popen(["sh", "-c", "pwd"], stdout=subprocess.PIPE, cwd=root / "src")
        stdout, _ = p.communicate()
        print("pwd ends with src:", stdout.decode().strip().endswith("/project/src"), p.returncode)
        lines = subprocess.check_output(["echo", "-n", "a\nb"], text=True)
        print("check_output:", lines.splitlines())
        try:
            subprocess.run(["definitely-not-a-command-xyz"])
        except FileNotFoundError as e:
            print("FileNotFoundError:", e.errno, e.strerror)

    print("cleaned up:", not Path(tmp).exists())
    return 0


if __name__ == "__main__":
    sys.exit(main())
