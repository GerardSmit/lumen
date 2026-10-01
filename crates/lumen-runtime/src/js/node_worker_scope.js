(() => {
  "use strict";
  const wself = globalThis.__wself;
  delete globalThis.__wself;
  const threadId = globalThis.__lumenWorkerThreadId;
  delete globalThis.__lumenWorkerThreadId;
  const initBytes = globalThis.__lumenWorkerInit;
  delete globalThis.__lumenWorkerInit;
  const hook = globalThis.__lumenInitWorkerThread;
  if (typeof hook !== "function") throw new Error("node worker glue is not installed");
  globalThis.__workerDispatchMessage = hook(wself, threadId, initBytes);
})();
