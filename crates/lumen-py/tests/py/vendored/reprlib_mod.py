import reprlib

print(reprlib.repr(list(range(20))))
print(reprlib.repr("x" * 100))
print(reprlib.repr({i: i for i in range(10)}))
print(reprlib.repr((1, 2, 3)))
print(reprlib.repr(set(range(10))))
print(reprlib.repr(12345678901234567890123))
print(reprlib.repr([[[[[1]]]]]))

r = reprlib.Repr()
r.maxlist = 3
r.maxstring = 10
print(r.repr([1, 2, 3, 4, 5]), r.repr("abcdefghijklmnop"))


class C:
    @reprlib.recursive_repr()
    def __repr__(self):
        return "C(%r)" % (self.child,)

    child = None


c = C()
c.child = c
print(repr(c))
