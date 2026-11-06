# Install a release binary

The first campaign pilots used **Linux x86_64**. That is the only advertised
binary target: `x86_64-unknown-linux-gnu`, built natively on Ubuntu 22.04 with
Rust 1.95, default features and `Cargo.lock`. It requires glibc 2.35 or newer,
the normal `libgcc_s.so.1` runtime, system CA certificates for HTTPS, and a POSIX
shell for `run_command` (Ubuntu/Debian package: `ca-certificates`).
No Rust, Python or Node installation is required to run the agent.
Alpine/musl, ARM, macOS and Windows binaries are not supplied or verified.

## Download, check and install

You need `curl`, `sha256sum` and `tar` for installation. Run in a shell:

```sh
version=0.10.0
archive="acp-llm-adapter-v$version-x86_64-unknown-linux-gnu.tar.gz"
release_url="https://github.com/euri10/acp-llm-adapter/releases/download/v$version"
download_dir=$(mktemp -d)
curl -fL "$release_url/$archive" -o "$download_dir/$archive" &&
curl -fL "$release_url/$archive.sha256" -o "$download_dir/$archive.sha256" &&
(cd "$download_dir" && sha256sum --check "$archive.sha256") &&
  tar -xzf "$download_dir/$archive" -C "$download_dir" &&
  mkdir -p "$HOME/.local/bin" &&
  install -m 755 "$download_dir/acp-llm-adapter" "$HOME/.local/bin/acp-llm-adapter" &&
export PATH="$HOME/.local/bin:$PATH" &&
acp-llm-adapter --version
```

Expected version: `acp-llm-adapter 0.10.0`. The archive also includes the MIT
license and `SOURCE.txt` with the release tag, exact source commit, compiler,
target, features and lockfile SHA-256. Checksums detect a damaged download;
they are not a separate cryptographic signature.

Keep `$HOME/.local/bin` in the environment that launches your editor or
cc-connect; for a GUI editor you can instead configure the absolute executable
path. Update by selecting a published version from
[Releases](https://github.com/euri10/acp-llm-adapter/releases) and repeating the
same verification/install steps, then restarting the agent. The executable
stays at `$HOME/.local/bin/acp-llm-adapter`.

## Key and client setup

Set your provider key securely in the launching environment. The agent reads
`LLM_API_KEY`; the [editor recipes](../README.md#editor-setup),
[agentic.nvim guide](agentic.md), and [cc-connect guide](cc-connect.md) map
`DEEPSEEK_API_KEY` to it. Do not put keys in a downloaded archive, public
configuration, shell history or feedback report. Select `serve --backend deepseek`,
`glm`, or `groq` in the client. The [mock backend](mock-backend.md) needs no key.

The binary includes the same default ACP v1 / experimental v2 support as a
Cargo install. Binary smoke checks use ACP v1 and the offline mock backend;
they do not establish live-provider or additional editor compatibility.

## Cargo alternative

With Rust 1.95 or newer:

```sh
cargo install --locked --version 0.10.0 acp-llm-adapter
acp-llm-adapter --version
```

For development in a source checkout, `cargo build --locked --bin acp-llm-adapter`
places the executable in `target/debug/`. The downloadable archive contains
the main agent only; use Cargo if you also need the `acp-proxy` debugging tool.

## Release verification and maintenance

The existing [Release-plz workflow](../.github/workflows/release-plz.yml) builds
the exact published tag, packages the executable and checksum, checks them in
a clean Ubuntu 22.04 Docker runtime, uploads them, then downloads and checks
the published assets again. The runtime has no Rust/Python/Node, no provider
key and no network. The host-side checker drives real ACP initialize/session
requests, rejects one command, approves another, checks disk effects and
continuation, then waits for graceful process exit. This is native runtime
verification, not merely cross-compilation.

On **2026-11-06**, [release run 42187690080](https://github.com/euri10/acp-llm-adapter/actions/runs/42187690080)
published `v0.10.0` from commit `844bfbb0d6404d1dd6c4fa0d73885be10cd97204`.
An independent public download passed the same mock workflow in Ubuntu 22.04
and Debian 13 containers, plus the `sha256sum` / `tar` / `install` path above.
Archive SHA-256:

```text
8e4584410588767cd4546126e4fe8bab0b77e422f9ab5ded8d3a527bcb3ed3c4
```

To repeat that check from a checkout, with Python 3 and Docker on the verifier
host (neither is required by the installed agent):

```sh
python3 scripts/check-release-binary \
  "$download_dir/$archive" "$download_dir/$archive.sha256" "$version"
```

Future Release-plz releases attach this same single target automatically.
Maintainers can backfill a published stable tag with
`gh workflow run release-plz.yml --ref main -f release_tag=v0.10.0`.
This runs only the binary job; it does not create a new crate version or release
PR. Existing assets are never overwritten. If an upload partially succeeds,
inspect the existing asset and failure before retrying; do not delete or
replace versioned binaries blindly.

CI also runs `python3 scripts/test-release-binary-timeout`: a deliberately hung
version fixture must time out without leaving a container behind. This is a
checker cleanup regression, separate from the published binary's successful
runtime evidence.
