// A cluster of smaller node: builtins. Each is a real implementation of the parts that mean
// something on lumen; where a feature is inherently V8- or debugger-specific it degrades to the
// honest behavior for a non-V8, non-inspected process (a no-op or a clear throw), never a fake
// success.

// ---- node:v8 ----------------------------------------------------------------------------------
__lazyGlue(__glueIndex, "v8", "", "", () => {
  const te = new TextEncoder();
  const td = new TextDecoder();

  // A genuine structured-clone-style codec over Buffer. It is NOT V8's private wire format (that is
  // engine-internal and not portable), but it is self-consistent: serialize -> deserialize round-
  // trips the object graph, including the reference types JSON drops (Map/Set/Date/BigInt/typed
  // arrays/ArrayBuffer) and shared/circular references. Format is a 2-byte header then tagged
  // values; container tags register the object before recursing so cycles resolve via back-refs.
  const TAG = {
    UNDEFINED: 0, NULL: 1, TRUE: 2, FALSE: 3, NUMBER: 4, STRING: 5, BIGINT: 6,
    ARRAY: 7, OBJECT: 8, MAP: 9, SET: 10, DATE: 11, REGEXP: 12,
    ARRAYBUFFER: 13, TYPEDARRAY: 14, BUFFER: 15, REF: 16,
  };
  // Ordered so an index survives round-trips; BUFFER is handled separately (it is Uint8Array too).
  const VIEW_CTORS = [
    Int8Array, Uint8Array, Uint8ClampedArray, Int16Array, Uint16Array,
    Int32Array, Uint32Array, Float32Array, Float64Array, BigInt64Array, BigUint64Array, DataView,
  ];
  const scratch = new DataView(new ArrayBuffer(8));

  class Serializer {
    constructor() {
      this._bytes = [];
      this._ids = new Map();
      this._nextId = 0;
    }
    _byte(b) { this._bytes.push(b & 0xff); }
    writeHeader() { this._byte(0xff); this._byte(0x0f); }
    writeUint32(v) {
      this._byte(v); this._byte(v >>> 8); this._byte(v >>> 16); this._byte(v >>> 24);
    }
    writeUint64(v) {
      const lo = Number(BigInt(v) & 0xffffffffn);
      const hi = Number((BigInt(v) >> 32n) & 0xffffffffn);
      this.writeUint32(lo); this.writeUint32(hi);
    }
    writeDouble(v) {
      scratch.setFloat64(0, v, true);
      for (let i = 0; i < 8; i++) this._byte(scratch.getUint8(i));
    }
    writeRawBytes(bytes) { for (let i = 0; i < bytes.length; i++) this._byte(bytes[i]); }
    _writeString(str) {
      const enc = te.encode(str);
      this.writeUint32(enc.length);
      this.writeRawBytes(enc);
    }
    transferArrayBuffer() {}
    _setTreatArrayBufferViewsAsHostObjects() {}
    _writeHostObject() {
      const err = new Error("Unserializable host object");
      err.code = "ERR_CANNOT_TRANSFER_OBJECT";
      throw err;
    }
    _ref(obj) {
      if (this._ids.has(obj)) { this._byte(TAG.REF); this.writeUint32(this._ids.get(obj)); return true; }
      this._ids.set(obj, this._nextId++);
      return false;
    }
    writeValue(value) {
      const t = typeof value;
      if (value === undefined) return this._byte(TAG.UNDEFINED);
      if (value === null) return this._byte(TAG.NULL);
      if (t === "boolean") return this._byte(value ? TAG.TRUE : TAG.FALSE);
      if (t === "number") { this._byte(TAG.NUMBER); return this.writeDouble(value); }
      if (t === "string") { this._byte(TAG.STRING); return this._writeString(value); }
      if (t === "bigint") { this._byte(TAG.BIGINT); return this._writeString(value.toString()); }
      if (t !== "object" && t !== "function") {
        const err = new Error(`Unsupported value type: ${t}`);
        err.code = "ERR_CANNOT_TRANSFER_OBJECT";
        throw err;
      }
      if ((typeof SharedArrayBuffer === "function" && value instanceof SharedArrayBuffer) || globalThis.__lumenPortClone?.isPort(value)) {
        throw new Error("Shared memory and MessagePort require native message transport");
      }
      if (this._ref(value)) return;
      if (Array.isArray(value)) {
        this._byte(TAG.ARRAY);
        this.writeUint32(value.length);
        for (let i = 0; i < value.length; i++) this.writeValue(value[i]);
        return;
      }
      if (value instanceof Date) { this._byte(TAG.DATE); return this.writeDouble(value.getTime()); }
      if (value instanceof RegExp) {
        this._byte(TAG.REGEXP); this._writeString(value.source); return this._writeString(value.flags);
      }
      if (value instanceof Map) {
        this._byte(TAG.MAP);
        this.writeUint32(value.size);
        for (const [k, v] of value) { this.writeValue(k); this.writeValue(v); }
        return;
      }
      if (value instanceof Set) {
        this._byte(TAG.SET);
        this.writeUint32(value.size);
        for (const v of value) this.writeValue(v);
        return;
      }
      if (value instanceof ArrayBuffer) {
        this._byte(TAG.ARRAYBUFFER);
        const view = new Uint8Array(value);
        this.writeUint32(view.length);
        return this.writeRawBytes(view);
      }
      if (typeof Buffer !== "undefined" && Buffer.isBuffer(value)) {
        this._byte(TAG.BUFFER);
        this.writeUint32(value.length);
        return this.writeRawBytes(value);
      }
      if (ArrayBuffer.isView(value)) {
        this._byte(TAG.TYPEDARRAY);
        this._byte(VIEW_CTORS.indexOf(value.constructor));
        const raw = new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
        this.writeUint32(raw.length);
        return this.writeRawBytes(raw);
      }
      // Plain object: own enumerable string keys (Symbols are dropped, as in structured clone).
      this._byte(TAG.OBJECT);
      const keys = Object.keys(value);
      this.writeUint32(keys.length);
      for (const k of keys) { this._writeString(k); this.writeValue(value[k]); }
    }
    releaseBuffer() { return Buffer.from(this._bytes); }
  }

  // DefaultSerializer is the class serialize() uses; it differs from Serializer only in how it
  // treats host objects (here: the base behavior is already correct for our surface).
  class DefaultSerializer extends Serializer {}

  class Deserializer {
    constructor(buffer) {
      this._buf = buffer instanceof Uint8Array ? buffer : new Uint8Array(buffer);
      this._view = new DataView(this._buf.buffer, this._buf.byteOffset, this._buf.byteLength);
      this._pos = 0;
      this._objs = [];
    }
    readHeader() { this._pos += 2; return 0x0f; }
    getWireFormatVersion() { return 0x0f; }
    readUint32() {
      const v = this._view.getUint32(this._pos, true);
      this._pos += 4;
      return v;
    }
    readUint64() {
      const lo = BigInt(this.readUint32());
      const hi = BigInt(this.readUint32());
      return Number((hi << 32n) | lo);
    }
    readDouble() {
      const v = this._view.getFloat64(this._pos, true);
      this._pos += 8;
      return v;
    }
    readRawBytes(len) {
      const out = this._buf.subarray(this._pos, this._pos + len);
      this._pos += len;
      return out;
    }
    _readString() {
      const len = this.readUint32();
      return td.decode(this.readRawBytes(len));
    }
    transferArrayBuffer() {}
    _readHostObject() {
      throw new Error("Unserializable host object");
    }
    readValue() {
      const tag = this._buf[this._pos++];
      switch (tag) {
        case TAG.UNDEFINED: return undefined;
        case TAG.NULL: return null;
        case TAG.TRUE: return true;
        case TAG.FALSE: return false;
        case TAG.NUMBER: return this.readDouble();
        case TAG.STRING: return this._readString();
        case TAG.BIGINT: return BigInt(this._readString());
        case TAG.REF: return this._objs[this.readUint32()];
        case TAG.ARRAY: {
          const n = this.readUint32();
          const arr = [];
          this._objs.push(arr);
          for (let i = 0; i < n; i++) arr.push(this.readValue());
          return arr;
        }
        case TAG.OBJECT: {
          const n = this.readUint32();
          const obj = {};
          this._objs.push(obj);
          for (let i = 0; i < n; i++) { const k = this._readString(); obj[k] = this.readValue(); }
          return obj;
        }
        case TAG.MAP: {
          const n = this.readUint32();
          const map = new Map();
          this._objs.push(map);
          for (let i = 0; i < n; i++) { const k = this.readValue(); map.set(k, this.readValue()); }
          return map;
        }
        case TAG.SET: {
          const n = this.readUint32();
          const set = new Set();
          this._objs.push(set);
          for (let i = 0; i < n; i++) set.add(this.readValue());
          return set;
        }
        case TAG.DATE: { const d = new Date(this.readDouble()); this._objs.push(d); return d; }
        case TAG.REGEXP: {
          const re = new RegExp(this._readString(), this._readString());
          this._objs.push(re);
          return re;
        }
        case TAG.ARRAYBUFFER: {
          const n = this.readUint32();
          const ab = this.readRawBytes(n).slice().buffer;
          this._objs.push(ab);
          return ab;
        }
        case TAG.BUFFER: {
          const n = this.readUint32();
          const b = Buffer.from(this.readRawBytes(n));
          this._objs.push(b);
          return b;
        }
        case TAG.TYPEDARRAY: {
          const kind = this._buf[this._pos++];
          const n = this.readUint32();
          const bytes = this.readRawBytes(n).slice();
          const Ctor = VIEW_CTORS[kind];
          const view = Ctor === DataView ? new DataView(bytes.buffer) : new Ctor(bytes.buffer);
          this._objs.push(view);
          return view;
        }
        default:
          throw new Error(`v8.deserialize: unknown tag ${tag}`);
      }
    }
  }

  class DefaultDeserializer extends Deserializer {}

  const serialize = (value) => {
    const s = new DefaultSerializer();
    s.writeHeader();
    s.writeValue(value);
    return s.releaseBuffer();
  };
  const deserialize = (buffer) => {
    const d = new DefaultDeserializer(buffer);
    d.readHeader();
    return d.readValue();
  };

  // lumen exposes no V8 heap accounting, so the numeric fields are honest zeros in Node's shape
  // (rather than invented figures). Field names and count match Node v22 exactly.
  const HEAP_LIMIT = 2 * 1024 * 1024 * 1024;
  const getHeapStatistics = () => ({
    total_heap_size: 0,
    total_heap_size_executable: 0,
    total_physical_size: 0,
    total_available_size: HEAP_LIMIT,
    used_heap_size: 0,
    heap_size_limit: HEAP_LIMIT,
    malloced_memory: 0,
    peak_malloced_memory: 0,
    does_zap_garbage: 0,
    number_of_native_contexts: 1,
    number_of_detached_contexts: 0,
    total_global_handles_size: 0,
    used_global_handles_size: 0,
    external_memory: 0,
  });
  const HEAP_SPACES = [
    "read_only_space", "new_space", "old_space", "code_space", "shared_space",
    "new_large_object_space", "large_object_space",
    "code_large_object_space", "shared_large_object_space",
  ];
  const getHeapSpaceStatistics = () =>
    HEAP_SPACES.map((space_name) => ({
      space_name,
      space_size: 0,
      space_used_size: 0,
      space_available_size: 0,
      physical_space_size: 0,
    }));
  const getHeapCodeStatistics = () => ({
    code_and_metadata_size: 0,
    bytecode_and_metadata_size: 0,
    external_script_source_size: 0,
    cpu_profiler_metadata_size: 0,
  });
  const getCppHeapStatistics = () => ({
    committed_size_bytes: 0,
    resident_size_bytes: 0,
    used_size_bytes: 0,
    space_statistics: [],
    type_names: [],
    detail_level: "brief",
  });

  // A real byte-level check: whether the string is representable in a single-byte (latin1) form.
  const isStringOneByteRepresentation = (str) => {
    if (typeof str !== "string") {
      const err = new TypeError('The "content" argument must be of type string.');
      err.code = "ERR_INVALID_ARG_TYPE";
      throw err;
    }
    for (let i = 0; i < str.length; i++) if (str.charCodeAt(i) > 0xff) return false;
    return true;
  };

  // Not backed by V8's build hash; a stable tag so callers that only compare it for equality across
  // a single process (compile-cache guards) behave consistently.
  let flagSalt = 0;
  const cachedDataVersionTag = () => (0x6c756d65 ^ flagSalt) >>> 0;
  const setFlagsFromString = (flags) => {
    if (typeof flags !== "string") throw new __errors.ERR_INVALID_ARG_TYPE("flags", "string", flags);
    for (let i = 0; i < flags.length; i++) flagSalt = (Math.imul(flagSalt, 31) + flags.charCodeAt(i) + 1) | 0;
  };

  // Inert GC profiler: lumen surfaces no GC event stream, so a session records nothing.
  class GCProfiler {
    start() { this._start = Date.now(); }
    stop() {
      return { version: 1, startTime: this._start || Date.now(), statistics: [], endTime: Date.now() };
    }
  }

  // ---- heap snapshots ------------------------------------------------------------------------
  // A V8-format `.heapsnapshot` of the object graph reachable from the global object (own data
  // properties and prototypes, walked breadth-first). lumen has no heap profiler, so the graph is
  // what JS reflection can see, bounded to `kMaxSnapshotNodes` objects and `kMaxEdgesPerNode`
  // edges per object; engine-internal structures are absent.
  const kMaxSnapshotNodes = 50000;
  const kMaxEdgesPerNode = 64;
  const NODE_TYPES = ["hidden", "array", "string", "object", "code", "closure", "regexp", "number",
    "native", "synthetic", "concatenated string", "sliced string", "symbol", "bigint", "object shape"];
  const EDGE_TYPES = ["context", "element", "property", "internal", "hidden", "shortcut", "weak"];
  const T_ARRAY = 1, T_OBJECT = 3, T_CLOSURE = 5, T_SYNTHETIC = 9;
  const E_ELEMENT = 1, E_PROPERTY = 2, E_INTERNAL = 3, E_SHORTCUT = 5;

  function buildHeapSnapshot() {
    const strings = [];
    const stringIds = new Map();
    const intern = (s) => {
      let id = stringIds.get(s);
      if (id === undefined) {
        id = strings.length;
        strings.push(s);
        stringIds.set(s, id);
      }
      return id;
    };
    const nodes = [];
    const edges = [];
    const indexOf = new Map();
    const queue = [];
    const classNameOf = (obj) => {
      try {
        let proto = Object.getPrototypeOf(obj);
        for (let depth = 0; proto !== null && depth < 8; depth++, proto = Object.getPrototypeOf(proto)) {
          const d = Object.getOwnPropertyDescriptor(proto, "constructor");
          if (d !== undefined && typeof d.value === "function" && typeof d.value.name === "string" && d.value.name) {
            return d.value.name;
          }
        }
      } catch {
        /* exotic object: fall through */
      }
      return "Object";
    };
    const visit = (obj) => {
      let idx = indexOf.get(obj);
      if (idx === undefined && queue.length < kMaxSnapshotNodes) {
        idx = queue.length + 1;
        indexOf.set(obj, idx);
        queue.push(obj);
      }
      return idx;
    };
    const rootEdges = [];
    const g = visit(globalThis);
    if (g !== undefined) rootEdges.push([E_SHORTCUT, intern("global"), g]);

    // node 0 is the synthetic root, then the queue in discovery order.
    const nodeFields = 7;
    let nextId = 1;
    const emitNode = (type, name, selfSize, edgeCount) => {
      nodes.push(type, intern(name), nextId, selfSize, edgeCount, 0, 0);
      nextId += 2;
    };
    emitNode(T_SYNTHETIC, "", 0, rootEdges.length);
    for (const [type, name, to] of rootEdges) edges.push(type, name, to * nodeFields);
    for (let i = 0; i < queue.length; i++) {
      const obj = queue[i];
      const isFunction = typeof obj === "function";
      const isArray = Array.isArray(obj);
      let keys;
      try {
        keys = Reflect.ownKeys(obj);
      } catch {
        keys = [];
      }
      const mine = [];
      for (const key of keys) {
        if (mine.length >= kMaxEdgesPerNode) break;
        let d;
        try {
          d = Object.getOwnPropertyDescriptor(obj, key);
        } catch {
          continue;
        }
        const value = d && "value" in d ? d.value : undefined;
        if (value === null || (typeof value !== "object" && typeof value !== "function")) continue;
        const to = visit(value);
        if (to === undefined) continue;
        const element = isArray && typeof key === "string" && /^\d+$/.test(key);
        mine.push([element ? E_ELEMENT : E_PROPERTY, element ? Number(key) : intern(typeof key === "symbol" ? key.toString() : key), to]);
      }
      let proto = null;
      try {
        proto = Object.getPrototypeOf(obj);
      } catch {
        /* proxy without a prototype trap */
      }
      if (proto !== null) {
        const to = visit(proto);
        if (to !== undefined) mine.push([E_INTERNAL, intern("__proto__"), to]);
      }
      const name = isFunction ? (typeof obj.name === "string" ? obj.name : "") : classNameOf(obj);
      emitNode(isFunction ? T_CLOSURE : isArray ? T_ARRAY : T_OBJECT, name, 16 + 8 * mine.length, mine.length);
      for (const [type, nameOrIndex, to] of mine) edges.push(type, nameOrIndex, to * nodeFields);
    }
    const nodeCount = nodes.length / nodeFields;
    return {
      snapshot: {
        meta: {
          node_fields: ["type", "name", "id", "self_size", "edge_count", "trace_node_id", "detachedness"],
          node_types: [NODE_TYPES, "string", "number", "number", "number", "number", "number"],
          edge_fields: ["type", "name_or_index", "to_node"],
          edge_types: [EDGE_TYPES, "string_or_number", "node"],
          trace_function_info_fields: ["function_id", "name", "script_name", "script_id", "line", "column"],
          trace_node_fields: ["id", "function_info_index", "count", "size", "children"],
          sample_fields: ["timestamp_us", "last_assigned_id"],
          location_fields: ["object_index", "script_id", "line", "column"],
        },
        node_count: nodeCount,
        edge_count: edges.length / 3,
        trace_function_count: 0,
      },
      nodes,
      edges,
      trace_function_infos: [],
      trace_tree: [],
      samples: [],
      locations: [],
      strings,
    };
  }

  // The snapshot as JSON text pieces, so a stream can hand them out without one giant string.
  function* heapSnapshotChunks() {
    const snap = buildHeapSnapshot();
    yield `{"snapshot":${JSON.stringify(snap.snapshot)},\n"nodes":[`;
    const rows = (array, width) => {
      const out = [];
      for (let i = 0; i < array.length; i += width * 1024) out.push(array.slice(i, i + width * 1024).join(","));
      return out;
    };
    const emitList = function* (array, width) {
      const pieces = rows(array, width);
      for (let i = 0; i < pieces.length; i++) yield (i === 0 ? "" : ",") + pieces[i];
    };
    yield* emitList(snap.nodes, 7);
    yield `],\n"edges":[`;
    yield* emitList(snap.edges, 3);
    yield `],\n"trace_function_infos":[],\n"trace_tree":[],\n"samples":[],\n"locations":[],\n"strings":[`;
    for (let i = 0; i < snap.strings.length; i += 1024) {
      const part = snap.strings.slice(i, i + 1024).map((s) => JSON.stringify(s)).join(",");
      yield (i === 0 ? "" : ",") + part;
    }
    yield "]}\n";
  }

  function getHeapSnapshot(options) {
    if (options !== undefined && (typeof options !== "object" || options === null)) {
      throw new __errors.ERR_INVALID_ARG_TYPE("options", "Object", options);
    }
    const { Readable } = __builtins.get("stream");
    const chunks = heapSnapshotChunks();
    return new Readable({
      read() {
        const next = chunks.next();
        this.push(next.done ? null : next.value);
      },
    });
  }

  let snapshotCounter = 0;
  function writeHeapSnapshot(filename, options) {
    if (filename !== undefined) {
      if (typeof filename !== "string" && !(filename instanceof Uint8Array)) {
        throw new __errors.ERR_INVALID_ARG_TYPE("path", ["string", "Buffer", "URL"], filename);
      }
    } else {
      const d = new Date();
      const pad = (n, w = 2) => String(n).padStart(w, "0");
      filename = `Heap.${d.getFullYear()}${pad(d.getMonth() + 1)}${pad(d.getDate())}.${pad(d.getHours())}${pad(d.getMinutes())}${pad(d.getSeconds())}.${process.pid}.0.${pad(++snapshotCounter, 3)}.heapsnapshot`;
    }
    if (options !== undefined && (typeof options !== "object" || options === null)) {
      throw new __errors.ERR_INVALID_ARG_TYPE("options", "Object", options);
    }
    const fs = __builtins.get("fs");
    const fd = fs.openSync(filename, "w");
    try {
      for (const chunk of heapSnapshotChunks()) fs.writeSync(fd, chunk);
    } finally {
      fs.closeSync(fd);
    }
    return filename;
  }

  const notSupported = (what) => () => {
    throw new Error(`node:v8 ${what} is not supported in lumen`);
  };

  // promiseHooks: lumen has no promise-lifecycle hook plumbing. Shaped like Node's; each registrar
  // is a no-op that returns the standard "stop" function.
  const noopStop = () => {};
  const promiseHooks = {
    createHook: () => noopStop,
    onInit: () => noopStop,
    onBefore: () => noopStop,
    onAfter: () => noopStop,
    onSettled: () => noopStop,
  };

  // startupSnapshot: lumen has no heap serializer. A snapshot blob names the entry script; running
  // from a blob replays that script (stdout/stderr muted) through the serialize and deserialize
  // callbacks, then hands over to the deserialize main function or the requested script. The CLI
  // drives this through the `lumen.snapshotControl` hooks.
  const snapshotState = {
    building: !!process[Symbol.for("lumen.options")]?.["--build-snapshot"],
    serialize: [],
    deserialize: [],
    main: undefined,
    mainData: undefined,
    muted: null,
  };
  const requireBuilding = () => {
    if (!snapshotState.building) throw new __errors.ERR_NOT_BUILDING_SNAPSHOT();
  };
  const addCallback = (list, callback, data) => {
    requireBuilding();
    if (typeof callback !== "function") throw new __errors.ERR_INVALID_ARG_TYPE("callback", "Function", callback);
    list.push([callback, data]);
  };
  const runCallbacks = (list) => {
    for (const [callback, data] of list.splice(0)) callback(data);
  };
  const startupSnapshot = {
    addDeserializeCallback: (callback, data) => addCallback(snapshotState.deserialize, callback, data),
    addSerializeCallback: (callback, data) => addCallback(snapshotState.serialize, callback, data),
    setDeserializeMainFunction(callback, data) {
      requireBuilding();
      if (typeof callback !== "function") throw new __errors.ERR_INVALID_ARG_TYPE("callback", "Function", callback);
      if (snapshotState.main !== undefined) throw new __errors.ERR_DUPLICATE_STARTUP_SNAPSHOT_MAIN_FUNCTION();
      snapshotState.main = callback;
      snapshotState.mainData = data;
    },
    isBuildingSnapshot: () => snapshotState.building,
  };
  Object.defineProperty(startupSnapshot, Symbol.for("lumen.snapshotControl"), {
    __proto__: null,
    value: {
      beginReplay() {
        snapshotState.building = true;
        const mute = () => true;
        snapshotState.muted = [process.stdout.write, process.stderr.write];
        process.stdout.write = mute;
        process.stderr.write = mute;
      },
      endBuild() {
        runCallbacks(snapshotState.serialize);
        snapshotState.building = false;
      },
      endReplay() {
        runCallbacks(snapshotState.serialize);
        snapshotState.building = false;
        [process.stdout.write, process.stderr.write] = snapshotState.muted;
        runCallbacks(snapshotState.deserialize);
        return snapshotState.main !== undefined;
      },
      runMain(args) {
        process.argv = [process.execPath, ...args];
        const { main, mainData } = snapshotState;
        return main(mainData);
      },
    },
  });

  __builtins.set("v8", {
    serialize,
    deserialize,
    Serializer,
    Deserializer,
    DefaultSerializer,
    DefaultDeserializer,
    getHeapStatistics,
    getHeapSpaceStatistics,
    getHeapCodeStatistics,
    getCppHeapStatistics,
    isStringOneByteRepresentation,
    cachedDataVersionTag,
    GCProfiler,
    promiseHooks,
    startupSnapshot,
    setFlagsFromString,
    // No snapshot-on-near-heap-limit mechanism exists here; registering a limit is a no-op.
    setHeapSnapshotNearHeapLimit: () => {},
    // Coverage collection is not wired up (no NODE_V8_COVERAGE sink); these are the inert no-ops
    // Node itself uses when coverage is disabled.
    takeCoverage: () => {},
    stopCoverage: () => {},
    // Heap introspection (walking live objects / writing .heapsnapshot) is unbackable without V8.
    queryObjects: notSupported("queryObjects"),
    getHeapSnapshot,
    writeHeapSnapshot,
  });
});

