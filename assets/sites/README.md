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
- [typst](https://github.com/typst/typst) (`brew install typst`) — used to
  render `PROTOCOL.typ` to per-page SVGs that are embedded in the site.
  Pass `--skip-protocol` to `gen_docs.py` if you don't have it installed.

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
4. If `RouterCfg` is reachable from `Config`, it gets a dedicated
   `configuration/router.md` page and a **Configuration → Router** navigation
   entry. The overview's `router` field links to it. Older versions without
   `RouterCfg` omit this entry.
5. The `nav` block of `zensical.toml` is rewritten between
   `# >>> generated nav` / `# <<< generated nav` markers.

If you add a new variant to `InboundCfg` / `OutboundCfg` or a new config
struct, just re-run the generator — no manual nav edits.

## Documentation versions

The header version selector lists every stable `vMAJOR.MINOR.PATCH` tag from
`v0.3.0` onward, plus `main`. Versions are sorted numerically; the newest release
uses its version number as the title, with a trailing `latest` alias marker. The site root redirects
to `latest`, while `main/` always documents the main branch.

Versioning uses the [Zensical-compatible mike fork](https://zensical.org/docs/compatibility/mkdocs/mike/),
pinned in `uv.lock`. The build uses today's site generator with each tag's Rust
sources, protocol, and API reference. Cargo may refresh old lockfiles inside the
temporary checkouts when required by the current toolchain. Historical releases
without an API reference omit that section. Source links point to the matching tag.

The documentation workflow has two modes:

- A release tag push (`v*`) rebuilds **all** stable versions since `v0.3.0`, plus
  `main`, then updates the `latest` alias. Manual dispatch also runs this mode,
  which can bootstrap the complete history before the next release.
- A normal push to `main` rebuilds **only main**, preserving the release pages
  and `latest` alias already stored on `gh-pages`. Before the first release build,
  the site root defaults to `main`.

`gh-pages` is the persistent build archive. CI pushes it only after all builds
succeed, and deploys its complete contents using a GitHub Pages artifact. Set the
repository's Pages source to **GitHub Actions**. Both workflow modes share one
concurrency group so updates cannot overwrite each other.

To build the complete archive locally (requires Git author configuration):

```sh
uv run --locked --project assets/sites python assets/sites/build_versions.py \
  --mode releases --main-ref origin/main --branch docs-preview --output /tmp/docs-preview
```

Use `--mode main` to update only main. These commands create local commits on the
specified archive branch; they do not push or deploy. Use a fresh output directory
when exporting the archive. To preview the complete version selector:

```sh
python -m http.server --directory /tmp/docs-preview 8000
```
