/** @jsxImportSource ./runtime */
function origin() { return new Error("jsx-position").stack; }
globalThis.jsxStack = <div>{origin()}</div>;
