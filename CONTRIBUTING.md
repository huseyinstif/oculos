# Contributing to OculOS

Thanks for your interest in OculOS! Here's how to get started.

## Development Setup

```bash
git clone https://github.com/huseyinstif/oculos.git
cd oculos
cargo build --release
```

## Making Changes

1. Fork the repo and create a branch from `main`
2. Make your changes
3. Run the checks CI enforces:
   ```bash
   cargo fmt --all -- --check
   cargo clippy --all-targets -- -D warnings
   cargo test
   ```
4. Platform code is behind `cfg(target_os)`, so also cross-check the other backends
   (no linker needed for `check`/`clippy`):
   ```bash
   rustup target add x86_64-pc-windows-msvc aarch64-apple-darwin
   cargo clippy --target x86_64-pc-windows-msvc -- -D warnings
   cargo clippy --target aarch64-apple-darwin -- -D warnings
   ```
5. If you touch the SDKs: `python sdk/python/test_sdk.py` and `cd sdk/typescript && npm install && npm test`
6. Open a pull request

## Areas We Need Help

- **macOS** — native element highlighting overlay, broader app coverage
- **Tests** — integration tests across different app types (Win32, Electron, Qt)
- **Dashboard** — UI improvements, new features
- **Documentation** — guides, examples, tutorials

## Code Style

- Follow existing Rust conventions in the codebase
- Keep functions focused and small
- Add comments for non-obvious logic
- No `unsafe` unless absolutely necessary (and document why)
- Backends return typed errors (`error::not_found`, `invalid_input`, `unsupported`…) so clients get a useful `code`, and never report success for an action that did not happen
- The dashboard (`static/index.html`) is a single file with no build step; escape every app-provided string (`esc()`) or set it with `textContent`

## Reporting Bugs

Open an issue with:
- OS and version
- Steps to reproduce
- Expected vs actual behavior
- Target application (if relevant)

## License

By contributing, you agree that your contributions will be licensed under the MIT License.
