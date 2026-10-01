(() => {
  "use strict";
  const post = __wself.post;
  const closeSelf = __wself.close;
  const report = __wself.report;
  delete globalThis.__wself;
  const serialize = (value, transfer) => globalThis.__serializeForClone(value, transfer, true);
  const deserialize = (bytes) => globalThis.__deserializeClone(bytes);

  // The global scope acts as an EventTarget for message/messageerror/error.
  let scopeTarget;
  const target = {
    addEventListener: (...args) => (scopeTarget ??= new EventTarget()).addEventListener(...args),
    removeEventListener: (...args) => (scopeTarget ??= new EventTarget()).removeEventListener(...args),
    dispatchEvent: (event) => (scopeTarget ??= new EventTarget()).dispatchEvent(event),
  };
  globalThis.addEventListener = target.addEventListener;
  globalThis.removeEventListener = target.removeEventListener;
  globalThis.dispatchEvent = target.dispatchEvent;

  globalThis.postMessage = (message, _transfer) => { post(serialize(message)); };
  globalThis.close = () => closeSelf();
  globalThis.onmessage = null;
  globalThis.onmessageerror = null;

  const fire = (type, event) => {
    const h = globalThis["on" + type];
    if (typeof h === "function") { try { h.call(globalThis, event); } catch (e) { reportError(e); } }
    target.dispatchEvent(event);
  };

  globalThis.__workerDispatchMessage = (bytes) => {
    if (bytes === false) return; // channel-closed sentinel
    let data;
    try { data = deserialize(bytes); }
    catch { fire("messageerror", new MessageEvent("messageerror", {})); return; }
    fire("message", new MessageEvent("message", { data }));
  };

  // A worker-side uncaught error propagates to the parent's Worker.onerror.
  globalThis.onerror = (message) => { report(String(message)); return true; };
})();
