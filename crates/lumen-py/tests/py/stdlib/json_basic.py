import json

doc = {"name": "lumen", "tags": ["a", "b"], "nested": {"z": 1, "a": [1, 2.5, None, True, False]}, "empty": {}, "list": []}
print(json.dumps(doc))
print(json.dumps(doc, sort_keys=True))
print(json.dumps(doc, sort_keys=True, indent=2))
print(json.dumps(doc, sort_keys=True, indent="\t"))
print(json.dumps(doc, sort_keys=True, separators=(",", ":")))
print(json.dumps([1, [2, [3]]], indent=1))
print(json.dumps({"k": 1}, indent=0))
print(json.dumps("héllo \"q\" \\ \n\t☃"), json.dumps("héllo ☃", ensure_ascii=False))
print(json.dumps("\U0001F600"), json.dumps("\x00\x1f"))
print(json.dumps(None), json.dumps(True), json.dumps(1.0), json.dumps(1e100), json.dumps(-0.5), json.dumps(10 ** 25))
print(json.dumps({1: "a", 2.5: "b", True: "c", None: "d"}))
print(json.dumps((1, 2)), json.dumps([]), json.dumps({}), json.dumps(""))

text = '{"a": [1, 2, {"b": null}], "c": "\\u00e9\\n", "d": 1.5e3, "e": -7, "f": true, "big": 123456789012345678901234567890}'
obj = json.loads(text)
print(obj, type(obj["d"]).__name__, type(obj["e"]).__name__, obj["big"] * 2)
print(json.loads("  [1 , 2,3 ]  "), json.loads('"x"'), json.loads("3"), json.loads("null"), json.loads("true"))
print(json.loads('"\\ud83d\\ude00"') == "\U0001F600", json.loads("1E2"), json.loads("-0"), json.loads("0.1"))
print(json.loads('{"a": 1, "a": 2}'), json.loads("[]"), json.loads("{}"), json.loads('{"k": {"k": {"k": []}}}'))

round_trip = json.loads(json.dumps(doc))
print(round_trip == doc, json.loads(json.dumps(doc, indent=4)) == doc)
nums = [0, -1, 1.5, 1e-7, 3.14159, 2 ** 70, 1e22]
print(json.dumps(nums), json.loads(json.dumps(nums)) == nums)

for bad in ["", "{", "[1,]", "{'a': 1}", '{"a" 1}', "nul", "[1 2]", '{"a": }', "01", '"unterminated', "[1] x", "NaN1"]:
    try:
        json.loads(bad)
        print("accepted", repr(bad))
    except json.JSONDecodeError as e:
        print("JSONDecodeError", repr(bad), e.pos, isinstance(e, ValueError))

for unser in [{1, 2}, object(), b"bytes", {"k": {1}}]:
    try:
        json.dumps(unser)
    except TypeError:
        print("TypeError unserializable")
print(json.dumps({"s": {1, 2}}, default=sorted), json.dumps(b"ab", default=lambda o: o.decode()))
try:
    json.dumps(float("nan"), allow_nan=False)
except ValueError:
    print("ValueError nan")
print(json.dumps(float("nan")), json.dumps(float("inf")), json.loads("NaN") != json.loads("NaN"), json.loads("Infinity"))
circ = []
circ.append(circ)
try:
    json.dumps(circ)
except ValueError:
    print("ValueError circular")
print(json.loads('{"b": 1, "a": 2}', object_pairs_hook=lambda pairs: [k for k, _ in pairs]))
print(json.loads("[1, 2]", parse_int=lambda s: int(s) * 10), json.loads("1.5", parse_float=lambda s: s + "!"))
print(list(json.loads('{"z": 1, "y": 2, "x": 3}')), list(json.loads('{"z": 1, "y": 2}').items()))
