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
  const requests = new Map();

  const host = {
    fetch(id, method, url, headers, body, options = {}) {
      const controller = new AbortController();
      const state = { controller, reader: null };
      requests.set(id, state);
      (async () => {
        try {
          const init = { method, headers: new Headers(headers), signal: controller.signal };
          for (const name of ["mode", "credentials", "redirect"]) {
            if (options[name] !== undefined) init[name] = options[name];
          }
          if (body !== null && method !== 'GET' && method !== 'HEAD') init.body = body;
          const res = await fetchImpl(url, init);
          const pairs = [];
          res.headers.forEach((v, k) => pairs.push(k, v));
          if (controller.signal.aborted) { if (res.body) await res.body.cancel(); return; }
          if (res.body) {
            state.reader = res.body.getReader();
            // No read is issued until the guest stream asks for a chunk.
            push(id, 'ok', [res.status, res.statusText, res.url, id, res.redirected === true, res.type || "basic", ...pairs]);
          } else if (res.body === null || method === 'HEAD' || [204, 205, 304].includes(res.status)) {
            requests.delete(id);
            push(id, 'ok', [res.status, res.statusText, res.url, null, res.redirected === true, res.type || "basic", ...pairs]);
          } else {
            // Compatibility with embedders predating the response reader bridge.
            const bytes = new Uint8Array(await res.arrayBuffer());
            if (controller.signal.aborted) return;
            requests.delete(id);
            push(id, 'ok', [res.status, res.statusText, res.url, bytes, res.redirected === true, res.type || "basic", ...pairs]);
          }
        } catch (e) {
          requests.delete(id);
          if (controller.signal.aborted) return;
          push(id, 'error', [String((e && e.message) || e)]);
        }
      })();
    },
    fetchRead(id, taskId) {
      const state = requests.get(id);
      (async () => {
        try {
          if (!state || !state.reader) throw new Error('Fetch response body is closed');
          const result = await state.reader.read();
          if (state.controller.signal.aborted) throw new Error('Fetch response body was aborted');
          if (result.done) {
            state.reader.releaseLock();
            state.reader = null;
            requests.delete(id);
            push(taskId, 'end', []);
          } else {
            push(taskId, 'chunk', [result.value]);
          }
        } catch (e) {
          requests.delete(id);
          // Pending guest reads must settle even when the parent request aborts.
          push(taskId, 'error', [String((e && e.message) || e)]);
        }
      })();
    },
    fetchAbort(id) {
      const state = requests.get(id);
      if (state) {
        state.controller.abort();
        if (state.reader) {
          state.reader.cancel().catch(() => {}).finally(() => {
            if (state.reader) { state.reader.releaseLock(); state.reader = null; }
          });
        }
      }
      requests.delete(id);
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
