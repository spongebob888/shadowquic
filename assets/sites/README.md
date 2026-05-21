# shadowquic documentation site

A Zensical static site whose configuration reference is generated from the
doc comments on the structs and enums in `shadowquic/src/config/`.

The generator (`gen_docs.py`) drives `cargo +nightly rustdoc` to emit JSON,
then walks the `Config` / `InboundCfg` / `OutboundCfg` type graph and writes
per-type markdown pages plus an updated `nav = [...]` block in `zensical.toml`.

## Layout

```
assets/sites/
├── README.md          this file
├── pyproject.toml     python deps (managed by uv)
├── uv.lock            locked dep versions (committed)
├── zensical.toml      site config; `nav` is rewritten by the generator
├── gen_docs.py        rustdoc JSON -> markdown generator
└── docs/              generated markdown (overwritten on each run)
    ├── index.md
    └── configuration/
        ├── index.md
        ├── inbound/
        ├── outbound/
        └── shared/
```

## Prerequisites

- A nightly Rust toolchain (`rustup install nightly`) — rustdoc's JSON
  output is nightly-only.
- [uv](https://docs.astral.sh/uv/) for Python tooling.

## Regenerate and preview

All commands are run from `assets/sites/`. `uv run` lazily provisions
`.venv/` from `pyproject.toml` + `uv.lock` on first use; no manual install
step is needed.

```sh
cd assets/sites

# Regenerate markdown + nav from the Rust sources.
uv run python gen_docs.py

# Live preview at http://localhost:8000
uv run zensical serve

# Or, produce a static site under assets/sites/site/
uv run zensical build
```

The generator also works from the repo root:

```sh
uv --project assets/sites run python assets/sites/gen_docs.py
```

## How it works

1. `cargo +nightly rustdoc -p shadowquic --lib -- -Z unstable-options --output-format json`
   writes `target/doc/shadowquic.json`.
2. The generator filters the rustdoc `index` to items whose source path is
   under `shadowquic/src/config/`, classifies each as a struct or enum, and
   resolves serde attributes (`rename_all`, `default`, `tag`, ...) to render
   the on-the-wire YAML field name.
3. For each top-level type a markdown page is written under
   `docs/configuration/...`; field types that resolve to another config
   type become internal links.
4. The `nav` block of `zensical.toml` is rewritten between
   `# >>> generated nav` / `# <<< generated nav` markers.

If you add a new variant to `InboundCfg` / `OutboundCfg` or a new config
struct, just re-run the generator — no manual nav edits.
