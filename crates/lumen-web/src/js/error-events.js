// Shared by the browser glue and the bare-metal HTML bootstrap. Keep this
// constructor separate so native script evaluation can report window errors
// without maintaining a second ErrorEvent implementation.
class ErrorEvent extends globalThis.Event {
  constructor(type, init = {}) {
    super(type, init);
    init = init && typeof init === "object" ? init : {};
    const num = (v) => (Number.isFinite(Number(v)) ? Number(v) >>> 0 : 0);
    this.message = "message" in init ? String(init.message) : "";
    this.filename = "filename" in init ? String(init.filename) : "";
    this.lineno = "lineno" in init ? num(init.lineno) : 0;
    this.colno = "colno" in init ? num(init.colno) : 0;
    this.error = "error" in init ? init.error : undefined;
  }
}

globalThis.ErrorEvent = ErrorEvent;
