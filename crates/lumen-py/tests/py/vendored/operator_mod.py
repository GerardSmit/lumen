import operator as op

print(op.add(1, 2), op.sub(5, 3), op.mul(3, 4), op.truediv(7, 2), op.floordiv(7, 2), op.mod(7, 3))
print(op.pow(2, 10), op.neg(3), op.pos(-3), op.abs(-4), op.invert(5))
print(op.lt(1, 2), op.le(2, 2), op.eq(1, 1), op.ne(1, 2), op.gt(3, 2), op.ge(1, 2))
print(op.and_(6, 3), op.or_(6, 3), op.xor(6, 3), op.lshift(1, 4), op.rshift(16, 2))
print(op.not_(0), op.truth([]), op.is_(None, None), op.is_not(1, None))
print(op.concat([1], [2]), op.contains([1, 2], 2), op.countOf([1, 1, 2], 1), op.indexOf([5, 6], 6))
lst = [1, 2, 3]
op.setitem(lst, 0, 9)
op.delitem(lst, 1)
print(lst, op.getitem(lst, -1))
print(op.index(7), op.length_hint([1, 2]))

g = op.itemgetter(1)
print(g("abc"), op.itemgetter(0, 2)("abc"), op.itemgetter("a")({"a": 1}))
class Obj:
    x = 1
    class inner:
        y = 2
print(op.attrgetter("x")(Obj), op.attrgetter("inner.y")(Obj), op.attrgetter("x", "inner")(Obj)[0])
class Calc:
    def double(self, v):
        return v * 2
print(op.methodcaller("double", 4)(Calc()), op.methodcaller("upper")("ab"))
print(sorted([("b", 2), ("a", 3)], key=op.itemgetter(1)))
x = [1]
x = op.iadd(x, [2])
print(x, op.imul(x, 2))
print(op.matmul.__name__, op.__add__(1, 2))