// ---- node:inspector (and node:inspector/promises) ---------------------------------------------
// No V8 inspector is attached; the correct state is "inert", not a pretend session. Session.post
// reports the unavailability honestly (callback error / rejected promise); the module shape
// otherwise mirrors Node's (open/close/url/waitForDebugger, the Network domain, `console`).
__lazyGlue(__glueIndex, "inspector inspector/promises", "", "", () => {
  const noop = () => {};
  const unavailable = () => new Error("node:inspector is not available in lumen");
  class Session {
    connect() {}
    connectToMainThread() {}
    disconnect() {}
    post(_method, _params, callback) {
      const cb = typeof _params === "function" ? _params : callback;
      if (cb) cb(unavailable());
    }
    on() { return this; }
    once() { return this; }
    removeListener() { return this; }
    emit() { return false; }
  }
  // The Network inspector domain (Node ≥ 22): reporting hooks that no-op without an inspector.
  const Network = {
    requestWillBeSent: noop,
    responseReceived: noop,
    loadingFinished: noop,
    loadingFailed: noop,
  };
  const base = {
    open: noop,
    close: noop,
    url: () => undefined,
    waitForDebugger: noop,
    console: globalThis.console,
    Network,
  };
  __builtins.set("inspector", { ...base, Session });

  // The promises variant: identical surface, but Session.post returns a Promise.
  class SessionPromises extends Session {
    post(_method, _params) {
      return Promise.reject(unavailable());
    }
  }
  __builtins.set("inspector/promises", { ...base, Session: SessionPromises });
});

