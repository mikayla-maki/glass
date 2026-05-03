# AGENTS.md

## Code Principles

## Prefer interfaces when accessing external resources

If there's a large external area that we don't control but depend on, create an interface
and a mock implementation for it. Never mock our own code, only external systems like the filesystem,
agent responses, etc.

### Tests

When testing, prefer declarative tests showing exact state transitions:

1. Set up the system state
2. Run the operation under test.
3. Assert on resulting state with `assert_eq!` against an _exact_ expected value.

The test reads top to bottom like a transcript of what happened. NEVER simulate behavior,
always use the public Rust API and the mocks.


## Code style

- `cargo fmt && cargo clippy -- -D warnings && cargo test` before any diff.
- We document in code, not in comments. Most functions don't need a doc
  comment — the name and signature do the work. Comment when you need to
  explain *why*, not *what*. Same for module preambles: skip them unless
  there's something genuinely non-obvious.
- Errors via `anyhow::Result` with `.context("…")`. Don't `unwrap()` outside
  tests. Don't `expect("should never happen")` in production paths.
- Logging via `tracing`. Don't log message contents.


## Don't

- Intoduce new major dependencies (testing frameworks, ORMs, etc.) without checking first
