"""A Hermes-agent style tool plugin and the host side that loads it.

The plugin ships a manifest and a `register(ctx)` entry point; tools are plain functions whose
JSON schemas are derived from their signatures and docstrings. The host parses Hermes-format
`<tool_call>` blocks out of model output, validates the arguments and dispatches them.
"""

from __future__ import annotations

import inspect
import json
import logging
import re
import sys
import tempfile
from dataclasses import asdict, dataclass, field
from enum import Enum
from pathlib import Path, PurePosixPath
from typing import Any, Callable, Dict, List, Literal, Optional, get_args, get_origin, get_type_hints

logger = logging.getLogger("hermes.plugins")
_handler = logging.StreamHandler(sys.stdout)
_handler.setFormatter(logging.Formatter("%(levelname)-7s %(name)s: %(message)s"))
logger.addHandler(_handler)
logger.setLevel(logging.DEBUG)
logger.propagate = False


class ToolError(Exception):
    """Raised when a tool call cannot be executed."""


class Units(str, Enum):
    METRIC = "metric"
    IMPERIAL = "imperial"


@dataclass
class ToolSpec:
    name: str
    toolset: str
    description: str
    parameters: Dict[str, Any]
    handler: Callable[..., Any] = field(repr=False, compare=False)
    requires_env: List[str] = field(default_factory=list)

    def to_openai(self) -> Dict[str, Any]:
        return {
            "type": "function",
            "function": {"name": self.name, "description": self.description, "parameters": self.parameters},
        }


@dataclass(frozen=True)
class ToolCall:
    name: str
    arguments: Dict[str, Any]
    call_id: str


@dataclass
class ToolResult:
    call_id: str
    name: str
    content: Any
    error: Optional[str] = None

    @property
    def ok(self) -> bool:
        return self.error is None


_JSON_TYPES = {str: "string", int: "integer", float: "number", bool: "boolean", dict: "object"}
_ARG_RE = re.compile(r"^\s{4}(\w+)(?:\s*\(([^)]*)\))?:\s*(.+)$")


def parse_docstring(doc: str) -> tuple[str, Dict[str, str]]:
    """Splits a Google-style docstring into its summary and per-argument descriptions."""
    doc = inspect.cleandoc(doc or "")
    summary, _, rest = doc.partition("\n\n")
    args: Dict[str, str] = {}
    in_args = False
    for line in rest.splitlines():
        if line.strip() in ("Args:", "Arguments:"):
            in_args = True
            continue
        if in_args:
            m = _ARG_RE.match(line)
            if m:
                args[m.group(1)] = m.group(3).strip()
            elif line and not line.startswith(" "):
                in_args = False
    return " ".join(summary.split()), args


def json_schema_for(tp: Any) -> Dict[str, Any]:
    origin = get_origin(tp)
    if origin is Literal:
        values = list(get_args(tp))
        return {"type": _JSON_TYPES[type(values[0])], "enum": values}
    if origin in (list, List):
        (item,) = get_args(tp) or (Any,)
        return {"type": "array", "items": json_schema_for(item)}
    if origin in (dict, Dict):
        return {"type": "object"}
    if origin is not None and type(None) in get_args(tp):
        inner = [a for a in get_args(tp) if a is not type(None)]
        return json_schema_for(inner[0])
    if isinstance(tp, type) and issubclass(tp, Enum):
        return {"type": "string", "enum": [m.value for m in tp]}
    if tp in _JSON_TYPES:
        return {"type": _JSON_TYPES[tp]}
    return {}


def schema_from_function(fn: Callable[..., Any]) -> tuple[str, Dict[str, Any]]:
    hints = get_type_hints(fn)
    summary, arg_docs = parse_docstring(fn.__doc__)
    props: Dict[str, Any] = {}
    required: List[str] = []
    for name, param in inspect.signature(fn).parameters.items():
        prop = json_schema_for(hints.get(name, Any))
        if name in arg_docs:
            prop["description"] = arg_docs[name]
        if param.default is inspect.Parameter.empty:
            required.append(name)
        else:
            default = param.default
            prop["default"] = default.value if isinstance(default, Enum) else default
        props[name] = prop
    return summary, {"type": "object", "properties": props, "required": required}


class PluginContext:
    """What a plugin's `register(ctx)` sees: it can add tools and lifecycle hooks."""

    def __init__(self, registry: "ToolRegistry", plugin: str):
        self._registry = registry
        self.plugin = plugin

    def register_tool(self, fn: Callable[..., Any], *, toolset: str, name: Optional[str] = None,
                      requires_env: Optional[List[str]] = None) -> ToolSpec:
        description, params = schema_from_function(fn)
        spec = ToolSpec(name or fn.__name__, toolset, description, params, fn, list(requires_env or []))
        self._registry.add(spec)
        return spec

    def register_hook(self, event: str, fn: Callable[..., Any]) -> None:
        self._registry.hooks.setdefault(event, []).append((self.plugin, fn))


