# Contributing to Atim

Thank you for your interest in contributing to Atim!

## Building from Source

```bash
git clone https://github.com/zitsen/atim.git
cd atim
cargo build --release
```

### Prerequisites

- [Rust](https://rustup.rs/) (stable, edition 2024)
- [tmux](https://github.com/tmux/tmux)
- [zoxide](https://github.com/ajeetdsouza/zoxide) (optional, for directory browsing)

## Running Tests

```bash
# Run all tests
cargo test

# Run clippy (required by CI)
cargo clippy --all-targets -- -D warnings

# Check formatting
cargo fmt --all --check
```

## Project Structure

| Crate          | Purpose                                                         |
| -------------- | --------------------------------------------------------------- |
| `atim-core`    | Config, error types, IM trait, message types, agent abstraction |
| `atim-im`      | Telegram + Feishu adapter implementations                       |
| `atim-tmux`    | tmux window lifecycle and terminal I/O                          |
| `atim-parser`  | JSONL log and terminal output parsing                           |
| `atim-monitor` | Session log polling with byte-offset tracking                   |
| `atim-queue`   | Per-user async message queues with flood control                |
| `atim-state`   | Thread binding and window state persistence (SQLite)            |
| `atim-bin`     | Entry point — server + CLI                                      |

## Submitting Changes

1. Fork the repository
2. Create a feature branch: `git checkout -b feat/my-feature`
3. Make your changes and add tests
4. Run `cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --all --check`
5. Commit using [Conventional Commits](https://www.conventionalcommits.org/): `feat: add my feature`
6. Open a Pull Request against `main`

## Commit Convention

We use [Conventional Commits](https://www.conventionalcommits.org/):

```
feat: add new feature
fix: resolve bug
docs: update documentation
refactor: restructure code without changing behavior
test: add or update tests
chore: build, CI, deps, tooling
perf: performance improvement
```

## Code Style

- Follow Rust idioms and naming conventions
- All tests must pass before submitting
- Clippy warnings are treated as errors (`-D warnings`)
- Use `tracing` for logging, not `println!`

## License

By contributing, you agree that your contributions will be licensed under the [MIT License](LICENSE).
