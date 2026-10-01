// The page side of the runtime: the host object the wasm runtime calls into (fetch, WebSocket,
// the suspending sync call) and the loop driver that resumes the realm when a timer is due or a
// browser operation settles.
//
//   import init, { RuntimeSession } from './pkg/lumen_wasm.js';
//   await init();
//   const rt = await createRuntime({ RuntimeSession, bridge });
//   rt.eval('setTimeout(() => console.log("hi"), 10)');
//   await rt.whenIdle();

export async function createRuntime({
  RuntimeSession,
  bridge = null,
  options = {},
  onOutput = () => {},
  fetchImpl = globalThis.fetch && globalThis.fetch.bind(globalThis),
  WebSocketImpl = globalThis.WebSocket,
}) {
  let session = null;
  let timer = null;
  let status = { nextTimerMs: null, pendingTasks: false, idle: true, halted: false };
  let waiters = [];

  function settle() {
    if (status.idle || status.halted) {
      const ready = waiters;
      waiters = [];
      for (const resolve of ready) resolve();
    }
  }

  function schedule() {
    if (timer !== null) clearTimeout(timer);
    timer = null;
    if (status.nextTimerMs !== null && !status.halted) {
      timer = setTimeout(() => {
        timer = null;
        handle(session.tick());
      }, Math.max(0, status.nextTimerMs));
    }
  }

  function handle(result) {
    if (result.stdout) onOutput('stdout', result.stdout);
    if (result.stderr) onOutput('stderr', result.stderr);
    status = result.status;
    schedule();
    settle();
    return result;
  }

  const push = (id, kind, args) => handle(session.pushEvent(id, kind, args));
  const sockets = new Map();

  const host = {
    fetch(id, method, url, headers, body) {
      (async () => {
        try {
          const init = { method, headers: new Headers(headers) };
          if (body !== null && method !== 'GET' && method !== 'HEAD') init.body = body;
          const res = await fetchImpl(url, init);
          const bytes = new Uint8Array(await res.arrayBuffer());
          const pairs = [];
          res.headers.forEach((v, k) => pairs.push(k, v));
          push(id, 'ok', [res.status, res.statusText, res.url || url, bytes, ...pairs]);
        } catch (e) {
          push(id, 'error', [String((e && e.message) || e)]);
        }
      })();
    },
    wsOpen(id, url, protocols) {
      const list = protocols ? protocols.split(',').map((p) => p.trim()).filter(Boolean) : [];
      const ws = new WebSocketImpl(url, list);
      ws.binaryType = 'arraybuffer';
      sockets.set(id, ws);
      ws.onopen = () => push(id, 'open', [ws.protocol || '']);
      ws.onmessage = (e) =>
        typeof e.data === 'string' ? push(id, 'text', [e.data]) : push(id, 'binary', [new Uint8Array(e.data)]);
      ws.onerror = () => {};
      ws.onclose = (e) => {
        sockets.delete(id);
        push(id, 'close', [e.code, e.reason, e.wasClean]);
      };
    },
    wsSend(id, data) {
      const ws = sockets.get(id);
      if (!ws || ws.readyState !== 1) return false;
      ws.send(data);
      return true;
    },
    wsClose(id, code, reason) {
      const ws = sockets.get(id);
      if (ws) ws.close(code === 1005 ? undefined : code, reason);
    },
    syncCall(kind, payload) {
      if (!bridge) throw new Error('no synchronous host bridge was provided');
      return bridge.call(kind, payload);
    },
  };

  session = new RuntimeSession(host, options);

  return {
    session,
    eval(src) {
      return handle(session.eval(src));
    },
    evalModule(src, key) {
      return handle(session.evalModule(src, key));
    },
    runMain(path) {
      return handle(session.runMain(path));
    },
    writeFile: (path, data) => session.writeFile(path, data),
    readFile: (path) => session.readFile(path),
    mountRemote: (prefix) => session.mountRemote(prefix),
    whenIdle() {
      if (status.idle || status.halted) return Promise.resolve();
      return new Promise((resolve) => waiters.push(resolve));
    },
  };
}
