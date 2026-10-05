# PEP 765: return/break/continue leaving a finally block is a SyntaxWarning.
import warnings

CASES = [
    "def f():\n    try:\n        pass\n    finally:\n        return 1\n",
    "for i in x:\n    try:\n        pass\n    finally:\n        break\n",
    "while x:\n    try:\n        pass\n    finally:\n        continue\n",
    # Not warned: the loop or function is inside the finally block.
    "try:\n    pass\nfinally:\n    for i in x:\n        break\n",
    "try:\n    pass\nfinally:\n    def g():\n        return 1\n",
    "for i in x:\n    try:\n        pass\n    finally:\n        for j in y:\n            continue\n",
    # Nested try statements inside the finally block still count.
    "def f():\n    try:\n        pass\n    finally:\n        if x:\n            try:\n                return\n            except E:\n                pass\n",
    # The loop's else clause is not part of the loop body.
    "for i in x:\n    pass\nelse:\n    try:\n        pass\n    finally:\n        pass\n",
    "def f():\n    for i in x:\n        pass\n    else:\n        try:\n            pass\n        finally:\n            return\n",
]

for src in CASES:
    with warnings.catch_warnings(record=True) as w:
        warnings.simplefilter("always")
        compile(src, "<case>", "exec")
    print([(str(x.message), x.category.__name__, x.lineno, x.filename) for x in w])

with warnings.catch_warnings():
    warnings.simplefilter("error")
    try:
        compile(CASES[0], "<case>", "exec")
    except SyntaxError as e:
        print(type(e).__name__, e.msg, e.lineno, e.offset, repr(e.text))

with warnings.catch_warnings():
    warnings.simplefilter("ignore")
    ns = {}
    exec(CASES[0], ns)
    print(ns["f"]())
