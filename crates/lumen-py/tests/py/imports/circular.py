print("main: importing _circ_a")
import _circ_a
print("main: done importing")
import _circ_b
print(_circ_a.A_NAME, _circ_b.NAME)
print(_circ_a.a_func())
print(_circ_b.b_func())
import sys
print(sorted(k for k in sys.modules if k.startswith("_circ")))
