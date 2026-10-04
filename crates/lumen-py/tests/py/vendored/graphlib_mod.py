import graphlib

ts = graphlib.TopologicalSorter({"d": {"b", "c"}, "b": {"a"}, "c": {"a"}})
print(list(ts.static_order())[0], len(list(graphlib.TopologicalSorter({"a": {"b"}}).static_order())))

ts = graphlib.TopologicalSorter()
ts.add("c", "a", "b")
ts.add("b", "a")
ts.prepare()
order = []
while ts.is_active():
    ready = sorted(ts.get_ready())
    order.append(ready)
    ts.done(*ready)
print(order)

try:
    graphlib.TopologicalSorter({"a": {"b"}, "b": {"a"}}).prepare()
except graphlib.CycleError as e:
    print("cycle", sorted(e.args[1])[:2])

ts = graphlib.TopologicalSorter({1: [2]})
try:
    ts.get_ready()
except ValueError as e:
    print("ValueError", e)
