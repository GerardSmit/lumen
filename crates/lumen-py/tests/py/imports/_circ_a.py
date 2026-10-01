print("a: start")
import _circ_b
print("a: b.NAME =", _circ_b.NAME)
A_NAME = "from-a"
def a_func():
    return "a_func sees " + _circ_b.NAME
print("a: end")
