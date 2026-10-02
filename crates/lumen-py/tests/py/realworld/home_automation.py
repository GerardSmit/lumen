"""A Home Assistant style automation engine.

A state machine and an event bus run on asyncio; automations are loaded from a JSON config and
react to state changes and to time (sunset, fixed times, "no motion for N minutes"). Time is
virtual, as in Home Assistant's own tests: the driver fires `time_changed` events, so the run is
deterministic, while the waiting itself uses real asyncio tasks, events, locks and queues.
"""

import asyncio
import json
import logging
import sys
from dataclasses import dataclass, field, replace
from datetime import date, datetime, time, timedelta, timezone
from enum import Enum, IntFlag, auto
from typing import Any, Awaitable, Callable, Dict, List, Optional
from zoneinfo import ZoneInfo

TZ = ZoneInfo("Europe/Amsterdam")
UTC = timezone.utc


class VirtualClock:
    def __init__(self, start: datetime):
        self.now = start

    def local(self) -> datetime:
        return self.now.astimezone(TZ)


CLOCK = VirtualClock(datetime(2024, 3, 30, 16, 0, tzinfo=UTC))


class ClockFormatter(logging.Formatter):
    def formatTime(self, record, datefmt=None):
        return CLOCK.local().strftime(datefmt or "%H:%M")


log = logging.getLogger("homeassistant.automation")
_h = logging.StreamHandler(sys.stdout)
_h.setFormatter(ClockFormatter("[%(asctime)s %(tz)s] %(levelname)s %(message)s", defaults={"tz": "local"}))
log.addHandler(_h)
log.setLevel(logging.INFO)
log.propagate = False


class EntityState(str, Enum):
    ON = "on"
    OFF = "off"
    UNAVAILABLE = "unavailable"
    HOME = "home"
    NOT_HOME = "not_home"


class LightFeature(IntFlag):
    BRIGHTNESS = auto()
    COLOR_TEMP = auto()
    EFFECT = auto()
    TRANSITION = auto()


@dataclass(frozen=True)
class State:
    entity_id: str
    state: str
    attributes: Dict[str, Any] = field(default_factory=dict)
    last_changed: datetime = field(default_factory=lambda: CLOCK.now, compare=False)

    @property
    def domain(self) -> str:
        return self.entity_id.split(".", 1)[0]


@dataclass(order=True)
class Event:
    time_fired: datetime
    event_type: str = field(compare=False)
    data: Dict[str, Any] = field(default_factory=dict, compare=False)


Listener = Callable[[Event], Awaitable[None]]


class EventBus:
    def __init__(self):
        self._listeners: Dict[str, List[Listener]] = {}
        self._queue: asyncio.Queue[Event] = asyncio.Queue()
        self.fired: Dict[str, int] = {}

    def listen(self, event_type: str, listener: Listener) -> Callable[[], None]:
        self._listeners.setdefault(event_type, []).append(listener)

        def remove() -> None:
            self._listeners[event_type].remove(listener)

        return remove

    def fire(self, event_type: str, **data: Any) -> None:
        self.fired[event_type] = self.fired.get(event_type, 0) + 1
        self._queue.put_nowait(Event(CLOCK.now, event_type, data))

    async def run(self) -> None:
        while True:
            event = await self._queue.get()
            try:
                for listener in list(self._listeners.get(event.event_type, [])) + list(self._listeners.get("*", [])):
                    await listener(event)
            except Exception:
                log.exception("Error handling %s", event.event_type)
            finally:
                self._queue.task_done()

    async def drain(self) -> None:
        await self._queue.join()
        # Let tasks woken by the handlers run before the next tick.
        for _ in range(3):
            await asyncio.sleep(0)


class StateMachine:
    def __init__(self, bus: EventBus):
        self._states: Dict[str, State] = {}
        self._bus = bus

    def get(self, entity_id: str) -> Optional[State]:
        return self._states.get(entity_id)

    def set(self, entity_id: str, new_state: str, **attributes: Any) -> None:
        old = self._states.get(entity_id)
        merged = {**(old.attributes if old else {}), **attributes}
        if old is not None and old.state == new_state and old.attributes == merged:
            return
        if old is not None and old.state == new_state:
            state = replace(old, attributes=merged)
        else:
            state = State(entity_id, new_state, merged)
        self._states[entity_id] = state
        self._bus.fire("state_changed", entity_id=entity_id, old_state=old, new_state=state)


