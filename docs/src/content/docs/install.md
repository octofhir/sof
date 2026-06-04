---
title: Install
description: Install octofhir-sof on macOS, Linux or Windows.
---

## Prebuilt binary (recommended)

The installer detects your platform and verifies the download's SHA-256:

```sh
curl -fsSL https://raw.githubusercontent.com/octofhir/sof/main/install.sh | sh
```

It installs the `octofhir-sof` binary to `~/.local/bin` by default; override the
destination with `OCTOFHIR_SOF_INSTALL_DIR`:

```sh
OCTOFHIR_SOF_INSTALL_DIR=/usr/local/bin \
  curl -fsSL https://raw.githubusercontent.com/octofhir/sof/main/install.sh | sh
```

Prebuilt archives are published for `x86_64`/`aarch64` Linux (gnu),
`x86_64`/`arm64` macOS, and `x86_64` Windows. Windows users can download the
`.zip` from the
[releases page](https://github.com/octofhir/sof/releases/latest) and extract
`octofhir-sof.exe`.

## With a Rust toolchain

```sh
cargo install octofhir-sof-cli                 # build from source via crates.io
cargo install octofhir-sof-cli --features parquet   # + Parquet output
```

The installed binary is named `octofhir-sof`.

## From source

```sh
git clone https://github.com/octofhir/sof
cd sof
cargo build --release -p octofhir-sof-cli                    # the CLI binary
cargo build --release -p octofhir-sof-cli --features parquet # + Parquet output
```

The binary is `target/release/octofhir-sof`. The library
(`octofhir-sof`) is network-free; the lint crate (`octofhir-sof-lint`)
resolves FHIR packages through the canonical manager only when you pass
`lint --package`.
