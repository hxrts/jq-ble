# Policy

`jq-ble` owns repo-specific policy here. The reusable toolkit stays in the
external toolkit flake input.

Use this directory for:

- `jq-ble` toolkit configuration in `toolkit.toml`
- repo-local checks or lints if this workspace grows rules that do not belong
  in the shared toolkit
- fixtures or policy docs that depend on `jq-ble` architecture terms

## Ownership Rule

- generic enforcement belongs in the toolkit
- `jq-ble`-specific semantics belong here

If a rule only needs repository-specific scope, configure it in
`policy/toolkit.toml`.