@dataclass
class Automation:
    alias: str
    trigger: Dict[str, Any]
    action: List[Dict[str, Any]]
    condition: List[Dict[str, Any]] = field(default_factory=list)
    mode: str = "single"
    runs: int = 0
    running: Optional[asyncio.Task] = field(default=None, repr=False)


def parse_offset(text: str) -> timedelta:
    sign = -1 if text.startswith("-") else 1
    h, m, s = (int(x) for x in text.lstrip("+-").split(":"))
    return sign * timedelta(hours=h, minutes=m, seconds=s)


# Sunset in Amsterdam, local wall time, around the 2024 spring DST switch.
SUNSET = {date(2024, 3, 30): time(19, 9), date(2024, 3, 31): time(20, 11), date(2024, 4, 1): time(20, 13)}


def next_sunset(after: datetime, offset: timedelta) -> datetime:
    day = after.astimezone(TZ).date()
    while True:
        local = datetime.combine(day, SUNSET[day], tzinfo=TZ) + offset
        if local.astimezone(UTC) > after:
            return local.astimezone(UTC)
        day += timedelta(days=1)


class Hass:
    def __init__(self, config: Dict[str, Any]):
        self.bus = EventBus()
        self.states = StateMachine(self.bus)
        self.automations = [Automation(**a) for a in config["automation"]]
        self.services: Dict[str, Callable[..., None]] = {}
        self.lock = asyncio.Lock()
        self._timers: List[tuple[datetime, int, Callable[[], None]]] = []
        self._seq = 0
        for entity_id, spec in config["entities"].items():
            self.states.set(entity_id, spec["state"], **spec.get("attributes", {}))
        self.services["light.turn_on"] = self._light_on
        self.services["light.turn_off"] = lambda entity_id: self.states.set(entity_id, EntityState.OFF.value)
        self.services["notify.mobile"] = lambda message: log.info("notify: %s", message)

    def _light_on(self, entity_id: str, brightness_pct: int = 100, color_temp_kelvin: Optional[int] = None) -> None:
        features = LightFeature(self.states.get(entity_id).attributes.get("supported_features", 0))
        attrs: Dict[str, Any] = {}
        if LightFeature.BRIGHTNESS in features:
            attrs["brightness"] = round(brightness_pct * 255 / 100)
        if color_temp_kelvin is not None:
            if LightFeature.COLOR_TEMP not in features:
                raise ValueError(f"{entity_id} does not support color temperature")
            attrs["color_temp_kelvin"] = color_temp_kelvin
        self.states.set(entity_id, EntityState.ON.value, **attrs)

    def track_point_in_time(self, when: datetime, action: Callable[[], None]) -> None:
        self._seq += 1
        self._timers.append((when, self._seq, action))
        self._timers.sort()

    async def sleep_until(self, when: datetime) -> None:
        done = asyncio.Event()
        self.track_point_in_time(when, done.set)
        await done.wait()

    async def advance(self, to: datetime) -> None:
        while self._timers and self._timers[0][0] <= to:
            when, _, action = self._timers.pop(0)
            CLOCK.now = when
            self.bus.fire("time_changed", now=when)
            action()
            await self.bus.drain()
        CLOCK.now = to
        await self.bus.drain()

    def check_conditions(self, automation: Automation) -> bool:
        for cond in automation.condition:
            if cond["condition"] == "state":
                st = self.states.get(cond["entity_id"])
                if st is None or st.state != cond["state"]:
                    return False
            elif cond["condition"] == "time":
                now = CLOCK.local().time()
                after, before = time.fromisoformat(cond["after"]), time.fromisoformat(cond["before"])
                inside = after <= now < before if after < before else (now >= after or now < before)
                if not inside:
                    return False
        return True

    async def run_actions(self, automation: Automation, variables: Dict[str, Any]) -> None:
        for step in automation.action:
            if "delay" in step:
                await self.sleep_until(CLOCK.now + parse_offset(step["delay"]))
                continue
            service = step["service"]
            data = {k: (v.format(**variables) if isinstance(v, str) else v) for k, v in step.get("data", {}).items()}
            async with self.lock:
                try:
                    self.services[service](**data)
                except (ValueError, TypeError) as e:
                    log.warning("%s: %s failed: %s", automation.alias, service, e)
                    return

    def trigger(self, automation: Automation, variables: Dict[str, Any]) -> None:
        if not self.check_conditions(automation):
            log.info("%s: conditions not met", automation.alias)
            return
        if automation.running is not None and not automation.running.done():
            if automation.mode == "restart":
                automation.running.cancel()
                log.info("%s: restarting", automation.alias)
            else:
                log.info("%s: already running", automation.alias)
                return
        automation.runs += 1
        log.info("%s: triggered by %s", automation.alias, variables.get("trigger", "?"))
        automation.running = asyncio.get_running_loop().create_task(self.run_actions(automation, variables))
        automation.running.set_name(automation.alias)

    def setup(self) -> None:
        for automation in self.automations:
            trig = automation.trigger
            if trig["platform"] == "state":
                async def on_state(event: Event, a=automation, t=trig) -> None:
                    new, old = event.data["new_state"], event.data["old_state"]
                    if event.data["entity_id"] != t["entity_id"] or new.state != t["to"]:
                        return
                    if old is not None and old.state == new.state:
                        return
                    porch = self.states.get("light.porch").state
                    self.trigger(a, {"trigger": t["entity_id"], "name": new.attributes.get("friendly_name", ""), "porch": porch})
                self.bus.listen("state_changed", on_state)
            elif trig["platform"] == "sun":
                offset = parse_offset(trig.get("offset", "00:00:00"))

                def schedule(a=automation, offset=offset) -> None:
                    when = next_sunset(CLOCK.now, offset)
                    log.info("%s: next sunset trigger at %s", a.alias, when.astimezone(TZ).isoformat())

                    def fire() -> None:
                        self.trigger(a, {"trigger": "sunset"})
                        schedule()

                    self.track_point_in_time(when, fire)

                schedule()
            elif trig["platform"] == "time":
                at = time.fromisoformat(trig["at"])

                def schedule_time(a=automation, at=at) -> None:
                    local = CLOCK.local()
                    when = datetime.combine(local.date(), at, tzinfo=TZ)
                    if when <= local:
                        when = datetime.combine(local.date() + timedelta(days=1), at, tzinfo=TZ)
                    self.track_point_in_time(when.astimezone(UTC), lambda: (self.trigger(a, {"trigger": "time"}), schedule_time()))

                schedule_time()


