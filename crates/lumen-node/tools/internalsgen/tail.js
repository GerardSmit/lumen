// ---- registration ------------------------------------------------------------------------------

__internals.set("exposedInternals", { has: hasModule, require });
// process.binding() serves its allowlisted bindings from the same table.
__internals.set("internalBinding", internalBinding);
