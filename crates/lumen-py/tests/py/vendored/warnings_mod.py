import warnings

with warnings.catch_warnings(record=True) as w:
    warnings.simplefilter("always")
    warnings.warn("one")
    warnings.warn("two", DeprecationWarning)
    warnings.warn(UserWarning("three"))
    print([(x.category.__name__, str(x.message)) for x in w])

with warnings.catch_warnings(record=True) as w:
    warnings.simplefilter("default")
    for _ in range(3):
        warnings.warn("same")
    print(len(w))

with warnings.catch_warnings(record=True) as w:
    warnings.simplefilter("ignore")
    warnings.warn("hidden")
    print(len(w))

with warnings.catch_warnings():
    warnings.simplefilter("error")
    try:
        warnings.warn("boom", RuntimeWarning)
    except RuntimeWarning as e:
        print("raised", e)

with warnings.catch_warnings(record=True) as w:
    warnings.simplefilter("always")
    warnings.warn_explicit("exp", UserWarning, "file.py", 10)
    print(w[0].filename, w[0].lineno)


def old():
    warnings.warn("old is deprecated", DeprecationWarning, stacklevel=2)


with warnings.catch_warnings(record=True) as w:
    warnings.simplefilter("always")
    old()
    print(w[0].category.__name__, w[0].lineno > 0)

print(warnings.formatwarning("m", UserWarning, "f.py", 3, "src").splitlines()[0])
print(len(warnings.filters) > 0)
