#!/usr/bin/env python3
"""Runs lumen-py over tests/py and reports pass/fail with the first differing line.

Usage: py-run-corpus.py [filter] [-v]
"""
import os
import subprocess
import sys

root = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "crates", "lumen-py", "tests", "py")
root = os.path.normpath(root)
binary = os.path.normpath(os.path.join(root, "..", "..", "..", "..", "target", "debug", "lumen-py"))
args = [a for a in sys.argv[1:] if not a.startswith("-")]
verbose = "-v" in sys.argv
flt = args[0] if args else ""

scripts = []
for d, dirs, files in os.walk(root):
    dirs[:] = sorted(x for x in dirs if not x.startswith("_") and x != "stdlib")
    for f in sorted(files):
        if f.endswith(".py") and not f.startswith("_"):
            scripts.append(os.path.join(d, f))

passed = 0
failed = []
for s in scripts:
    rel = os.path.relpath(s, root)
    if flt not in rel:
        continue
    exp = open(s[:-3] + ".out", "rb").read() if os.path.exists(s[:-3] + ".out") else b""
    try:
        p = subprocess.run([binary, os.path.basename(s)], cwd=os.path.dirname(s), capture_output=True, timeout=10, stdin=subprocess.DEVNULL)
        out, err, code = p.stdout, p.stderr, p.returncode
    except subprocess.TimeoutExpired:
        failed.append((rel, "TIMEOUT"))
        continue
    expl = exp.decode(errors="replace").split("\n")
    gotl = out.decode(errors="replace").split("\n")
    msg = None
    if expl != gotl:
        for i in range(max(len(expl), len(gotl))):
            e = expl[i] if i < len(expl) else "<eof>"
            g = gotl[i] if i < len(gotl) else "<eof>"
            if e != g:
                msg = "line %d: expected %r got %r" % (i + 1, e[:100], g[:100])
                break
        lasterr = err.decode(errors="replace").strip().split("\n")[-1] if err else ""
        msg += " | stderr: " + lasterr[:150]
    elif os.path.exists(s[:-3] + ".err"):
        el = open(s[:-3] + ".err").read().split("\n")
        lasterr = err.decode(errors="replace").strip().split("\n")[-1]
        if el[0].strip():
            want = int(el[0].strip()) if el[0].strip().lstrip("-").isdigit() else 1
            wl = el[1] if len(el) > 1 else ""
            if code != want:
                msg = "exit code: expected %d got %s" % (want, code)
            elif wl and wl != lasterr:
                msg = "stderr: expected %r got %r" % (wl[:100], lasterr[:100])
    if msg:
        failed.append((rel, msg))
    else:
        passed += 1

print("passed %d failed %d" % (passed, len(failed)))
for rel, msg in failed:
    print("FAIL %s: %s" % (rel, msg))
