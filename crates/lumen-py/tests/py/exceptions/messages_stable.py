d = {"a": 1}
def unpack():
    x, y = [1, 2, 3]
def unpack2():
    x, y, z = (1, 2)
def args(a, b):
    pass
cases = [
    lambda: d["zz"],
    lambda: [1, 2][5],
    lambda: int("abc"),
    lambda: "a" + 1,
    lambda: None.x,
    lambda: undefined_name,
    unpack,
    unpack2,
    lambda: args(1),
    lambda: 1 / 0,
    lambda: [].pop(),
    lambda: len(5),
    lambda: "abc".index("z"),
    lambda: [1].index(9),
    lambda: {}.pop("k"),
    lambda: (1).foo,
    lambda: {1, [2]},
    lambda: next(iter([])),
]
for fn in cases:
    try:
        fn()
    except Exception as e:
        print(type(e).__name__, e)
