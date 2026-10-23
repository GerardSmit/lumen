(function () {
'use strict';
const g = globalThis;
if (typeof g.TextTrack === 'function' || typeof g.EventTarget !== 'function') return;
const TOKEN = {};
const slots = new WeakMap();
const slot = (o) => slots.get(o);

function publish(name, ctor) {
    Object.defineProperty(g, name, { value: ctor, writable: true, enumerable: false, configurable: true });
    Object.defineProperty(ctor.prototype, Symbol.toStringTag, { value: name, configurable: true });
}
function define(proto, props) {
    for (const key of Reflect.ownKeys(props)) {
        const d = Object.getOwnPropertyDescriptor(props, key);
        d.enumerable = true;
        d.configurable = true;
        Object.defineProperty(proto, key, d);
    }
}
function illegal() {
    throw new TypeError('Illegal constructor');
}
function handlerAttributes(proto, names) {
    for (const name of names) {
        define(proto, {
            get ['on' + name]() {
                const s = slot(this);
                return s.handlers[name] || null;
            },
            set ['on' + name](value) {
                const s = slot(this);
                const handler = typeof value === 'function' || (typeof value === 'object' && value !== null) ? value : null;
                s.handlers[name] = handler;
                if (handler && !s.listening[name]) {
                    s.listening[name] = true;
                    const target = s.self;
                    EventTarget.prototype.addEventListener.call(target, name, function (event) {
                        const h = s.handlers[name];
                        if (typeof h === 'function') {
                            h.call(target, event);
                        }
                    });
                }
            },
        });
    }
}
const isIndex = (key) => typeof key === 'string' && /^(0|[1-9][0-9]*)$/.test(key) && Number(key) < 4294967295;

function indexedProxy(target, items, bindMethods) {
    const bound = new Map();
    return new Proxy(target, {
        get(t, key) {
            if (isIndex(key)) {
                return items()[Number(key)];
            }
            const value = Reflect.get(t, key, t);
            if (bindMethods && typeof value === 'function' && key !== 'constructor') {
                let fn = bound.get(key);
                if (!fn || fn.original !== value) {
                    fn = value.bind(t);
                    fn.original = value;
                    bound.set(key, fn);
                }
                return fn;
            }
            return value;
        },
        set(t, key, value) {
            if (isIndex(key)) {
                return false;
            }
            return Reflect.set(t, key, value, t);
        },
        has(t, key) {
            if (isIndex(key)) {
                return Number(key) < items().length;
            }
            return Reflect.has(t, key);
        },
        getOwnPropertyDescriptor(t, key) {
            if (isIndex(key)) {
                const list = items();
                const i = Number(key);
                return i < list.length
                    ? { value: list[i], writable: false, enumerable: true, configurable: true }
                    : undefined;
            }
            return Reflect.getOwnPropertyDescriptor(t, key);
        },
        defineProperty(t, key, descriptor) {
            if (isIndex(key)) {
                return false;
            }
            return Reflect.defineProperty(t, key, descriptor);
        },
        deleteProperty(t, key) {
            if (isIndex(key)) {
                return Number(key) >= items().length;
            }
            return Reflect.deleteProperty(t, key);
        },
        ownKeys(t) {
            const keys = items().map((_, i) => String(i));
            return keys.concat(Reflect.ownKeys(t));
        },
    });
}

class TextTrackCue extends EventTarget {
    constructor(token) {
        if (token !== TOKEN) {
            illegal();
        }
        super();
    }
}
function initCue(cue, start, end, text) {
    slots.set(cue, {
        self: cue,
        handlers: {},
        listening: {},
        track: null,
        order: 0,
        id: '',
        start,
        end,
        pauseOnExit: false,
        text,
    });
}
function finiteNumber(value) {
    const n = Number(value);
    if (!Number.isFinite(n)) {
        throw new TypeError('The provided double value is non-finite.');
    }
    return n;
}
function endNumber(value) {
    const n = Number(value);
    if (Number.isNaN(n) || n === -Infinity) {
        throw new TypeError('The provided double value is not valid.');
    }
    return n;
}
define(TextTrackCue.prototype, {
    get track() { return slot(this).track; },
    get id() { return slot(this).id; },
    set id(v) { slot(this).id = String(v); },
    get startTime() { return slot(this).start; },
    set startTime(v) { slot(this).start = finiteNumber(v); },
    get endTime() { return slot(this).end; },
    set endTime(v) { slot(this).end = endNumber(v); },
    get pauseOnExit() { return slot(this).pauseOnExit; },
    set pauseOnExit(v) { slot(this).pauseOnExit = Boolean(v); },
});
handlerAttributes(TextTrackCue.prototype, ['enter', 'exit']);
publish('TextTrackCue', TextTrackCue);

const enumAttr = (name, allowed, initial) => {
    const state = Symbol(name);
    return {
        get [name]() { return slot(this)[state] === undefined ? initial : slot(this)[state]; },
        set [name](v) {
            const value = String(v);
            if (allowed.includes(value)) {
                slot(this)[state] = value;
            }
        },
    };
};
const autoOrNumber = (name) => {
    const state = Symbol(name);
    return {
        get [name]() { return slot(this)[state] === undefined ? 'auto' : slot(this)[state]; },
        set [name](v) {
            if (v === 'auto') {
                slot(this)[state] = 'auto';
                return;
            }
            const n = Number(v);
            if (!Number.isFinite(n)) {
                throw new TypeError('The provided double value is non-finite.');
            }
            if (name === 'position' && (n < 0 || n > 100)) {
                throw new DOMException('Value out of range', 'IndexSizeError');
            }
            slot(this)[state] = n;
        },
    };
};

class VTTCue extends TextTrackCue {
    constructor(startTime, endTime, text) {
        if (arguments.length < 3) {
            throw new TypeError("Failed to construct 'VTTCue': 3 arguments required.");
        }
        super(TOKEN);
        initCue(this, finiteNumber(startTime), endNumber(endTime), String(text));
    }
    getCueAsHTML() {
        const fragment = document.createDocumentFragment();
        fragment.appendChild(document.createTextNode(slot(this).text));
        return fragment;
    }
}
const regionState = Symbol('region');
const snapState = Symbol('snapToLines');
const sizeState = Symbol('size');
define(VTTCue.prototype, {
    get region() { const r = slot(this)[regionState]; return r === undefined ? null : r; },
    set region(v) { slot(this)[regionState] = v === undefined ? null : v; },
    get snapToLines() { const v = slot(this)[snapState]; return v === undefined ? true : v; },
    set snapToLines(v) { slot(this)[snapState] = Boolean(v); },
    get size() { const v = slot(this)[sizeState]; return v === undefined ? 100 : v; },
    set size(v) {
        const n = finiteNumber(v);
        if (n < 0 || n > 100) {
            throw new DOMException('Value out of range', 'IndexSizeError');
        }
        slot(this)[sizeState] = n;
    },
    get text() { return slot(this).text; },
    set text(v) { slot(this).text = String(v); },
});
define(VTTCue.prototype, enumAttr('vertical', ['', 'rl', 'lr'], ''));
define(VTTCue.prototype, autoOrNumber('line'));
define(VTTCue.prototype, enumAttr('lineAlign', ['start', 'center', 'end'], 'start'));
define(VTTCue.prototype, autoOrNumber('position'));
define(VTTCue.prototype, enumAttr('positionAlign', ['line-left', 'center', 'line-right', 'auto'], 'auto'));
define(VTTCue.prototype, enumAttr('align', ['start', 'center', 'end', 'left', 'right'], 'center'));
publish('VTTCue', VTTCue);

class TextTrackCueList {
    constructor() {
        illegal();
    }
    getCueById(id) {
        const wanted = String(id);
        if (wanted === '') {
            return null;
        }
        const found = slot(this).items().find((cue) => slot(cue).id === wanted);
        return found || null;
    }
}
define(TextTrackCueList.prototype, {
    get length() { return slot(this).items().length; },
});
publish('TextTrackCueList', TextTrackCueList);

function makeCueList(items) {
    const target = Object.create(TextTrackCueList.prototype);
    const state = { items };
    slots.set(target, state);
    const proxy = indexedProxy(target, items, false);
    slots.set(proxy, state);
    return proxy;
}

const kinds = ['subtitles', 'captions', 'descriptions', 'chapters', 'metadata'];
const modes = ['disabled', 'hidden', 'showing'];

class TextTrack extends EventTarget {
    constructor(token) {
        if (token !== TOKEN) {
            illegal();
        }
        super();
    }
    addCue(cue) {
        if (!(cue instanceof TextTrackCue)) {
            throw new TypeError("Failed to execute 'addCue' on 'TextTrack': parameter 1 is not of type 'TextTrackCue'.");
        }
        const s = slot(this);
        const cs = slot(cue);
        if (cs.track) {
            const old = slot(cs.track);
            old.cues.splice(old.cues.indexOf(cue), 1);
        }
        cs.track = this;
        cs.order = ++s.counter;
        s.cues.push(cue);
    }
    removeCue(cue) {
        if (!(cue instanceof TextTrackCue)) {
            throw new TypeError("Failed to execute 'removeCue' on 'TextTrack': parameter 1 is not of type 'TextTrackCue'.");
        }
        const s = slot(this);
        const index = s.cues.indexOf(cue);
        if (index < 0) {
            throw new DOMException('The cue is not listed in this track.', 'NotFoundError');
        }
        s.cues.splice(index, 1);
        slot(cue).track = null;
    }
}
function sortedCues(s) {
    return s.cues
        .slice()
        .sort((a, b) => {
            const x = slot(a);
            const y = slot(b);
            return x.start - y.start || y.end - x.end || x.order - y.order;
        });
}
function activeCues(s) {
    const media = s.media();
    if (!media) {
        return [];
    }
    const now = media.currentTime;
    return sortedCues(s).filter((cue) => {
        const c = slot(cue);
        return c.start <= now && now < c.end;
    });
}
define(TextTrack.prototype, {
    get kind() { return slot(this).kind(); },
    get label() { return slot(this).label(); },
    get language() { return slot(this).language(); },
    get id() { return slot(this).id(); },
    get inBandMetadataTrackDispatchType() { return ''; },
    get mode() { return slot(this).mode; },
    set mode(v) {
        const value = String(v);
        if (modes.includes(value)) {
            slot(this).mode = value;
        }
    },
    get cues() {
        const s = slot(this);
        return s.mode === 'disabled' ? null : s.cuesList;
    },
    get activeCues() {
        const s = slot(this);
        return s.mode === 'disabled' ? null : s.activeList;
    },
});
handlerAttributes(TextTrack.prototype, ['cuechange']);
publish('TextTrack', TextTrack);

function newTrack(init) {
    const track = new TextTrack(TOKEN);
    const s = Object.assign(
        { self: track, handlers: {}, listening: {}, mode: 'disabled', cues: [], counter: 0 },
        init,
    );
    s.cuesList = makeCueList(() => sortedCues(s));
    s.activeList = makeCueList(() => activeCues(s));
    slots.set(track, s);
    return track;
}

class TextTrackList extends EventTarget {
    constructor() {
        illegal();
    }
    getTrackById(id) {
        const wanted = String(id);
        const found = slot(this).items().find((track) => track.id === wanted);
        return found || null;
    }
}
define(TextTrackList.prototype, {
    get length() { return slot(this).items().length; },
});
handlerAttributes(TextTrackList.prototype, ['change', 'addtrack', 'removetrack']);
publish('TextTrackList', TextTrackList);

const isHtml = (el) => el.namespaceURI === 'http://www.w3.org/1999/xhtml';
const mediaStates = new WeakMap();
function mediaState(media) {
    let state = mediaStates.get(media);
    if (state) {
        return state;
    }
    state = { added: [] };
    const items = () => {
        const fromElements = [];
        for (const child of media.children) {
            if (child.localName === 'track' && isHtml(child)) {
                fromElements.push(child.track);
            }
        }
        return fromElements.concat(state.added);
    };
    const list = Object.setPrototypeOf(new EventTarget(), TextTrackList.prototype);
    const listState = { self: list, handlers: {}, listening: {}, items };
    slots.set(list, listState);
    const proxy = indexedProxy(list, items, true);
    slots.set(proxy, listState);
    state.list = proxy;
    state.target = list;
    mediaStates.set(media, state);
    return state;
}

define(HTMLMediaElement.prototype, {
    get textTracks() { return mediaState(this).list; },
    addTextTrack(kind, label, language) {
        if (arguments.length < 1) {
            throw new TypeError("Failed to execute 'addTextTrack': 1 argument required.");
        }
        const k = String(kind);
        if (!kinds.includes(k)) {
            throw new TypeError("Failed to execute 'addTextTrack': The provided value '" + k + "' is not a valid enum value of type TextTrackKind.");
        }
        const l = label === undefined ? '' : String(label);
        const lang = language === undefined ? '' : String(language);
        const media = this;
        const track = newTrack({
            kind: () => k,
            label: () => l,
            language: () => lang,
            id: () => '',
            mode: 'hidden',
            media: () => media,
        });
        const state = mediaState(this);
        state.added.push(track);
        setTimeout(() => {
            const event = new TrackEvent('addtrack', { track });
            state.target.dispatchEvent(event);
        }, 0);
        return track;
    },
});

if (typeof g.HTMLTrackElement === 'function') {
    const trackStates = new WeakMap();
    define(HTMLTrackElement.prototype, {
        get track() {
            let track = trackStates.get(this);
            if (!track) {
                const el = this;
                track = newTrack({
                    kind: () => el.kind,
                    label: () => el.label,
                    language: () => el.srclang,
                    id: () => el.id,
                    media: () => {
                        const parent = el.parentNode;
                        return parent instanceof HTMLMediaElement ? parent : null;
                    },
                });
                trackStates.set(this, track);
            }
            return track;
        },
    });
}

class TrackEvent extends Event {
    constructor(type, eventInitDict) {
        if (arguments.length < 1) {
            throw new TypeError("Failed to construct 'TrackEvent': 1 argument required.");
        }
        super(type, eventInitDict);
        const init = eventInitDict === undefined || eventInitDict === null ? {} : Object(eventInitDict);
        const track = init.track === undefined ? null : init.track;
        if (track !== null && typeof track !== 'object') {
            throw new TypeError("Failed to construct 'TrackEvent': The provided 'track' is not an object.");
        }
        slots.set(this, { track });
    }
}
define(TrackEvent.prototype, {
    get track() { return slot(this).track; },
});
publish('TrackEvent', TrackEvent);
})();
