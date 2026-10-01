// ---- registration ------------------------------------------------------------------------------

__builtins.set("test", require("test"));
__builtins.set("test/reporters", require("test/reporters"));
// `--test`: lumen-cli runs Node's internal/main/test_runner through this.
__internals.set("testRunnerMain", () => require("internal/main/test_runner"));
__internals.set("testRunnerRequire", require);