// ---- node:sys ---------------------------------------------------------------------------------
// The long-deprecated alias for node:util — the *same* object, exactly as Node's `sys` is.
// Registered to load on first use, like node:util itself (see preamble.js `__lazyGlue`).
__lazyGlue(__glueIndex, "sys", "", "", () => __builtins.set("sys", __builtins.get("util")));

// ---- node:stream/consumers --------------------------------------------------------------------
// Node's lib/stream/consumers.js: fully consume a stream / async-iterable into a single value.
__lazyGlue(__glueIndex, "stream/consumers", "", "", () => {
  async function blob(stream) {
    const chunks = [];
    for await (const chunk of stream) chunks.push(chunk);
    return new Blob(chunks);
  }
  async function arrayBuffer(stream) {
    const ret = await blob(stream);
    return ret.arrayBuffer();
  }
  async function buffer(stream) {
    return Buffer.from(await arrayBuffer(stream));
  }
  async function text(stream) {
    const dec = new TextDecoder();
    let str = "";
    for await (const chunk of stream) {
      if (typeof chunk === "string") str += chunk;
      else str += dec.decode(chunk, { stream: true });
    }
    // Flush the streaming TextDecoder so that any pending incomplete multibyte characters are
    // handled.
    str += dec.decode(undefined, { stream: false });
    return str;
  }
  async function json(stream) {
    const str = await text(stream);
    return JSON.parse(str);
  }
  __builtins.set("stream/consumers", { arrayBuffer, blob, buffer, json, text });
});

