# Contributing to Agentree

Thanks for helping improve Agentree. Changes should preserve the documented
task isolation, explicit Git mutations, and conservative recovery behavior.

## Development setup

Install Git and Rust 1.80 or newer, then run the quality checks from the
repository root:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo test --doc
```

Changes to GitHub Actions should also pass actionlint and zizmor. End-to-end
tests create temporary Git repositories and require a working Git executable.

## Pull requests

- Explain the user-visible behavior and the safety or recovery implications.
- Add or update tests for behavior changes, especially failure and interruption
  paths.
- Update the command reference or README when the CLI contract changes.
- Keep commits focused and avoid committing generated build artifacts.

Before opening a pull request, confirm that the full local quality suite passes
and that the working tree contains only intended changes.

## Security-sensitive changes

Do not disclose a vulnerability in a public issue or pull request. Follow the
process in [SECURITY.md](SECURITY.md).
