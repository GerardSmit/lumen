(() => {
  "use strict";
  const take = (name) => {
    const value = globalThis[name];
    delete globalThis[name];
    return value;
  };
  const wself = take("__wself");
  const threadId = take("__lumenWorkerThreadId");
  const initBytes = take("__lumenWorkerInit");
  const ports = take("__lumenWorkerPorts");
  const hook = globalThis.__lumenInitWorkerThread;
  if (typeof hook !== "function") throw new Error("node worker glue is not installed");
  const hooks = hook(wself, threadId, initBytes, ports);
  Object.defineProperty(globalThis, "__lumenWorkerHooks", { value: hooks, configurable: true });
  globalThis.__workerDispatchMessage = hooks.dispatch;
})();
