// `navigator.userAgent`.

if (typeof globalThis.navigator === "undefined") {
  globalThis.navigator = {};
}
Object.defineProperty(globalThis.navigator, "userAgent", {
  value: "lumen",
  enumerable: true,
  configurable: true,
});