class ToolRegistry:
    def __init__(self, env: Dict[str, str]):
        self.tools: Dict[str, ToolSpec] = {}
        self.hooks: Dict[str, List[tuple[str, Callable[..., Any]]]] = {}
        self.env = env

    def add(self, spec: ToolSpec) -> None:
        if spec.name in self.tools:
            raise ToolError(f"tool {spec.name!r} is already registered")
        self.tools[spec.name] = spec
        logger.debug("registered %s (toolset=%s, %d params)", spec.name, spec.toolset,
                     len(spec.parameters["properties"]))

    def available(self) -> List[ToolSpec]:
        return [t for t in self.tools.values() if all(k in self.env for k in t.requires_env)]

    def emit(self, event: str, **payload: Any) -> None:
        for plugin, fn in self.hooks.get(event, []):
            fn(**payload)

    def validate(self, spec: ToolSpec, args: Dict[str, Any]) -> Dict[str, Any]:
        props = spec.parameters["properties"]
        missing = [r for r in spec.parameters["required"] if r not in args]
        if missing:
            raise ToolError(f"{spec.name}: missing required argument(s): {', '.join(missing)}")
        unknown = sorted(set(args) - set(props))
        if unknown:
            raise ToolError(f"{spec.name}: unexpected argument(s): {', '.join(unknown)}")
        out = {}
        for key, prop in props.items():
            if key not in args:
                continue
            value = args[key]
            expected = prop.get("type")
            checks = {"string": str, "integer": int, "number": (int, float), "boolean": bool, "array": list}
            if expected in checks and not isinstance(value, checks[expected]):
                raise ToolError(f"{spec.name}: {key} must be {expected}, got {type(value).__name__}")
            if "enum" in prop and value not in prop["enum"]:
                raise ToolError(f"{spec.name}: {key} must be one of {prop['enum']}")
            out[key] = value
        return out

    def dispatch(self, call: ToolCall) -> ToolResult:
        spec = self.tools.get(call.name)
        if spec is None:
            return ToolResult(call.call_id, call.name, None, f"unknown tool {call.name!r}")
        try:
            args = self.validate(spec, call.arguments)
            self.emit("pre_tool_call", name=call.name, args=args)
            content = spec.handler(**args)
        except ToolError as e:
            logger.warning("rejected %s: %s", call.call_id, e)
            return ToolResult(call.call_id, call.name, None, str(e))
        except Exception as e:  # a plugin bug must not take the agent down
            logger.error("tool %s raised %s: %s", call.name, type(e).__name__, e)
            return ToolResult(call.call_id, call.name, None, f"{type(e).__name__}: {e}")
        self.emit("post_tool_call", name=call.name, result=content)
        return ToolResult(call.call_id, call.name, content)


# ---- the plugin --------------------------------------------------------------------------------

_FORECASTS = {
    "amsterdam": [(12.5, "rain"), (14.0, "cloudy"), (16.25, "sunny")],
    "lisbon": [(21.0, "sunny"), (22.5, "sunny"), (19.75, "windy")],
}


def get_forecast(city: str, days: int = 1, units: Units = Units.METRIC) -> Dict[str, Any]:
    """Returns the weather forecast for a city.

    Args:
        city (str): City name, case-insensitive.
        days (int): Number of days to forecast, 1-3.
        units (Units): Temperature units.
    """
    key = city.strip().lower()
    if key not in _FORECASTS:
        raise KeyError(city)
    if not 1 <= days <= 3:
        raise ToolError(f"days must be between 1 and 3, got {days}")
    rows = []
    for temp, sky in _FORECASTS[key][:days]:
        if units == Units.IMPERIAL.value:
            temp = round(temp * 9 / 5 + 32, 1)
        rows.append({"temp": temp, "sky": sky})
    return {"city": city.title(), "units": units, "days": rows}


def search_notes(query: str, tags: Optional[List[str]] = None,
                 mode: Literal["any", "all"] = "any") -> List[str]:
    """Searches the user's notes.

    Args:
        query: A regular expression matched against note titles.
        tags: Only return notes with these tags.
        mode: Whether a note needs any or all of the tags.
    """
    notes = {
        "Groceries for the week": {"home"},
        "Quarterly planning": {"work", "planning"},
        "Weekend hiking plan": {"home", "planning"},
    }
    pattern = re.compile(query, re.IGNORECASE)
    wanted = set(tags or [])
    hits = []
    for title, note_tags in notes.items():
        if not pattern.search(title):
            continue
        if wanted and not (wanted <= note_tags if mode == "all" else wanted & note_tags):
            continue
        hits.append(title)
    return sorted(hits)


def send_email(to: str, subject: str, body: str = "") -> str:
    """Sends an email (needs SMTP credentials)."""
    return f"sent to {to}"


def register(ctx: PluginContext) -> None:
    ctx.register_tool(get_forecast, toolset="weather")
    ctx.register_tool(search_notes, toolset="notes")
    ctx.register_tool(send_email, toolset="email", requires_env=["SMTP_HOST"])
    calls: List[str] = []
    ctx.register_hook("pre_tool_call", lambda name, args: calls.append(name))
    ctx.register_hook("post_tool_call",
                      lambda name, result: logger.info("%s -> %s", name, type(result).__name__))
    PLUGIN_STATE["calls"] = calls


