def load():
    try:
        return {}["missing"]
    except KeyError as e:
        raise RuntimeError("load failed") from e


print("before")
load()
print("unreachable")
