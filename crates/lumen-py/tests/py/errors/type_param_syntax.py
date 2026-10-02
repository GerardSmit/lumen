cases = [
    "def f[**A, A](): ...",
    "class C[A, *A](): ...",
    "type T[*A, **A] = None",
    "def f[*A: str](): pass",
    "def f[*A: (int, str)](): pass",
    "class X[**A: str]: pass",
    "type X[**A: (int, str)] = int",
    "def outer():\n    X = 1\n    def inner[X]():\n        nonlocal X\n    return X",
    "def outer2[T]():\n    def inner1():\n        nonlocal T",
    "class Cls[T]:\n    nonlocal T",
    "class C[T]:\n    class Inner[U](make_base(T for _ in (1,))): pass",
    "class C[T]:\n    def meth[U](x: (T for _ in (1,)), y: T): pass",
    "class C[T]:\n    type A = lambda: T",
    "class C[T]:\n    type A[U] = [T for _ in (1,)]",
    "type X = (yield)",
    "class X[T: (await 42)]: pass",
    "def f[T](y: (x := int)): pass",
]
for c in cases:
    try:
        compile(c, "<t>", "exec")
        print("ok")
    except SyntaxError as e:
        print(e.msg)
