# Repository Guidelines

## Project Structure & Module Organization

This Rust 2024 workspace contains `shadowquic` (the default library and CLI crate), `shadowquic-macros` (procedural macros), and `perf` (TCP/UDP performance binaries). In `shadowquic/src/`, protocol modules contain inbound/outbound implementations; `config/`, `utils/`, and `observe/` provide shared support. Integration tests live in `shadowquic/tests/`, with fixtures under `tests/fixtures/`; unit tests also live alongside source code. Use `shadowquic/config_examples/` and `shadowquic/examples/` for configuration and API examples. Documentation lives in `document/`, site tooling in `assets/sites/`, and protocol sources in `PROTOCOL.typ`.

## Build, Test, and Development Commands

Run commands from the repository root:

- `nix develop`: enter the development shell with native dependencies and supporting tools.
- `cargo build`: build the default `shadowquic` crate.
- `cargo run -p shadowquic -- -c shadowquic/config_examples/client.yaml`: run the proxy; adjust the example configuration for your environment first.
- `cargo fmt --all -- --check`: check Rust formatting; use `cargo fmt --all` to apply it.
- `cargo clippy -p shadowquic --all-targets`: lint the main crate and its targets.
- `cargo test --release`: run Rust tests, matching the main CI workflow.
- `cargo test -p shadowquic --test tcp_echo`: run one integration test target.
- `uv run ./scripts/main_test.py`: run the Python integration suite used by CI.

## Coding Style & Naming Conventions

Use rustfmt defaults, including four-space indentation. Follow Rust conventions: `snake_case` for modules and functions, `PascalCase` for types, and `SCREAMING_SNAKE_CASE` for constants. Keep protocol-specific behavior in its module and preserve feature and platform gates when changing shared code.

## Testing Guidelines

Use `#[test]` for synchronous tests and `#[tokio::test]` for asynchronous behavior. Name new tests descriptively by behavior; integration filenames identify the scenario, such as `socks_out_dual_stack.rs`. Add regression coverage for fixes. Network tests need available local ports; check platform requirements for TPROXY tests. No numeric coverage threshold is configured.

## Commit & Pull Request Guidelines

Follow recent commit prefixes: `fix:`, `refactor:`, `chore:`, and `doc:`; use a scope when helpful, such as `fix(direct):`. Keep subjects concise and imperative. PR descriptions should explain the problem, resulting behavior, relevant issues, and validation performed. Identify affected features or platforms, and update configuration examples and documentation when user-facing behavior changes.