PLUGIN_STATE: Dict[str, Any] = {}


# ---- the host ----------------------------------------------------------------------------------

@dataclass
class Manifest:
    name: str
    version: str
    entry: str
    description: str = ""
    provides_tools: List[str] = field(default_factory=list)

    @classmethod
    def load(cls, path: Path) -> "Manifest":
        data = json.loads(path.read_text(encoding="utf-8"))
        known = {f for f in cls.__dataclass_fields__}
        extra = sorted(set(data) - known)
        if extra:
            logger.warning("%s: ignoring unknown manifest keys %s", path.name, extra)
        return cls(**{k: v for k, v in data.items() if k in known})


TOOL_CALL_RE = re.compile(r"<tool_call>\s*(.*?)\s*</tool_call>", re.DOTALL)


def parse_tool_calls(text: str) -> tuple[str, List[ToolCall]]:
    calls = []
    for i, m in enumerate(TOOL_CALL_RE.finditer(text)):
        try:
            payload = json.loads(m.group(1))
        except json.JSONDecodeError as e:
            logger.error("malformed tool call #%d: %s (line %d col %d)", i, e.msg, e.lineno, e.colno)
            continue
        calls.append(ToolCall(payload["name"], payload.get("arguments", {}), f"call_{i}"))
    return TOOL_CALL_RE.sub("", text).strip(), calls


def format_tool_response(result: ToolResult) -> str:
    body = {"name": result.name, "content": result.content} if result.ok else {"name": result.name, "error": result.error}
    return "<tool_response>\n" + json.dumps(body, sort_keys=True) + "\n</tool_response>"


def main() -> None:
    with tempfile.TemporaryDirectory(prefix="hermes-") as tmp:
        plugin_dir = Path(tmp) / "plugins" / "weather-tools"
        plugin_dir.mkdir(parents=True)
        manifest_path = plugin_dir / "plugin.json"
        manifest_path.write_text(json.dumps({
            "name": "weather-tools",
            "version": "1.2.0",
            "entry": "weather_tools:register",
            "description": "Weather and notes tools",
            "provides_tools": ["get_forecast", "search_notes", "send_email"],
            "homepage": "https://example.invalid",
        }, indent=2), encoding="utf-8")

        found = sorted(p.relative_to(tmp).as_posix() for p in Path(tmp).rglob("plugin.json"))
        print("discovered:", found)
        manifest = Manifest.load(manifest_path)
        print(manifest)
        module, _, func = manifest.entry.partition(":")
        print("entry:", PurePosixPath(module.replace(".", "/")).with_suffix(".py"), func)

    registry = ToolRegistry(env={"HOME": "/home/agent"})
    register(PluginContext(registry, manifest.name))
    print("missing tools:", sorted(set(manifest.provides_tools) - set(registry.tools)))
    print("available:", [t.name for t in registry.available()])
    print(json.dumps([t.to_openai() for t in registry.available()], indent=2))
    print(registry.tools["get_forecast"])

    model_output = """I'll check the weather and your notes.
<tool_call>
{"name": "get_forecast", "arguments": {"city": "Amsterdam", "days": 2}}
</tool_call>
<tool_call>
{"name": "get_forecast", "arguments": {"city": "lisbon", "units": "imperial", "days": 3}}
</tool_call>
<tool_call>
{"name": "search_notes", "arguments": {"query": "plan", "tags": ["home"]}}
</tool_call>
<tool_call>
{"name": "search_notes", "arguments": {"query": "plan", "tags": ["home", "planning"], "mode": "all"}}
</tool_call>
<tool_call>
{"name": "get_forecast", "arguments": {"city": "Paris"}}
</tool_call>
<tool_call>
{"name": "get_forecast", "arguments": {"city": "Lisbon", "days": "two"}}
</tool_call>
<tool_call>
{"name": "get_forecast", "arguments": {"city": "Lisbon", "days": 9}}
</tool_call>
<tool_call>
{"name": "search_notes", "arguments": {"query": "x", "mode": "some"}}
</tool_call>
<tool_call>
{"name": "lookup", "arguments": {}}
</tool_call>
<tool_call>
{"name": "get_forecast", "arguments": {"city": "Lisbon", "verbose": true, "debug": 1}}
</tool_call>
<tool_call>
{"name": "search_notes", "arguments": {"query": "plan",}}
</tool_call>"""
    text, calls = parse_tool_calls(model_output)
    print("text:", repr(text))
    print("calls:", len(calls))
    results = [registry.dispatch(c) for c in calls]
    for r in results:
        print(format_tool_response(r))
    print("hook saw:", PLUGIN_STATE["calls"])
    summary = {"ok": sum(r.ok for r in results), "failed": sum(not r.ok for r in results)}
    print(json.dumps(summary), asdict(results[0])["content"]["days"][-1])
    print(ToolCall("a", {}, "c1") == ToolCall("a", {}, "c1"), hash(ToolCall("a", (), "c1")) == hash(ToolCall("a", (), "c1")))
    try:
        register(PluginContext(registry, "again"))
    except ToolError as e:
        print("ToolError:", e)


if __name__ == "__main__":
    main()
