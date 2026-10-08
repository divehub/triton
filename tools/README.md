# Local tools

Nothing in this directory is versioned except this note: `/tools/*` is ignored by Git (see `.gitignore`).

The only tool the repository can use from here is an optional **repository-local Rust toolchain** in `tools/rust/`. `./cargo` uses it when `tools/rust/cargo/bin/cargo` exists, with `RUSTUP_HOME=tools/rust/rustup` and `CARGO_HOME=tools/rust/cargo`, so no global rustup or cargo state is read or written. Without it, `./cargo` runs the `cargo` on your `PATH`, which is what CI does; rustup then installs nothing by itself, so install the pinned toolchain once:

```sh
rustup toolchain install 1.98.1 --profile minimal --target wasm32-unknown-unknown
```

(`rust-toolchain.toml` at the repository root pins Rust 1.98.1 and the `wasm32-unknown-unknown` target.)

## Provisioning `tools/rust`

Only if you want an isolated toolchain that never touches `~/.rustup` or `~/.cargo`:

1. Download `rustup-init` for your platform from <https://static.rust-lang.org/rustup/dist/> (for Apple silicon: `aarch64-apple-darwin/rustup-init`) together with its published `.sha256` file, and verify the checksum before running it.
2. Run it with the toolchain homes pointed into this directory:

   ```sh
   RUSTUP_HOME=tools/rust/rustup CARGO_HOME=tools/rust/cargo \
     ./rustup-init -y --no-modify-path --profile minimal --default-toolchain 1.98.1 --target wasm32-unknown-unknown
   ```

3. Keep the download, its hash and the installed versions in a provenance note of your own (for example `tools/rust-provenance.json`; it is ignored by Git like everything else here).

Use absolute paths for `RUSTUP_HOME` and `CARGO_HOME` if your shell does not expand the relative ones. Do not commit anything from `tools/`.
