print("b: start")
import _circ_a
print("b: has A_NAME yet?", hasattr(_circ_a, "A_NAME"))
NAME = "from-b"
def b_func():
    return "b_func sees " + _circ_a.A_NAME
print("b: end")