// ---- node:process ----------------------------------------------------------------------------
// Node's `process` is an EventEmitter (SIGINT/exit/beforeExit/…). lumen builds `process` in Rust
// without that surface, so the emitter methods are mixed in here. Signals never fire (no handler
// plumbing), but registering/removing listeners no longer throws, which is what tools rely on.
// The methods start as stubs: `events` loads, and the default 'warning' listener registers, the
// first time a program uses the emitter. Until then the process holds exactly that one listener,
// so `emit` of anything else is false and `listenerCount` is 0 without loading anything.
{
  const proc = globalThis.process;
  const EMITTER_METHODS = [
    "on", "off", "once", "emit", "addListener", "removeListener", "removeAllListeners",
    "prependListener", "prependOnceListener", "listeners", "rawListeners", "listenerCount",
    "eventNames", "setMaxListeners", "getMaxListeners",
  ];
  const BOOKKEEPING = ["_events", "_eventsCount", "_maxListeners"];
  let EventEmitter = null;
  let defaultWarningListener;
  const stubs = new Map();
  const materialize = () => {
    if (EventEmitter !== null) return;
    EventEmitter = __builtins.get("events");
    // EventEmitter bookkeeping fields Node exposes as own keys (methods lazily init these too).
    Object.defineProperty(proc, "_events", { value: Object.create(null), writable: true, enumerable: true, configurable: true });
    Object.defineProperty(proc, "_eventsCount", { value: 0, writable: true, enumerable: true, configurable: true });
    Object.defineProperty(proc, "_maxListeners", { value: undefined, writable: true, enumerable: true, configurable: true });
    for (const [m, stub] of stubs) if (proc[m] === stub) proc[m] = EventEmitter.prototype[m];
    EventEmitter.prototype.on.call(proc, "warning", defaultWarningListener);
  };
  const quiet = (self, type) => EventEmitter === null && self === proc && type !== "warning" && type !== "error";
  for (const m of EMITTER_METHODS) {
    if (typeof proc[m] === "function") continue;
    const stub = ({
      [m]: function (...args) {
        if (m === "emit" && quiet(this, args[0])) return false;
        if (m === "listenerCount" && quiet(this, args[0])) return 0;
        materialize();
        return Reflect.apply(EventEmitter.prototype[m], this, args);
      },
    })[m];
    stubs.set(m, stub);
    proc[m] = stub;
  }
  if (proc._events === undefined) {
    for (const key of BOOKKEEPING) {
      Object.defineProperty(proc, key, {
        get() { materialize(); return proc[key]; },
        set(value) { materialize(); proc[key] = value; },
        enumerable: true,
        configurable: true,
      });
    }
  }

  // ---- fuller node:process surface ----------------------------------------------------------
  // The Rust layer (lumen-runtime/process.rs) already supplies argv/env/platform/pid/arch,
  // cwd/exit/nextTick, stdout/stderr, hrtime/uptime, and the real OS-identity calls (uid/gid/ppid,
  // kill/umask/chdir/abort). Everything below is JS-expressible: derived facts, honest zero-data
  // metrics, and Node-shaped stubs for surfaces lumen can't back with real data. Nothing here
  // fabricates plausible-but-false numbers — unmeasured metrics report 0 / [] and unsupported
  // operations throw.

  // argv0/execPath/title are stamped by the Rust data-prop pass (which, unlike this glue, runs
  // after argv is populated). execArgv — the runtime flags before the script — is empty for lumen.
  proc.execArgv = [];

  // Plain data slots (all settable, matching Node).
  proc.exitCode = undefined;
  proc.debugPort = 9229;
  proc.domain = null;
  proc.moduleLoadList = [];
  // The subset of Node's build config that programs (and Node's test/common) probe.
  proc.config = {
    target_defaults: { default_configuration: "Release" },
    variables: {
      v8_enable_i18n_support: 1, icu_small: false, node_shared_openssl: false,
      openssl_is_fips: false, openssl_quic: false, node_module_version: 115, napi_build_version: "9",
      node_shared: false, node_use_openssl: true, asan: 0,
    },
  };
  proc.sourceMapsEnabled = false;
  proc.allowedNodeEnvironmentFlags = new Set();

  // cwd/exit/nextTick are defined by the Rust op layer as non-enumerable; Node exposes them as
  // own-enumerable keys, so re-stamp the descriptor (they are configurable).
  for (const k of ["cwd", "nextTick"]) {
    const d = Object.getOwnPropertyDescriptor(proc, k);
    if (d && !d.enumerable && d.configurable) {
      Object.defineProperty(proc, k, { value: proc[k], enumerable: true, configurable: true, writable: true });
    }
  }

  // nextTick callbacks run in the async context they were queued from (AsyncLocalStorage).
  const rawNextTick = proc.nextTick;
  proc.nextTick = function nextTick(callback, ...args) {
    if (typeof callback !== "function") {
      const err = new TypeError(`The "callback" argument must be of type function. Received ${callback === null ? "null" : typeof callback}`);
      err.code = "ERR_INVALID_ARG_TYPE";
      throw err;
    }
    return rawNextTick(__bindAsyncContext(callback, "TickObject", { callback, args }), ...args);
  };

  // exit() honors process.exitCode when called without an explicit code; reallyExit is the raw op.
  const nativeExit = proc.exit;
  proc.reallyExit = nativeExit;
  Object.defineProperty(proc, "exit", {
    value: function (code) {
      const c = code !== undefined && code !== null ? code
        : (proc.exitCode !== undefined && proc.exitCode !== null ? proc.exitCode : 0);
      return nativeExit(c);
    },
    enumerable: true, configurable: true, writable: true,
  });

  // Node's lib/internal/process/warning.js: emitWarning builds the warning and emits 'warning' on
  // the next tick; the default 'warning' listener (absent under --no-warnings/NODE_NO_WARNINGS=1)
  // prints it. --no-deprecation / --throw-deprecation / --trace-deprecation / --trace-warnings
  // are honored from execArgv.
  {
    // execArgv is stamped after the glue runs, so the flag-backed properties resolve on first read.
    const hasFlag = (f) => {
      const options = proc[Symbol.for("lumen.options")];
      if (options !== undefined) {
        return f.startsWith("--no-") ? options["--" + f.slice(5)] === false : options[f] === true;
      }
      return Array.isArray(proc.execArgv) && proc.execArgv.includes(f);
    };
    for (const [prop, flag] of [["noDeprecation", "--no-deprecation"], ["throwDeprecation", "--throw-deprecation"],
      ["traceDeprecation", "--trace-deprecation"], ["traceProcessWarnings", "--trace-warnings"]]) {
      Object.defineProperty(proc, prop, {
        configurable: true, enumerable: false,
        get() { return hasFlag(flag) ? true : undefined; },
        set(v) { Object.defineProperty(proc, prop, { value: v, writable: true, configurable: true, enumerable: true }); },
      });
    }
    let traceWarningHelperShown = false;
    const onWarning = function onWarning(warning) {
      if (!(warning instanceof Error)) return;
      const isDeprecation = warning.name === "DeprecationWarning";
      if (isDeprecation && proc.noDeprecation) return;
      const trace = proc.traceProcessWarnings || (isDeprecation && proc.traceDeprecation);
      let msg = `(node:${proc.pid}) `;
      if (warning.code) msg += `[${warning.code}] `;
      if (trace && warning.stack) {
        msg += `${warning.stack}`;
      } else {
        msg += typeof warning.toString === "function" ? `${warning.toString()}` : Error.prototype.toString.call(warning);
      }
      if (typeof warning.detail === "string") msg += `\n${warning.detail}`;
      if (!trace && !traceWarningHelperShown) {
        const flag = isDeprecation ? "--trace-deprecation" : "--trace-warnings";
        msg += `\n(Use \`node ${flag} ...\` to show where the warning was created)`;
        traceWarningHelperShown = true;
      }
      console.error(msg);
    };
    const createWarningObject = (warning, type, code, detail) => {
      const err = new Error(warning);
      err.name = String(type || "Warning");
      if (code !== undefined) err.code = code;
      if (detail !== undefined) err.detail = detail;
      return err;
    };
    proc.emitWarning = function emitWarning(warning, type, code, ctor) {
      let detail;
      if (type !== null && typeof type === "object" && !Array.isArray(type)) {
        ctor = type.ctor;
        code = type.code;
        if (typeof type.detail === "string") detail = type.detail;
        type = type.type || "Warning";
      } else if (typeof type === "function") {
        ctor = type;
        code = undefined;
        type = "Warning";
      }
      if (type !== undefined) __validators.validateString(type, "type");
      if (typeof code === "function") {
        ctor = code;
        code = undefined;
      } else if (code !== undefined) {
        __validators.validateString(code, "code");
      }
      if (typeof warning === "string") {
        warning = createWarningObject(warning, type, code, detail);
      } else if (!(warning instanceof Error)) {
        throw new __errors.ERR_INVALID_ARG_TYPE("warning", ["Error", "string"], warning);
      }
      if (warning.name === "DeprecationWarning") {
        if (proc.noDeprecation) return;
        if (proc.throwDeprecation) {
          // Delay throwing so that all former warnings are logged first.
          return proc.nextTick(() => { throw warning; });
        }
      }
      proc.nextTick(() => proc.emit("warning", warning));
    };
    // Anonymous, like the `proc.on("warning", function (warning) {…})` it stands for.
    defaultWarningListener = (0, function (warning) {
      if (hasFlag("--no-warnings") || (proc.env && proc.env.NODE_NO_WARNINGS === "1")) return;
      onWarning(warning);
    });
  }

  // Real: bridge to the builtin-module registry (getBuiltinModule('fs') === require('fs')).
  proc.getBuiltinModule = function (id) {
    const name = typeof id === "string" && id.startsWith("node:") ? id.slice(5) : id;
    return __builtins.has(name) ? __builtins.get(name) : undefined;
  };

  // Real: parse a .env file (default ".env") and assign into process.env. Throws if unreadable.
  proc.loadEnvFile = function (path) {
    const fs = __builtins.get("fs");
    const text = fs.readFileSync(path == null ? ".env" : path, "utf8");
    for (const rawLine of text.split(/\r?\n/)) {
      const line = rawLine.trim();
      if (!line || line[0] === "#") continue;
      const eq = line.indexOf("=");
      if (eq === -1) continue;
      const key = line.slice(0, eq).trim();
      if (!key) continue;
      let val = line.slice(eq + 1).trim();
      const q = val[0];
      if ((q === '"' || q === "'") && val[val.length - 1] === q) val = val.slice(1, -1);
      proc.env[key] = val;
    }
  };

  // Real: dlopen a native addon into module.exports via the N-API loader (dylib.rs / napi.rs).
  proc.dlopen = function (module, filename) {
    module.exports = globalThis.__node.loadNativeAddon(filename);
    return module.exports;
  };

  // Real state, no real source-map support yet: toggle the flag setSourceMapsEnabled reads back.
  proc.setSourceMapsEnabled = function (val) { proc.sourceMapsEnabled = !!val; };

  // Native OS process counters. Heap-specific fields stay zero until the engine exposes allocator
  // accounting, while RSS/CPU/resource counters are real getrusage(2) measurements.
  const metrics = () => proc._nativeMetrics();
  const memoryUsage = () => ({ rss: metrics()[0], heapTotal: 0, heapUsed: 0, external: 0, arrayBuffers: 0 });
  memoryUsage.rss = () => metrics()[0];
  proc.memoryUsage = memoryUsage;
  const identity = proc[Symbol.for("lumen.identity")];
  if (identity !== undefined) {
    const credential = (kind, name, value) => {
      if (typeof value === "number") {
        __validators.validateUint32(value, name);
        return value;
      }
      if (typeof value !== "string") throw new __errors.ERR_INVALID_ARG_TYPE(name, ["number", "string"], value);
      const id = kind === "User" ? identity.uidOf(value) : identity.gidOf(value);
      if (id === undefined) throw new __errors.ERR_UNKNOWN_CREDENTIAL(kind, value);
      return id;
    };
    const defineSetter = (name, kind) => {
      proc[name] = { [name](id) { return identity[name](credential(kind, "id", id)); } }[name];
    };
    defineSetter("setuid", "User");
    defineSetter("seteuid", "User");
    defineSetter("setgid", "Group");
    defineSetter("setegid", "Group");
    proc.setgroups = function setgroups(groups) {
      __validators.validateArray(groups, "groups");
      const ids = groups.map((group, i) => credential("Group", `groups[${i}]`, group));
      return identity.setgroups(ids.join(","));
    };
    proc.initgroups = function initgroups(user, extraGroup) {
      if (typeof user !== "number" && typeof user !== "string") {
        throw new __errors.ERR_INVALID_ARG_TYPE("user", ["number", "string"], user);
      }
      if (typeof extraGroup !== "number" && typeof extraGroup !== "string") {
        throw new __errors.ERR_INVALID_ARG_TYPE("extraGroup", ["number", "string"], extraGroup);
      }
      const gid = credential("Group", "extraGroup", extraGroup);
      let userName = user;
      if (typeof user === "number") {
        userName = identity.userNameOf(user);
        if (userName === undefined) throw new __errors.ERR_UNKNOWN_CREDENTIAL("User", String(user));
      } else if (identity.uidOf(user) === undefined) {
        throw new __errors.ERR_UNKNOWN_CREDENTIAL("User", user);
      }
      return identity.initgroups(userName, gid);
    };
  }
  proc.availableMemory = () => metrics()[14];
  proc.constrainedMemory = () => metrics()[15] || 0;
  proc.cpuUsage = (previous) => {
    const m = metrics();
    const current = { user: m[2], system: m[3] };
    return previous ? { user: current.user - previous.user, system: current.system - previous.system } : current;
  };
  proc.resourceUsage = () => {
    const m = metrics();
    return {
      userCPUTime: m[2], systemCPUTime: m[3], maxRSS: m[1], sharedMemorySize: 0,
      unsharedDataSize: 0, unsharedStackSize: 0, minorPageFault: m[4], majorPageFault: m[5],
      swappedOut: m[6], fsRead: m[7], fsWrite: m[8], ipcSent: m[9], ipcReceived: m[10],
      signalsCount: m[11], voluntaryContextSwitches: m[12], involuntaryContextSwitches: m[13],
    };
  };
  proc.getActiveResourcesInfo = () => [];

  // Honest build-feature booleans (false where lumen genuinely lacks the capability, e.g. tls).
  proc.features = {
    inspector: false, debug: false, uv: false, ipv6: true,
    tls_alpn: false, tls_sni: false, tls_ocsp: false, tls: false, cached_builtins: true,
  };

  proc.release = { name: "node", lts: undefined, sourceUrl: "", headersUrl: "" };

  // Diagnostic reports with live process/runtime data. Native stacks and libuv handles are not
  // available, but the report is useful and writable rather than an empty compatibility shell.
  const report = {
    compact: false, directory: "", filename: "", signal: "SIGUSR2",
    reportOnFatalError: false, reportOnSignal: false, reportOnUncaughtException: false,
    excludeEnv: false, excludeNetwork: false,
    getReport(error) {
      const now = new Date();
      const stack = error && error.stack ? String(error.stack).split("\n") : [];
      return {
        header: {
          reportVersion: 5, event: error ? "Exception" : "JavaScript API", trigger: "GetReport",
          filename: null, dumpEventTime: now.toISOString(), dumpEventTimeStamp: now.getTime(),
          processId: proc.pid, cwd: proc.cwd(), commandLine: proc.argv.slice(),
          nodejsVersion: proc.version, wordSize: proc.arch.includes("64") || proc.arch === "arm64" ? 64 : 32,
          arch: proc.arch, platform: proc.platform, componentVersions: { ...proc.versions },
        },
        javascriptStack: { message: error ? String(error) : "No stack.", stack, errorProperties: {} },
        javascriptHeap: proc.memoryUsage(), resourceUsage: proc.resourceUsage(),
        environmentVariables: report.excludeEnv ? {} : { ...proc.env },
        libuv: [], userLimits: {}, sharedObjects: [], workers: [],
      };
    },
    writeReport(filename, error) {
      if (filename instanceof Error && error === undefined) { error = filename; filename = undefined; }
      let target = filename || report.filename;
      if (!target) {
        const stamp = new Date().toISOString().replace(/[-:TZ.]/g, "").slice(0, 14);
        target = `report.${stamp}.${proc.pid}.0.001.json`;
      }
      if (report.directory && !String(target).includes("/") && !String(target).includes("\\")) {
        target = __builtins.get("path").join(report.directory, String(target));
      }
      const data = report.getReport(error);
      data.header.filename = String(target);
      const json = JSON.stringify(data, null, report.compact ? 0 : 2);
      __builtins.get("fs").writeFileSync(target, json + (report.compact ? "" : "\n"));
      return String(target);
    },
  };
  proc.report = report;

  const finalizationRecords = new WeakMap();
  const finalizer = new FinalizationRegistry((record) => {
    if (record.active) record.callback(record.reference.deref(), "exit");
  });
  const registerFinalizer = (ref, callback, beforeExit) => {
    if ((typeof ref !== "object" && typeof ref !== "function") || ref === null) {
      throw new TypeError('The "ref" argument must be an object');
    }
    if (typeof callback !== "function") throw new TypeError('The "callback" argument must be a function');
    const old = finalizationRecords.get(ref);
    if (old) old.active = false;
    const record = { active: true, callback, reference: new WeakRef(ref) };
    finalizationRecords.set(ref, record);
    finalizer.register(ref, record, ref);
    if (beforeExit) proc.once("beforeExit", () => {
      if (record.active) callback(record.reference.deref(), "beforeExit");
    });
  };
  proc.finalization = {
    register: (ref, callback) => registerFinalizer(ref, callback, false),
    registerBeforeExit: (ref, callback) => registerFinalizer(ref, callback, true),
    unregister(ref) {
      const record = finalizationRecords.get(ref);
      if (!record) return false;
      record.active = false;
      finalizationRecords.delete(ref);
      return finalizer.unregister(ref);
    },
  };

  // Modern Node throws for internal bindings; lumen exposes none.
  const noBinding = function (name) {
    throw new Error("process.binding('" + name + "') is not supported in lumen");
  };
  // process.binding(): Node 20 still serves an allowlist of internal bindings (DEP0111, a warning
  // only under --pending-deprecation). The ones net.js implements are served from there (loading
  // node:net on first use).
  const servedBindings = ["http_parser", "tcp_wrap", "pipe_wrap", "stream_wrap", "uv"];
  proc.binding = function binding(name) {
    name = `${name}`;
    if (servedBindings.includes(name)) return __internals.get("internalBinding")(name);
    return noBinding.call(this, name);
  };
  proc._linkedBinding = noBinding;

  // Not supported: replacing the process image / changing OS identity can't be done honestly
  // without the underlying syscall, and silently "succeeding" would misrepresent the result.

  let uncaughtCb = null;
  proc.setUncaughtExceptionCaptureCallback = function (cb) {
    if (cb === null) {
      uncaughtCb = null;
      return;
    }
    if (typeof cb !== "function") {
      throw new __errors.ERR_INVALID_ARG_TYPE("fn", ["Function", "null"], cb);
    }
    const Events = __builtins.get("events");
    if (Events.usingDomains) {
      const err = new __errors.ERR_DOMAIN_CANNOT_SET_UNCAUGHT_EXCEPTION_CAPTURE();
      const loadedAt = Events[Symbol.for("lumen.domainRequireStack")];
      if (typeof loadedAt === "string") err.stack = `${err.stack}\n${"-".repeat(40)}\n${loadedAt}`;
      throw err;
    }
    if (uncaughtCb !== null) {
      throw new __errors.ERR_UNCAUGHT_EXCEPTION_CAPTURE_ALREADY_SET();
    }
    uncaughtCb = cb;
  };
  // The runtime's uncaught-error path asks this first (node:domain replaces it, chaining here).
  Object.defineProperty(globalThis, "__lumen_domain_uncaught", {
    value: (err) => {
      if (uncaughtCb === null) return false;
      uncaughtCb(err);
      return true;
    },
    configurable: true, writable: true, enumerable: false,
  });
  proc._fatalException = function _fatalException(err) {
    if (globalThis.__lumen_domain_uncaught(err)) return true;
    if (proc.listenerCount("uncaughtException") > 0) {
      proc.emit("uncaughtException", err, "uncaughtException");
      return true;
    }
    return false;
  };
  proc.hasUncaughtExceptionCaptureCallback = () => uncaughtCb !== null;

  // Real: the deprecated process.assert.
  proc.assert = function (value, message) {
    if (!value) throw new Error("assertion failed" + (message ? ": " + message : ""));
  };

  // No-ops: process-level ref/unref have no handle to keep the loop alive here.
  proc.ref = () => {};
  proc.unref = () => {};

  // ---- process.stdin / stdout / stderr ----------------------------------------------------------
  // stdin: a Readable over the runtime's non-blocking stdin op. Each `_read` issues one native
  // read, so reading follows the consumer (backpressure; nothing is read until someone asks),
  // and an unused stdin never holds the event loop open. The read in flight cannot be
  // cancelled, so a paused or destroyed stdin unrefs it instead of waiting for the writer to
  // close the pipe (Node lets the process exit in both cases).
  // Created on first access (as Node's is), so a script that never reads stdin does not load
  // node:stream at startup.
  let stdinStream;
  const getStdin = () => (stdinStream ??= createStdin());
  function createStdin() {
  const stdinIsTTY = typeof proc._isatty === "function" && proc._isatty(0);
  const stdin = new (__builtins.get("stream").Readable)({ highWaterMark: 64 * 1024 });
  {
    let pending = null;
    let wanted = true;
    let reading = false;
    const setRef = (keep) => {
      wanted = keep;
      if (pending !== null) proc._stdinRef(pending, keep);
    };
    stdin._read = function () {
      if (reading) return;
      reading = true;
      new Promise((resolve, reject) => {
        pending = proc._readStdin(resolve, reject);
        if (!wanted) proc._stdinRef(pending, false);
      }).then(
        (chunk) => {
          pending = null;
          reading = false;
          if (chunk === null) stdin.push(null);
          else stdin.push(Buffer.from(chunk));
        },
        (error) => {
          pending = null;
          reading = false;
          stdin.destroy(error);
        },
      );
    };
    const resume = stdin.resume;
    stdin.resume = function () {
      setRef(true);
      return resume.call(this);
    };
    const pause = stdin.pause;
    stdin.pause = function () {
      setRef(false);
      return pause.call(this);
    };
    stdin._destroy = function (error, callback) {
      setRef(false);
      callback(error);
    };
    stdin.ref = function () { setRef(true); return this; };
    stdin.unref = function () { setRef(false); return this; };
  }
  if (stdinIsTTY) {
    stdin.isTTY = true;
    stdin.isRaw = false;
    stdin.setRawMode = function (mode) { this.isRaw = !!mode; return this; };
  }
  stdin.fd = 0;
  return stdin;
  }
  Object.defineProperty(proc, "stdin", {
    enumerable: true, configurable: true,
    get: getStdin,
  });
  proc.openStdin = function () { const stdin = getStdin(); stdin.resume(); return stdin; };

  // stdout / stderr: Writables over the runtime's synchronous sinks (which honour an embedder's
  // redirection) — tty.WriteStream on a terminal, else Node's SyncWriteStream shape. Created on
  // first access, as Node's are. `destroy()` only ends the JS stream: fd 1/2 stay open.
  const rawStdio = { 1: proc.stdout, 2: proc.stderr };
  __internals.set("stdio_raw", rawStdio);
  // Built on first use: the stream module loads only when a stdio stream is created.
  let SyncWriteStreamClass;
  const syncWriteStream = () => (SyncWriteStreamClass ??= class SyncWriteStream extends __builtins.get("stream").Writable {
    constructor(fd, raw) {
      super({ autoDestroy: true, decodeStrings: false });
      this.fd = fd;
      this._raw = raw;
      this.readable = false;
    }
    _write(chunk, encoding, cb) {
      try {
        this._raw.write(typeof chunk === "string" && encoding !== "utf8" && encoding !== "utf-8" ? Buffer.from(chunk, encoding) : chunk);
      } catch (e) {
        if (e && e.code === "EPIPE" && e.errno === undefined) e.errno = __uvCodes().get("EPIPE");
        cb(e);
        return;
      }
      cb();
    }
  });
  function dummyDestroy(err, cb) {
    cb(err);
    this._undestroy();
    if (!this._writableState.emitClose) process.nextTick(() => this.emit("close"));
  }
  function createWritableStdioStream(fd) {
    __internals.get("console_native").setLive(fd);
    const isTTY = proc._isatty && proc._isatty(fd);
    const tty = isTTY ? __builtins.get("tty") : undefined;
    const stream = tty
      ? new tty.WriteStream(fd)
      : new (syncWriteStream())(fd, rawStdio[fd]);
    stream.fd = fd;
    stream._type = stream.isTTY ? "tty" : "fs";
    stream._isStdio = true;
    stream.destroySoon = stream.destroy;
    stream._destroy = dummyDestroy;
    return stream;
  }
  let stdoutStream, stderrStream;
  Object.defineProperty(proc, "stdout", {
    configurable: true, enumerable: true,
    get() { return stdoutStream ??= createWritableStdioStream(1); },
  });
  Object.defineProperty(proc, "stderr", {
    configurable: true, enumerable: true,
    get() { return stderrStream ??= createWritableStdioStream(2); },
  });
  __internals.get("console_native").watch(proc,
    Object.getOwnPropertyDescriptor(proc, "stdout").get, Object.getOwnPropertyDescriptor(proc, "stderr").get);

  // child_process.fork IPC. Lumen has no extra inherited fd, so fork reserves stdin and frames
  // messages with a control-prefixed JSON line; ordinary stdout lines remain ordinary stdout.
  const IPC_PREFIX = "\x1eLUMEN_IPC ";
  let forkIpcStarted = false;
  let forkConnected = false;
  let forkDisconnected = false;
  let forkPending = "";
  const isForkChild = () => proc.env && proc.env.LUMEN_FORK_IPC === "1";
  let dedicatedIpc;
  function openDedicatedIpc(fd) {
    const __ipcWire=__builtins.get("child_process")._ipcWire;
    __child.ipcOpen(fd);
    const decoder = new (__builtins.get("string_decoder").StringDecoder)("utf8");
    let pending = "", queuedBytes = 0, closed = false, ending = false;
    const queue = [];
    const updateRef = () => {
      if (closed) return;
      if (queue.length || forkConnected && (proc.listenerCount("message") || proc.listenerCount("disconnect"))) timer.ref(); else timer.unref();
    };
    function close(error) {
      if (closed) return;
      closed = true; clearInterval(timer); forkConnected = false; forkDisconnected = true;
      try { __child.ipcClose(fd); } catch {}
      for (const item of queue.splice(0)) if (item.callback) item.callback(error || new Error("IPC channel closed"));
      queuedBytes = 0;
      process.nextTick(() => {
        proc.emit("disconnect");
        // A cluster worker whose primary went away exits, as in the stdin-backed channel below.
        if ((proc._lumenClusterWorker || (proc.env && proc.env.LUMEN_CLUSTER_WORKER === "1")) && !proc._clusterExitedAfterDisconnect) proc.exit(0);
      });
    }
    function flush() {
      let writes=0;
      while (queue.length && writes++ < 16) {
        const item=queue[0], count=__child.ipcWrite(fd,item.bytes.subarray(item.offset,item.offset+65536));
        if (!count) break;
        item.offset+=count;queuedBytes-=count;
        if (item.offset===item.bytes.length) {queue.shift();if(item.callback) queueMicrotask(()=>item.callback(null));}
      }
      if (ending && queue.length===0) close(); else updateRef();
    }
    function send(bytes, callback) {
      if (closed || ending) { if(callback) queueMicrotask(()=>callback(new Error("IPC channel closed"))); return false; }
      if (queuedBytes+bytes.length > __ipcWire.limit) throw new RangeError("IPC queued messages exceed 8 MiB");
      queue.push({bytes,offset:0,callback});queuedBytes+=bytes.length;
      try { flush(); } catch(error) {close(error);}
      return queuedBytes < 65536;
    }
    const timer=setInterval(()=>{
      try {
        flush(); if(closed) return;
        for(let reads=0;reads<16;reads++) {
          const chunk=__child.ipcRead(fd);if(chunk===null) break;
          if(chunk.length===0) {close();break;}
          pending+=decoder.write(Buffer.from(chunk));
          if(pending.length > __ipcWire.limit) throw new RangeError("IPC frame exceeds 8 MiB");
          for(;;) {
            const end=pending.indexOf("\n");if(end<0) break;
            const line=pending.slice(0,end);pending=pending.slice(end+1);
            if(line==="\x1eLUMEN_IPC_DISCONNECT") {close();return;}
            const message = __ipcWire.decode(line);
            if (message !== null && typeof message === "object" && message.__lumenCluster !== undefined) proc.emit("internalMessage", message);
            else proc.emit("message", message, null);
          }
        }
      } catch(error) {close(error);}
    },1);
    timer.unref();
    return {updateRef,send,disconnect(){send(Buffer.from("\x1eLUMEN_IPC_DISCONNECT\n"));ending=true;flush()}};
  }
  // Node's channel ref counting: the IPC channel keeps the child alive only while it is
  // connected and 'message' / 'disconnect' listeners exist, so a child that only sends exits.
  const updateForkRef = () => {
    if (!forkIpcStarted) return;
    if (dedicatedIpc) {dedicatedIpc.updateRef();return;}
    const keep = forkConnected && (proc.listenerCount("message") > 0 || proc.listenerCount("disconnect") > 0);
    if (keep) getStdin().ref(); else getStdin().unref();
  };
  const startForkIpc = () => {
    if (forkIpcStarted || forkDisconnected || !isForkChild()) return;
    forkIpcStarted = true;
    forkConnected = true;
    const ipcFd = Number(proc.env.LUMEN_FORK_IPC_FD);
    if (Number.isInteger(ipcFd) && ipcFd >= 3) {dedicatedIpc=openDedicatedIpc(ipcFd);return;}
    getStdin().on("data", (chunk) => {
      forkPending += Buffer.from(chunk).toString("utf8");
      for (;;) {
        const newline = forkPending.indexOf("\n");
        if (newline < 0) break;
        const line = forkPending.slice(0, newline);
        forkPending = forkPending.slice(newline + 1);
        if (!line.startsWith(IPC_PREFIX)) continue;
        try {
          const message = JSON.parse(line.slice(IPC_PREFIX.length));
          // cluster's own control messages never reach user 'message' listeners.
          if (message !== null && typeof message === "object" && message.__lumenCluster !== undefined) proc.emit("internalMessage", message);
          else proc.emit("message", message, null);
        } catch (error) {
          proc.emit("error", error);
        }
      }
    });
    getStdin().on("end", () => {
      if (!forkConnected) return;
      forkConnected = false;
      updateForkRef();
      proc.emit("disconnect");
      // A cluster worker whose primary went away exits (Node's cluster child does the same), so
      // a primary that dies never leaves orphaned workers running. A disconnect the primary asked
      // for (worker.disconnect()) is different: the worker then exits under normal loop rules.
      if ((proc._lumenClusterWorker || (proc.env && proc.env.LUMEN_CLUSTER_WORKER === "1")) && !proc._clusterExitedAfterDisconnect) proc.exit(0);
    });
    updateForkRef();
  };
  const forkSend = (message, sendHandle, options, callback) => {
    if (typeof sendHandle === "function") { callback = sendHandle; sendHandle = undefined; options = undefined; }
    else if (typeof options === "function") { callback = options; options = undefined; }
    else if (options !== undefined) __validators.validateObject(options, "options");
    startForkIpc();
    if (!forkConnected) {
      const ex = new __errors.ERR_IPC_CHANNEL_CLOSED();
      if (typeof callback === "function") process.nextTick(callback, ex);
      else process.nextTick(() => proc.emit("error", ex));
      return false;
    }
    __builtins.get("child_process")._ipcWire.validate(message);
    if (sendHandle != null) {
      const error = new Error("child_process.fork handle transfer is not supported in lumen");
      if (typeof callback === "function") process.nextTick(callback, error);
      else process.nextTick(() => proc.emit("error", error));
      return false;
    }
    try {
      if (dedicatedIpc) return dedicatedIpc.send(__builtins.get("child_process")._ipcWire.encode(message,proc.env.LUMEN_FORK_IPC_MODE === "advanced"),callback);
      proc.stdout.write(IPC_PREFIX + JSON.stringify(message === undefined ? null : message) + "\n");
      if (callback) queueMicrotask(() => callback(null));
      return true;
    } catch (error) {
      if (callback) queueMicrotask(() => callback(error)); else throw error;
      return false;
    }
  };
  Object.defineProperty(proc, "send", {
    enumerable: true,
    configurable: true,
    get() {
      if (!isForkChild()) return undefined;
      startForkIpc();
      return forkSend;
    },
  });
  Object.defineProperty(proc, "connected", {
    enumerable: true,
    configurable: true,
    get() { return isForkChild() ? forkConnected : undefined; },
  });
  proc.disconnect = function () {
    if (!isForkChild()) return;
    if (forkDisconnected || !forkConnected) {
      proc.emit("error", new __errors.ERR_IPC_DISCONNECTED());
      return;
    }
    if (dedicatedIpc) {dedicatedIpc.disconnect();return;}
    forkDisconnected = true;
    forkConnected = false;
    updateForkRef();
    try { proc.stdout.write("\x1eLUMEN_IPC_DISCONNECT\n"); } catch {}
    queueMicrotask(() => proc.emit("disconnect"));
  };
  const processOn = proc.on;
  proc.on = proc.addListener = function (event, listener) {
    if (event === "message" || event === "disconnect") startForkIpc();
    const result = processOn.call(this, event, listener);
    if (event === "message" || event === "disconnect") updateForkRef();
    return result;
  };
  const processOnce = proc.once;
  proc.once = function (event, listener) {
    if (event === "message" || event === "disconnect") startForkIpc();
    const result = processOnce.call(this, event, listener);
    if (event === "message" || event === "disconnect") updateForkRef();
    return result;
  };
  const processOff = proc.removeListener;
  proc.removeListener = proc.off = function (event, listener) {
    const result = processOff.call(this, event, listener);
    if (event === "message" || event === "disconnect") updateForkRef();
    return result;
  };

  // Signals reach an embedded realm only from its launcher (a parent realm's `child.kill()` or
  // `process.kill(childPid)`): the realm records which signals have listeners, and `deliver`
  // emits one when the launcher sends it. The listener methods are wrapped rather than observed
  // through `newListener`, which would load `events` at startup.
  {
    const setHandler = proc[Symbol.for("lumen.signalHandler")];
    if (typeof setHandler === "function" && setHandler(0, false) === true) {
      // `process.platform` is stamped after this glue runs, so the tables are built on first use.
      let table;
      const signals = () => {
        if (table === undefined) {
          const linux = process.platform !== "darwin";
          const numbers = { SIGHUP: 1, SIGINT: 2, SIGQUIT: 3, SIGALRM: 14, SIGTERM: 15, SIGWINCH: 28,
            SIGUSR1: linux ? 10 : 30, SIGUSR2: linux ? 12 : 31, SIGCONT: linux ? 18 : 19 };
          const names = {};
          for (const [name, number] of Object.entries(numbers)) names[number] = name;
          table = { numbers, names };
        }
        return table;
      };
      const sync = (event) => {
        const number = typeof event === "string" ? signals().numbers[event] : undefined;
        if (number) setHandler(number, proc.listenerCount(event) > 0);
      };
      for (const method of ["on", "addListener", "once", "prependListener", "prependOnceListener",
        "off", "removeListener", "removeAllListeners"]) {
        const inner = proc[method];
        proc[method] = function (...args) {
          const result = Reflect.apply(inner, this, args);
          if (this === proc) {
            if (args.length === 0) for (const name of Object.keys(signals().numbers)) sync(name);
            else sync(args[0]);
          }
          return result;
        };
      }
      Object.defineProperty(globalThis, "__lumen_deliver_signal", {
        configurable: true,
        value: (number) => {
          const name = signals().names[number];
          if (name === undefined || proc.listenerCount(name) === 0) return false;
          proc.emit(name, name, number);
          return true;
        },
      });
    }
  }

  // Semi-internal underscore surface Node exposes as own keys. Honest no-ops / empty collectors;
  // _rawDebug writes straight to stderr (its one real behavior).
  proc._getActiveHandles = () => [];
  proc._getActiveRequests = () => [];
  proc._rawDebug = (...a) => { proc.stderr.write(a.join(" ") + "\n"); };
  proc._exiting = false;
  proc._kill = (pid, sig) => proc.kill(pid, sig);
  proc._eval = undefined;
  proc._print_eval = false;
  proc._preload_modules = [];
  proc._debugProcess = () => {};
  proc._debugEnd = () => {};
  proc._startProfilerIdleNotifier = () => {};
  proc._stopProfilerIdleNotifier = () => {};
}

