# Contributing

Thanks for helping improve Plexus. This guide describes the expected workflow for contributors.

## Workflow

1. Start from the latest version of this branch.
2. Keep each pull request focused on one concern.
3. Prefer small, reviewable changes over broad rewrites.
4. Update documentation when setup, commands, structure, or contributor workflow changes.
5. Add or update tests when behavior changes.

## Local Validation

Before opening a pull request, run the Rust workflow locally:

```sh
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo build --workspace --all-features
```

You can run the same sequence with:

```sh
make fmt
make clippy
make test
make build
```

## Pull Requests

- Use a clear title that describes the change.
- Explain why the change is needed, not only what changed.
- Link related issues with `Closes #123` when applicable.
- Ensure the GitHub checks for format, clippy, and tests pass.
- Do not include unrelated formatting, generated files, or refactors.

## Commit Guidelines

- Use concise, descriptive commit messages.
- Group related changes together.

## Issues

Use the bug report template for reproducible problems and the feature request template for new ideas. Include enough context for maintainers to understand the use case, expected behavior, and validation path.

## Code Style

- Follow standard Rust formatting with `cargo fmt`.
- Treat clippy warnings as failures.
- Keep placeholder code minimal until implementation work begins.
- Prefer explicit, simple structure over premature abstraction.
