import builtins
for n in ("PythonFinalizationError", "_IncompleteInputError"):
    c = getattr(builtins, n, None)
    print(n, c, c and c.__mro__, c and c.__module__)
e = builtins._IncompleteInputError("x")
print(e.args, e.msg, e.lineno, repr(e))
print(builtins.PythonFinalizationError("y").args)
print(sorted(n for n in dir(builtins) if "Error" in n and n[0] == "_"))
