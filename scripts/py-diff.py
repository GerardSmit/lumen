#!/usr/bin/env python3
"""Shows a unified diff of expected vs actual output for corpus scripts: py-diff.py dir/name [...]"""
import difflib
import os
import subprocess
import sys

root = os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "crates", "lumen-py", "tests", "py"))
binary = os.path.normpath(os.path.join(root, "..", "..", "..", "..", "target", "debug", "lumen-py"))
for name in sys.argv[1:]:
    name = name[:-3] if name.endswith(".py") else name
    s = os.path.join(root, name + ".py")
    exp = open(s[:-3] + ".out").read() if os.path.exists(s[:-3] + ".out") else ""
    p = subprocess.run([binary, os.path.basename(s)], cwd=os.path.dirname(s), capture_output=True, timeout=20, stdin=subprocess.DEVNULL)
    got = p.stdout.decode(errors="replace")
    print("=== %s (exit %d)" % (name, p.returncode))
    for l in list(difflib.unified_diff(exp.splitlines(), got.splitlines(), "expected", "got", lineterm="", n=0))[:40]:
        print(l)
    if p.stderr:
        print("stderr:", p.stderr.decode(errors="replace")[-600:])
