def documented(a: int, b: "str" = "x") -> float:
    """Docstring here."""
    return a
print(documented.__name__, documented.__doc__)
print(documented(3, 4))
def nodoc():
    pass
print(nodoc.__doc__, nodoc.__name__, nodoc.__qualname__)
def outer():
    def inner():
        pass
    return inner
print(outer().__name__, outer().__qualname__)
print(documented.__module__ == __name__, __name__)
documented.extra = 5
print(documented.extra, documented.__dict__)
print(callable(documented), callable(3))

def show(*a, **k):
    return len(a), sorted(k)
print(show(*"abc", *[1, 2], **{"x": 1}, y=2))
print(show(*range(4)))
first, *mid, last = range(6)
print(first, mid, last)
a, (b, *c), d = 1, (2, 3, 4), 5
print(a, b, c, d)
print([*range(3), *"ab"], {**{"a": 1}, **{"b": 2, "a": 3}})
print((*[1], *(2,)))
def ret_multi():
    return 1, 2, 3
print(ret_multi(), type(ret_multi()).__name__)
a, b = 1, 2
a, b = b, a
print(a, b)
def noreturn():
    return
print(noreturn())
v = f = lambda: 3
print(v(), f is v)
print(print.__name__, len.__name__, type(documented).__name__)
def many(a, b, c, d, e):
    return a + b + c + d + e
print(many(*[1, 2], *[3, 4], 5))
print(many(1, *[2, 3], e=5, d=4))
