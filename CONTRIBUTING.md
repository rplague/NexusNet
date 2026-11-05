# Contributing to NexusNet

Thanks for taking the time to contribute. This guide covers the local workflow,
the checks CI runs, and how changes are integrated.

## Prerequisites

- Rust 1.85 or newer. `edition = "2024"` is required, and `rust-toolchain.toml`
  pins a stable toolchain with `rustfmt` and `clippy`.
- `python3` 3.11+ and `whiptail` only if you work on the Debian installer.
- `mingw-w64` and NSIS only if you build the Windows package.

## Quickstart

```bash
git clone https://git.oahd.cn/git/rplague/NexusNet.git
cd NexusNet

cargo build
cargo run --example basic_node
cargo run --example two_nodes
```

Both examples use loopback addresses and temporary directories, so they do not
need a public IP. `basic_node` runs until Ctrl-C; `two_nodes` exits on its own
after a successful service call.

## Checks

Run the same checks as CI before opening a pull request:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --verbose
```

If you changed the installer, also run:

```bash
for f in deb/config deb/postinst deb/prerm deb/postrm; do sh -n "$f"; done
python3 deb/tests/prefill_test.py
python3 deb/tests/noninteractive_test.py
python3 deb/tests/interactive_test.py
python3 deb/tests/postinst_test.py
```

## Commit conventions

The repository uses [Conventional Commits](https://www.conventionalcommits.org/):
`feat`, `fix`, `refactor`, `docs`, `test`, `ci`, `chore`. Keep one logical change
per commit when practical, and use a branch instead of committing to `main`.

Examples:

```text
feat(dispatcher): retry backend connection with jitter
fix(auth): reject whitelist entries with duplicate versions
docs(readme): clarify relay requirements
```

## Pull requests

- Open issues and pull requests on the GitHub mirror. The primary repository is
  `git.oahd.cn`; accepted changes are integrated there and the mirror is synced
  without force-pushing, so existing branches and reviews are preserved.
- Keep pull requests focused. Unrelated refactors belong in separate changes.
- Describe the behavior change, the tests you ran, and any compatibility impact.
- Add tests for behavior changes. A bug fix should include a regression test when
  the failure can be reproduced.
- Update the relevant document under `docs/` when a protocol or runtime behavior
  changes.
- The English `README.md` is canonical; `README.zh-CN.md` is a translation and may
  lag behind it.

## Code style

- Use `cargo fmt` with the default settings; there are no custom rustfmt options.
- Keep `cargo clippy --all-targets -- -D warnings` clean.
- Prefer existing modules and patterns over new abstractions. Discuss new
  dependencies in an issue before adding them.
- Do not log secrets, key material or full configuration files.

## Security

Security reports are handled in the open by default. See [SECURITY.md](./SECURITY.md)
before filing a report, and redact keys and private data from configs and logs.

## License

NexusNet is licensed under GPL-3.0. By contributing you agree that your changes
are distributed under the same license.
