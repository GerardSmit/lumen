import __future__
import copyreg
import keyword
import numbers
import collections.abc as cabc

print(__future__.annotations.getMandatoryRelease(), __future__.all_feature_names[:3])
print(keyword.iskeyword("class"), keyword.iskeyword("foo"), keyword.issoftkeyword("match"))
print(len(keyword.kwlist), "lambda" in keyword.kwlist)
print(isinstance(1, numbers.Integral), isinstance(1.5, numbers.Real), isinstance(1.5, numbers.Integral))
print(isinstance(1j, numbers.Complex), isinstance(True, numbers.Number))
print(issubclass(bool, numbers.Integral), numbers.Rational.__mro__[1].__name__)
print(isinstance([], cabc.Sequence), isinstance({}, cabc.Mapping), isinstance(set(), cabc.Set))
print(isinstance("a", cabc.Iterable), isinstance(len, cabc.Callable), isinstance(1, cabc.Hashable))
print(isinstance(iter([]), cabc.Iterator), isinstance((x for x in ()), cabc.Generator))


class MyMap(cabc.Mapping):
    def __init__(self, d):
        self._d = d

    def __getitem__(self, k):
        return self._d[k]

    def __iter__(self):
        return iter(self._d)

    def __len__(self):
        return len(self._d)


m = MyMap({"a": 1, "b": 2})
print(list(m), m.get("a"), m.get("z", 0), "a" in m, sorted(m.items()), m == {"a": 1, "b": 2})


class MyList(cabc.MutableSequence):
    def __init__(self):
        self._l = []

    def __getitem__(self, i):
        return self._l[i]

    def __setitem__(self, i, v):
        self._l[i] = v

    def __delitem__(self, i):
        del self._l[i]

    def __len__(self):
        return len(self._l)

    def insert(self, i, v):
        self._l.insert(i, v)


ml = MyList()
ml.append(1)
ml.extend([2, 3])
print(list(ml), ml.pop(), ml.index(2), 2 in ml, list(reversed(ml)))
print(copyreg.__name__, callable(copyreg.pickle))
