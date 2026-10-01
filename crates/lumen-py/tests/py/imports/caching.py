import sys
print("before", "_count_mod" in sys.modules)
import _count_mod
print("after first", "_count_mod" in sys.modules)
import _count_mod
import _count_mod as again
from _count_mod import LOADS
_count_mod.state.append(1)
print(again.state, again is _count_mod, sys.modules["_count_mod"] is _count_mod)
from _count_mod import state
state.append(2)
print(_count_mod.state)
print("_pkg.sub" in sys.modules)
import _pkg.sub
print("_pkg" in sys.modules, "_pkg.sub" in sys.modules, "_pkg.inner" in sys.modules)
print(sys.modules["_pkg.sub"] is _pkg.sub, sys.modules["_pkg"] is _pkg)
print("sys" in sys.modules, "math" in sys.modules or True, "__main__" in sys.modules)
print(sys.modules["__main__"].__name__ if False else __name__)
del sys.modules["_count_mod"]
import _count_mod as fresh
print(fresh is _count_mod, fresh.state)
print(sys.modules["_count_mod"] is fresh)
print(LOADS)
