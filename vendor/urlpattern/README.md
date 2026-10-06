# urlpattern

Vendored from [`urlpattern` 0.6.0](https://crates.io/crates/urlpattern/0.6.0), the Deno
implementation of the [`URLPattern` web API][urlpattern]. See [UPSTREAM.md](UPSTREAM.md) for what
Lumen changed: patterns compile to ECMAScript regular expressions run by `lumen_common::regex`,
and URLs are canonicalized by `lumen_common::url`.

[urlpattern]: https://urlpattern.spec.whatwg.org/
