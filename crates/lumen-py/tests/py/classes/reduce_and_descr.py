import copyreg
import pickle
import types


class Slotted:
    __slots__ = ("a", "b")


s = Slotted()
s.a = 1
print(s.__reduce_ex__(2)[2])


class WithArgs:
    def __init__(self, x):
        self.x = x

    def __getnewargs_ex__(self):
        return (self.x,), {}


print(WithArgs(5).__reduce_ex__(2)[:2][0] is copyreg.__newobj_ex__)

print(Ellipsis.__reduce__(), NotImplemented.__reduce__())
print(type(Ellipsis).__name__)

print(type(str.join).__name__)
print(type(str.__add__).__name__)
print(type((1).__add__).__name__)
print(type(len).__name__)
print(type(dict.__dict__["fromkeys"]).__name__)


def outer():
    x = 1

    def inner():
        return x

    return inner


closure = outer().__closure__
code = outer().__code__
exec(outer().__code__.co_consts[0] if False else compile("pass", "<s>", "exec"))

try:
    exec(compile("pass", "<s>", "exec"), {}, None, closure=closure)
except TypeError as e:
    print(e)

for bad in (list[int], dict[str, int]):
    print(bad)
try:
    str[int]
except TypeError:
    print("str not subscriptable")

try:
    compile("def f():\n  return (", "<s>", "exec")
except SyntaxError as e:
    print(e.offset is not None)