CONFIG = json.loads("""
{
  "entities": {
    "light.living_room": {"state": "off", "attributes": {"friendly_name": "Living Room", "supported_features": 3}},
    "light.porch": {"state": "off", "attributes": {"friendly_name": "Porch", "supported_features": 1}},
    "binary_sensor.hallway_motion": {"state": "off"},
    "person.anna": {"state": "home", "attributes": {"friendly_name": "Anna"}}
  },
  "automation": [
    {"alias": "Porch light at sunset", "trigger": {"platform": "sun", "event": "sunset", "offset": "-00:15:00"},
     "action": [{"service": "light.turn_on", "data": {"entity_id": "light.porch", "brightness_pct": 60}}]},
    {"alias": "Hallway motion", "mode": "restart",
     "trigger": {"platform": "state", "entity_id": "binary_sensor.hallway_motion", "to": "on"},
     "condition": [{"condition": "time", "after": "18:00", "before": "07:00"}],
     "action": [{"service": "light.turn_on", "data": {"entity_id": "light.living_room", "brightness_pct": 40, "color_temp_kelvin": 2700}},
                {"delay": "00:05:00"},
                {"service": "light.turn_off", "data": {"entity_id": "light.living_room"}}]},
    {"alias": "Anna leaves", "trigger": {"platform": "state", "entity_id": "person.anna", "to": "not_home"},
     "action": [{"service": "notify.mobile", "data": {"message": "{name} left; porch light {porch}"}}]},
    {"alias": "Porch color", "trigger": {"platform": "state", "entity_id": "light.porch", "to": "on"},
     "action": [{"service": "light.turn_on", "data": {"entity_id": "light.porch", "color_temp_kelvin": 3000}}]},
    {"alias": "Lights off at midnight", "trigger": {"platform": "time", "at": "00:30:00"},
     "condition": [{"condition": "state", "entity_id": "person.anna", "state": "home"}],
     "action": [{"service": "light.turn_off", "data": {"entity_id": "light.porch"}}]}
  ]
}
""")