// The `process` global as an importable module (`import process from 'node:process'`).
__builtins.set("process", globalThis.process);

// ---- node:tls ---------------------------------------------------------------------------------
// TLS cannot be built on std alone (no crypto/handshake stack) and lumen takes no third-party
// crate, so — like node:net's sockets and fetch's https — anything that establishes a TLS
// connection (connect/createServer/TLSSocket) throws. The pure pieces are real: the constants,
// checkServerIdentity (RFC 6125 hostname/SAN matching), convertALPNProtocols (the length-prefixed
// wire encoding), and getCiphers (the OpenSSL cipher enumeration).
__lazyGlue(__glueIndex, "tls", "", "", () => {
  const notSupported = function () {
    throw new Error("node:tls is not supported in lumen (TLS requires a crypto stack)");
  };

  // Real: leftmost-label wildcard match (RFC 6125). "*.example.com" matches one label only.
  function matchHostname(host, pattern) {
    if (pattern === host) return true;
    if (!pattern.startsWith("*.")) return false;
    const dot = host.indexOf(".");
    if (dot < 0) return false;
    return host.slice(dot + 1) === pattern.slice(2);
  }

  // Real hostname/IP verification against a peer certificate's subjectAltName (falling back to the
  // subject CN), returning undefined on success or an ERR_TLS_CERT_ALTNAME_INVALID Error on failure
  // — the exact shape callers (and Node's own https client) check.
  function checkServerIdentity(hostname, cert) {
    hostname = String(hostname);
    const net = __builtins.get("net");
    const dnsNames = [];
    const ips = [];
    if (cert && cert.subjectaltname) {
      for (const part of cert.subjectaltname.split(", ")) {
        const idx = part.indexOf(":");
        const kind = part.slice(0, idx);
        const val = part.slice(idx + 1);
        if (kind === "DNS") dnsNames.push(val);
        else if (kind === "IP Address") ips.push(val);
      }
    }
    if (dnsNames.length === 0 && ips.length === 0 && cert && cert.subject && cert.subject.CN) {
      const cns = Array.isArray(cert.subject.CN) ? cert.subject.CN : [cert.subject.CN];
      for (const cn of cns) dnsNames.push(cn);
    }
    let valid;
    if (net.isIP(hostname)) valid = ips.includes(hostname);
    else { const host = hostname.toLowerCase(); valid = dnsNames.some((n) => matchHostname(host, n.toLowerCase())); }
    if (valid) return undefined;

    const altStr = cert && cert.subjectaltname
      ? cert.subjectaltname
      : [...dnsNames.map((d) => `DNS:${d}`), ...ips.map((ip) => `IP Address:${ip}`)].join(", ");
    const err = new Error(`Hostname/IP does not match certificate's altnames: Host: ${hostname}. is not in the cert's altnames: ${altStr}`);
    err.name = "Error";
    err.reason = `Host: ${hostname}. is not in the cert's altnames: ${altStr}`;
    err.host = hostname;
    err.cert = cert;
    err.code = "ERR_TLS_CERT_ALTNAME_INVALID";
    return err;
  }

  // Real: encode ALPN protocol names as the length-prefixed wire format; mutate `context.ALPNProtocols`.
  function convertALPNProtocols(protocols, context) {
    let buf;
    if (Array.isArray(protocols)) {
      let total = 0;
      const encoded = protocols.map((p) => Buffer.from(String(p), "utf8"));
      for (const e of encoded) total += 1 + e.length;
      buf = Buffer.alloc(total);
      let off = 0;
      for (const e of encoded) {
        buf[off++] = e.length;
        for (let i = 0; i < e.length; i++) buf[off + i] = e[i];
        off += e.length;
      }
    } else if (protocols instanceof Uint8Array) {
      buf = Buffer.from(protocols);
    } else {
      buf = Buffer.alloc(0);
    }
    if (context) context.ALPNProtocols = buf;
  }

  // The OpenSSL cipher enumeration (pure data). Real list, lowercased, as Node returns it.
  const CIPHERS = ["aes128-gcm-sha256", "aes128-sha", "aes128-sha256", "aes256-gcm-sha384", "aes256-sha", "aes256-sha256", "dhe-psk-aes128-cbc-sha", "dhe-psk-aes128-cbc-sha256", "dhe-psk-aes128-gcm-sha256", "dhe-psk-aes256-cbc-sha", "dhe-psk-aes256-cbc-sha384", "dhe-psk-aes256-gcm-sha384", "dhe-psk-chacha20-poly1305", "dhe-rsa-aes128-gcm-sha256", "dhe-rsa-aes128-sha", "dhe-rsa-aes128-sha256", "dhe-rsa-aes256-gcm-sha384", "dhe-rsa-aes256-sha", "dhe-rsa-aes256-sha256", "dhe-rsa-chacha20-poly1305", "ecdhe-ecdsa-aes128-gcm-sha256", "ecdhe-ecdsa-aes128-sha", "ecdhe-ecdsa-aes128-sha256", "ecdhe-ecdsa-aes256-gcm-sha384", "ecdhe-ecdsa-aes256-sha", "ecdhe-ecdsa-aes256-sha384", "ecdhe-ecdsa-chacha20-poly1305", "ecdhe-psk-aes128-cbc-sha", "ecdhe-psk-aes128-cbc-sha256", "ecdhe-psk-aes256-cbc-sha", "ecdhe-psk-aes256-cbc-sha384", "ecdhe-psk-chacha20-poly1305", "ecdhe-rsa-aes128-gcm-sha256", "ecdhe-rsa-aes128-sha", "ecdhe-rsa-aes128-sha256", "ecdhe-rsa-aes256-gcm-sha384", "ecdhe-rsa-aes256-sha", "ecdhe-rsa-aes256-sha384", "ecdhe-rsa-chacha20-poly1305", "psk-aes128-cbc-sha", "psk-aes128-cbc-sha256", "psk-aes128-gcm-sha256", "psk-aes256-cbc-sha", "psk-aes256-cbc-sha384", "psk-aes256-gcm-sha384", "psk-chacha20-poly1305", "rsa-psk-aes128-cbc-sha", "rsa-psk-aes128-cbc-sha256", "rsa-psk-aes128-gcm-sha256", "rsa-psk-aes256-cbc-sha", "rsa-psk-aes256-cbc-sha384", "rsa-psk-aes256-gcm-sha384", "rsa-psk-chacha20-poly1305", "srp-aes-128-cbc-sha", "srp-aes-256-cbc-sha", "srp-rsa-aes-128-cbc-sha", "srp-rsa-aes-256-cbc-sha", "tls_aes_128_ccm_8_sha256", "tls_aes_128_ccm_sha256", "tls_aes_128_gcm_sha256", "tls_aes_256_gcm_sha384", "tls_chacha20_poly1305_sha256"];

  __builtins.set("tls", {
    // socket-dependent (honest throwing stubs)
    connect: notSupported,
    createServer: notSupported,
    TLSSocket: notSupported,
    Server: notSupported,
    // createSecurePair is deprecated in Node and only made sense atop a real socket pair.
    createSecurePair: function () { throw new Error("tls.createSecurePair is deprecated and not supported in lumen"); },
    // context objects: shells (no OpenSSL context underneath, but constructing is inert)
    createSecureContext: () => ({}),
    SecureContext: function () {},
    // real, transport-free
    checkServerIdentity,
    convertALPNProtocols,
    getCiphers: () => CIPHERS.slice(),
    // No CA trust store is bundled, so getCACertificates yields an empty set (not fake certs).
    getCACertificates: () => [],
    rootCertificates: Object.freeze([]),
    // real constants
    CLIENT_RENEG_LIMIT: 3,
    CLIENT_RENEG_WINDOW: 600,
    DEFAULT_CIPHERS: "TLS_AES_256_GCM_SHA384:TLS_CHACHA20_POLY1305_SHA256:TLS_AES_128_GCM_SHA256:ECDHE-RSA-AES128-GCM-SHA256:ECDHE-ECDSA-AES128-GCM-SHA256:ECDHE-RSA-AES256-GCM-SHA384:ECDHE-ECDSA-AES256-GCM-SHA384:DHE-RSA-AES128-GCM-SHA256:ECDHE-RSA-AES128-SHA256:DHE-RSA-AES128-SHA256:ECDHE-RSA-AES256-SHA384:DHE-RSA-AES256-SHA384:ECDHE-RSA-AES256-SHA256:DHE-RSA-AES256-SHA256:HIGH:!aNULL:!eNULL:!EXPORT:!DES:!RC4:!MD5:!PSK:!SRP:!CAMELLIA",
    DEFAULT_ECDH_CURVE: "auto",
    DEFAULT_MIN_VERSION: "TLSv1.2",
    DEFAULT_MAX_VERSION: "TLSv1.3",
  });
});

