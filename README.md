# Leech

<img src="assets/brand/leech-1024.png" alt="Leech mark" width="96">

A fast, native Discord client for the desktop, written in Rust on egui: plum-cast surfaces, a
rose accent, hairline edges, and controls placed where Discord users expect them.

> [!WARNING]
> Leech is an unofficial third-party client. It talks to Discord's gateway and REST endpoints
> with your normal account. Discord's Terms of Service do not allow that, and an account can be
> terminated for it. Use Leech at your own risk. Leech is not affiliated with or endorsed by
> Discord.

## Status

Early. The interface comes first; performance work follows. Leech is developed and checked on
Windows. The Linux and macOS packaging in `packaging/` has not been built yet.

## Run it

Rust **1.98.1** is pinned in `rust-toolchain.toml`. Windows needs the Visual Studio C++ build
tools, CMake and the WebView2 Runtime. For macOS and Linux prerequisites see
[platform support](docs/platform-support.md).

```sh
# Offline preview with synthetic data. No account, no network sign-in.
cargo run --locked --features demo -- --demo

# The real client. You sign in on Discord's own login page inside a temporary webview.
cargo run --locked
```

Add `--demo-call`, `--demo-voice`, `--demo-settings`, `--demo-friends` or `--demo-light` after
`--demo` to open the preview on a specific screen.

Source builds keep their data in the OS-local `leech-development` folder; packaged builds use
`leech`. Set `LEECH_DATA_DIR` to an absolute path to use another folder. Login tokens stay in
the operating-system credential store.

```sh
cargo xtask check     # formatting, Clippy, tests and policy checks
cargo xtask package   # release package for the current platform
```

## Documentation

- [Design](docs/design.md) and the [theme API](docs/theme-api.md)
- [Architecture](docs/architecture.md)
- [Authentication](docs/authentication.md) and the [storage policy](docs/storage-policy.md)
- [Discord compatibility](docs/discord-compatibility.md)
- [Voice](docs/voice.md)
- [Extensions](docs/extensions.md)
- [Threat model](docs/threat-model.md)

## Licence

`MIT OR Apache-2.0`. See [LICENSE-MIT](LICENSE-MIT), [LICENSE-APACHE](LICENSE-APACHE) and the
[third-party notices](THIRD_PARTY_NOTICES.md).
