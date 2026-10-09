This is the exact crates.io `content-security-policy` 0.9.0 source release.
The upstream license files are preserved.

Local patch (`src/lib.rs`): adds `is_permissions_source_expression` and
`matches_permissions_source_expression`, which expose the crate's existing
scheme-source and host-source matchers so `lumen-common::permissions_policy`
can match Permissions Policy allowlists without a second source-expression
parser. Candidate for upstreaming.
