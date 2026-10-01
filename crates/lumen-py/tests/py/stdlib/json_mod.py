import json

data = {"name": "x", "n": [1, 2.5, None, True], "nested": {"b": 1, "a": [], "c": {}}, "s": "é\n\"q\""}
print(json.dumps(data))
print(json.dumps(data, sort_keys=True))
print(json.dumps(data, sort_keys=True, indent=2))
print(json.dumps([1, "a"], separators=(",", ":")), json.dumps("é", ensure_ascii=False), json.dumps("é"))
print(json.dumps(None), json.dumps(True), json.dumps(1.0), json.dumps(10 ** 20), json.dumps({1: "a", 2: "b"}))
back = json.loads('{"a": [1, 2, {"b": null}], "c": "d\\u00e9", "e": 1.5e2, "f": false, "g": -0}')
print(back, back["a"][2]["b"] is None, type(back["e"]).__name__, type(back["g"]).__name__)
print(json.loads("[]"), json.loads("123"), json.loads('"s"'), json.loads(" true "), json.loads("{}"))
print(json.loads(json.dumps(data)) == data)
for bad in ["{", "[1,]", "nope", ""]:
    try:
        json.loads(bad)
    except json.JSONDecodeError:
        print("JSONDecodeError", repr(bad))
try:
    json.dumps({"s": {1, 2}})
except TypeError as e:
    print("TypeError")
print(json.dumps({"b": 1, "a": 2}, indent=None), json.dumps([], indent=2), json.dumps({}, indent=2))
print(json.dumps(float("inf")), json.dumps([1, [2, [3]]], indent=1))