async def main() -> None:
    hass = Hass(CONFIG)
    changes: List[str] = []

    async def recorder(event: Event) -> None:
        if event.event_type == "state_changed":
            new = event.data["new_state"]
            changes.append(f"{CLOCK.local():%H:%M} {new.entity_id}={new.state}")

    hass.bus.listen("*", recorder)
    runner = asyncio.create_task(hass.bus.run(), name="event-bus")
    await hass.bus.drain()
    hass.setup()

    start = CLOCK.local()
    print("start:", start.isoformat(), start.tzname(), start.utcoffset())

    await hass.advance(datetime(2024, 3, 30, 18, 30, tzinfo=UTC))
    hass.states.set("binary_sensor.hallway_motion", "on")
    await hass.bus.drain()
    await hass.advance(CLOCK.now + timedelta(minutes=3))
    hass.states.set("binary_sensor.hallway_motion", "off")
    hass.states.set("binary_sensor.hallway_motion", "on")
    await hass.bus.drain()
    await hass.advance(CLOCK.now + timedelta(minutes=4))
    print("living room:", hass.states.get("light.living_room").state)
    await hass.advance(CLOCK.now + timedelta(minutes=2))
    print("living room:", hass.states.get("light.living_room").state, hass.states.get("light.living_room").attributes)

    # Across the DST switch (02:00 CET -> 03:00 CEST on 31 March).
    await hass.advance(datetime(2024, 3, 31, 3, 0, tzinfo=UTC))
    local = CLOCK.local()
    print("after DST:", local.isoformat(), local.tzname(), local.dst())
    hass.states.set("person.anna", EntityState.NOT_HOME.value, porch="ignored")
    await hass.bus.drain()
    hass.states.set("person.anna", EntityState.HOME.value)
    await hass.advance(datetime(2024, 4, 1, 0, 0, tzinfo=UTC))

    # Waiting for something that never happens.
    never = asyncio.Event()
    try:
        await asyncio.wait_for(never.wait(), timeout=0.01)
    except asyncio.TimeoutError:
        print("wait_for timed out")

    results = await asyncio.gather(*(asyncio.sleep(0.001 * i, result=i * i) for i in range(4)))
    print("gather:", results)

    runner.cancel()
    try:
        await runner
    except asyncio.CancelledError:
        print("event bus stopped; cancelled:", runner.cancelled())

    print("\n".join(changes))
    for a in hass.automations:
        print(f"{a.alias:<24} runs={a.runs} mode={a.mode}")
    print("events:", sorted(hass.bus.fired.items()))
    print("features:", LightFeature(3), LightFeature.BRIGHTNESS | LightFeature.EFFECT, int(LightFeature.TRANSITION))
    print([str(s) for s in (EntityState.ON, EntityState("home"))], EntityState.OFF == "off")
    gap = datetime(2024, 3, 31, 2, 30, tzinfo=TZ)
    print("gap:", gap.isoformat(), gap.astimezone(UTC).isoformat())
    fold0 = datetime(2024, 10, 27, 2, 30, tzinfo=TZ)
    fold1 = fold0.replace(fold=1)
    print("fold:", fold0.utcoffset(), fold1.utcoffset(), fold1.astimezone(UTC).time())
    ny = datetime(2024, 3, 31, 12, 0, tzinfo=ZoneInfo("America/New_York"))
    print("ny:", ny.astimezone(TZ).isoformat(), ny - datetime(2024, 3, 31, 12, 0, tzinfo=TZ), repr(TZ))


asyncio.run(main())