// ---- node:test --------------------------------------------------------------------------------
// The runner Node ships: tests and suites run one at a time in declaration order, hooks wrap
// them, `t.after`/`t.mock` clean up, a spec-style report goes to stdout, and a failure sets the
// exit code — so a `test/*.test.cjs` file behaves under lumen as it does under `node --test`.
// Loaded on first use (see preamble.js `__lazyGlue`).
__lazyGlue(__glueIndex, "test", "", "", () => {
  const assert = __builtins.get("assert");
  const now = () => (typeof performance !== "undefined" && performance.now ? performance.now() : Date.now());

  class MockTracker {
    constructor() { this._restores = []; }
    fn(original, implementation, options) {
      if (typeof original === "object" && original !== null && implementation === undefined) { options = original; original = undefined; }
      const impl = typeof implementation === "function" ? implementation : original;
      const times = options && options.times;
      const state = { calls: [], impl, original, remaining: times === undefined ? Infinity : times };
      const mock = function (...args) {
        const call = { arguments: args, this: this, result: undefined, error: undefined, stack: new Error(), target: new.target };
        const fnToCall = state.remaining > 0 && state.impl ? state.impl : (state.original || (() => undefined));
        if (state.remaining > 0) state.remaining--;
        try {
          call.result = new.target ? Reflect.construct(fnToCall, args, new.target) : fnToCall.apply(this, args);
        } catch (e) {
          call.error = e;
          state.calls.push(call);
          throw e;
        }
        state.calls.push(call);
        return call.result;
      };
      mock.mock = {
        get calls() { return state.calls.slice(); },
        callCount() { return state.calls.length; },
        mockImplementation(fn) { state.impl = fn; state.remaining = Infinity; },
        mockImplementationOnce(fn, onCall) {
          const prev = state.impl;
          const at = onCall === undefined ? state.calls.length : onCall;
          const once = function (...args) { if (state.calls.length === at) { state.impl = prev; return fn.apply(this, args); } return prev.apply(this, args); };
          state.impl = once;
        },
        resetCalls() { state.calls.length = 0; },
        restore() { state.impl = state.original; },
      };
      return mock;
    }
    method(object, methodName, implementation, options) {
      const original = object[methodName];
      if (typeof original !== "function") throw new TypeError(`The "methodName" argument must name a function property. Received ${typeof original}`);
      const mock = this.fn(original, implementation, options);
      const descriptor = Object.getOwnPropertyDescriptor(object, methodName);
      object[methodName] = mock;
      const restore = mock.mock.restore;
      mock.mock.restore = () => { if (descriptor) Object.defineProperty(object, methodName, descriptor); else delete object[methodName]; restore(); };
      this._restores.push(mock.mock.restore);
      return mock;
    }
    getter(object, name, implementation, options) { return this.method(object, name, implementation, { ...options, getter: true }); }
    setter(object, name, implementation, options) { return this.method(object, name, implementation, { ...options, setter: true }); }
    module() { throw new Error("mock.module is not supported in lumen"); }
    reset() { this.restoreAll(); }
    restoreAll() { for (const r of this._restores.splice(0)) r(); }
  }

  let passed = 0, failed = 0, skipped = 0, todo = 0, total = 0;
  const out = (s) => { process.stdout.write(s + "\n"); };
  // Report lines stream out as tests finish, like Node's spec reporter.
  const lines = { push: out, splice: () => [] };

  function describeError(e) {
    if (e && typeof e === "object" && "stack" in e && e.stack) return String(e.stack);
    if (e && typeof e === "object" && "message" in e) return `${e.name || "Error"}: ${e.message}`;
    return String(e);
  }

  // A test (or suite) in the tree. Children run after the body has registered them, in order.
  class TestNode {
    constructor(parent, name, options, fn, kind) {
      this.parent = parent;
      this.name = name;
      this.options = options || {};
      this.fn = fn;
      this.kind = kind; // "test" | "suite"
      this.children = [];
      this.hooks = { before: [], after: [], beforeEach: [], afterEach: [] };
      this.depth = parent ? parent.depth + 1 : 0;
      this.mock = new MockTracker();
      this.done = false;
    }
    get fullName() { return this.parent && this.parent.parent ? `${this.parent.fullName} > ${this.name}` : this.name; }
    hasOnly() { return this.children.some((c) => c.options.only || c.hasOnly()); }
  }

  const root = new TestNode(null, "<root>", {}, null, "suite");
  let current = root;
  let running = false;

  async function runHooks(list, ctx) { for (const h of list) await h(ctx); }
  async function runEachHooks(node, which, ctx) {
    const chain = [];
    for (let n = node.parent; n; n = n.parent) chain.push(n);
    if (which === "beforeEach") chain.reverse();
    for (const n of chain) await runHooks(n.hooks[which], ctx);
  }

  function makeContext(node) {
    const ctx = {
      name: node.name,
      fullName: node.fullName,
      signal: typeof AbortController === "function" ? new AbortController().signal : undefined,
      mock: node.mock,
      assert,
      diagnostic(message) { lines.push(`${"  ".repeat(node.depth)}# ${message}`); },
      skip(message) { ctx._skipped = message || true; },
      todo(message) { ctx._todo = message || true; },
      plan() {},
      after(fn) { node.hooks.after.push(fn); },
      before(fn) { node.hooks.before.push(fn); },
      beforeEach(fn) { node.hooks.beforeEach.push(fn); },
      afterEach(fn) { node.hooks.afterEach.push(fn); },
      test(name, options, fn) { return declare(node, name, options, fn, "test"); },
      waitFor: async (fn, options) => {
        const timeout = (options && options.timeout) || 1000, interval = (options && options.interval) || 50;
        const deadline = now() + timeout;
        for (;;) {
          try { return await fn(); } catch (e) { if (now() >= deadline) throw e; }
          await new Promise((r) => setTimeout(r, interval));
        }
      },
    };
    ctx.runOnly = () => {};
    return ctx;
  }

  function withTimeout(promise, ms, name) {
    if (!(ms > 0) || ms === Infinity) return promise;
    let timer;
    const timeout = new Promise((_, reject) => { timer = setTimeout(() => reject(new Error(`test timed out after ${ms}ms`)), ms); });
    return Promise.race([promise, timeout]).finally(() => clearTimeout(timer));
  }

  async function runNode(node, onlyMode) {
    const skipReason = node.options.skip;
    const todoReason = node.options.todo;
    const indent = "  ".repeat(node.depth - 1);
    const skipNow = (suffix) => {
      if (node.kind === "test") { skipped++; total++; }
      lines.push(`${indent}﹣ ${node.name} # SKIP${suffix}`);
      node.done = true;
      if (node.onDone) node.onDone();
    };
    if (onlyMode && !node.options.only && !node.hasOnly()) return skipNow("");
    if (skipReason) return skipNow(typeof skipReason === "string" ? " " + skipReason : "");
    const start = now();
    const ctx = makeContext(node);
    let error = null;
    const previous = current;
    current = node;
    try {
      if (node.kind === "test") await runEachHooks(node, "beforeEach", ctx);
      await runHooks(node.hooks.before, ctx);
      node.running = true;
      if (typeof node.fn === "function") {
        const result = node.fn.length >= 2
          ? new Promise((resolve, reject) => { try { node.fn(ctx, (e) => (e ? reject(e) : resolve())); } catch (e) { reject(e); } })
          : Promise.resolve().then(() => node.fn(ctx));
        await withTimeout(result, node.options.timeout, node.name);
      }
      const childOnly = node.hasOnly();
      while (node.children.some((c) => !c.done && !c.running)) {
        await runNode(node.children.find((c) => !c.done && !c.running), childOnly);
      }
    } catch (e) {
      error = e;
    } finally {
      node.running = false;
      current = previous;
      try { await runHooks(node.hooks.after, ctx); } catch (e) { error = error || e; }
      // Each-hooks wrap tests, not suites (a suite's own hooks wrap the tests inside it).
      if (node.kind === "test") {
        try { await runEachHooks(node, "afterEach", ctx); } catch (e) { error = error || e; }
      }
      node.mock.restoreAll();
      node.done = true;
      if (node.onDone) node.onDone();
    }
    const ms = (now() - start).toFixed(3);
    // The summary counts tests; suites are tallied separately and a suite fails only through
    // its tests (or its own hooks), so it is not one more failure.
    const isTest = node.kind === "test";
    if (isTest) total++;
    const childFailed = node.children.some((c) => c.failed);
    if (ctx._skipped) { if (isTest) skipped++; lines.push(`${indent}﹣ ${node.name} # SKIP`); return; }
    if (ctx._todo || todoReason) { if (isTest) todo++; lines.push(`${indent}${error ? "✖" : "✔"} ${node.name} # TODO`); return; }
    if (error || childFailed) {
      if (isTest || (error && !childFailed)) failed++;
      node.failed = true;
      lines.push(`${indent}✖ ${node.name} (${ms}ms)`);
      if (error) {
        for (const l of describeError(error).split("\n")) lines.push(`${indent}  ${l}`);
        node.error = error;
      }
    } else {
      if (isTest) passed++;
      lines.push(`${indent}✔ ${node.name} (${ms}ms)`);
    }
  }

  let queue = Promise.resolve();
  let scheduled = false;
  // A test awaiting something the loop does not otherwise hold open (an unref'd timer, say)
  // must still get to finish: the harness keeps the loop alive for the whole run, as Node's
  // does, and the exit hook above only ever sees a genuinely stuck test.
  let keepAlive = null;
  function schedule() {
    if (scheduled) return;
    scheduled = true;
    // Let the whole file register its tests before the first one runs (Node does the same).
    setTimeout(() => {
      scheduled = false;
      if (running) return;
      running = true;
      keepAlive = setInterval(() => {}, 1 << 30);
      queue = queue.then(async () => {
        const onlyMode = root.hasOnly();
        const rootCtx = makeContext(root);
        try {
          await runHooks(root.hooks.before, rootCtx);
          while (root.children.some((c) => !c.done && !c.running)) {
            await runNode(root.children.find((c) => !c.done && !c.running), onlyMode);
          }
        } catch (e) {
          failed++;
          lines.push(`✖ hook failed: ${describeError(e)}`);
        }
        try { await runHooks(root.hooks.after, rootCtx); } catch (e) { failed++; lines.push(`✖ after hook failed: ${describeError(e)}`); }
        root.hooks.before.length = root.hooks.after.length = 0;
        running = false;
        clearInterval(keepAlive);
        report();
      });
    }, 0);
  }

  // A test that is still awaiting something when the loop goes quiet never finishes: Node
  // reports it as cancelled and fails the run, rather than exiting 0 in silence.
  process.on("exit", () => {
    if (!running) return;
    const unfinished = [];
    const walk = (n) => { for (const c of n.children) { if (!c.done) unfinished.push(c); walk(c); } };
    walk(root);
    for (const c of unfinished) out(`✖ ${c.fullName} (cancelled: the process exited before the test finished)`);
    if (unfinished.length) { failed += unfinished.length; total += unfinished.length; }
    running = false;
    clearInterval(keepAlive);
    report();
  });

  function report() {
    out(`ℹ tests ${total}`);
    out(`ℹ suites ${root.children.filter((c) => c.kind === "suite").length}`);
    out(`ℹ pass ${passed}`);
    out(`ℹ fail ${failed}`);
    out(`ℹ cancelled 0`);
    out(`ℹ skipped ${skipped}`);
    out(`ℹ todo ${todo}`);
    if (failed > 0) {
      out("");
      out("✖ failing tests:");
      const walk = (n) => { for (const c of n.children) { if ((c.failed && c.error) || !c.done) out(`✖ ${c.fullName}`); walk(c); } };
      walk(root);
      process.exitCode = 1;
    }
    passed = failed = skipped = todo = total = 0;
    root.children.length = 0;
  }

  function normalize(name, options, fn) {
    if (typeof name === "function") { fn = name; name = fn.name || "<anonymous>"; options = {}; }
    else if (typeof options === "function") { fn = options; options = {}; }
    return { name: String(name), options: options || {}, fn };
  }

  function declare(parent, name, options, fn, kind) {
    const spec = normalize(name, options, fn);
    const node = new TestNode(parent, spec.name, spec.options, spec.fn, kind);
    parent.children.push(node);
    // A subtest declared from inside its parent's body runs right away, so `await t.test(...)`
    // sequences like it does in Node; a top-level test waits for the file to finish registering.
    if (parent.running) return runNode(node, false);
    if (parent === root) schedule();
    return new Promise((resolve) => { node.onDone = resolve; });
  }

  const test = (name, options, fn) => declare(current, name, options, fn, "test");
  const describe = (name, options, fn) => {
    const spec = normalize(name, options, fn);
    const node = new TestNode(current, spec.name, spec.options, null, "suite");
    current.children.push(node);
    // A suite's body runs immediately so its tests register in order under it.
    const previous = current;
    current = node;
    try { if (spec.fn) spec.fn(); } finally { current = previous; }
    if (previous === root) schedule();
    return Promise.resolve();
  };
  const variants = (base) => {
    base.skip = (name, options, fn) => { const s = normalize(name, options, fn); return base(s.name, { ...s.options, skip: true }, s.fn); };
    base.todo = (name, options, fn) => { const s = normalize(name, options, fn); return base(s.name, { ...s.options, todo: true }, s.fn); };
    base.only = (name, options, fn) => { const s = normalize(name, options, fn); return base(s.name, { ...s.options, only: true }, s.fn); };
    return base;
  };
  variants(test);
  variants(describe);
  test.test = test;
  test.it = test;
  test.describe = describe;
  test.suite = describe;
  test.before = (fn) => { current.hooks.before.push(fn); };
  test.after = (fn) => { current.hooks.after.push(fn); };
  test.beforeEach = (fn) => { current.hooks.beforeEach.push(fn); };
  test.afterEach = (fn) => { current.hooks.afterEach.push(fn); };
  test.mock = new MockTracker();
  test.run = () => { throw new Error("test.run is not supported in lumen"); };
  test.snapshot = { setDefaultSnapshotSerializers() {}, setResolveSnapshotPath() {} };
  test.assert = assert;
  __builtins.set("test", test);
});
