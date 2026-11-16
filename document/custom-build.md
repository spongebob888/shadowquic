# Customized GitHub Actions builds

Open **Actions → Custom build → Run workflow** in this repository (write access
required), or in your own fork. Select the branch/tag and target environment,
then configure:

- **features**: comma- or space-separated Cargo features; blank uses the defaults.
- **no-default-features**: disable defaults and build with only the selected features.
- **profile**: `debug` or `release`.
- **opt-level**: `0`, `1`, `2`, `3`, `s` (size), or `z` (size without loop vectorization).
  `profile` keeps the profile default (`debug=0`, `release=3`).
- **debug-info**: `none`, `line-tables-only`, `limited`, or `full`. `profile` keeps
  the profile default (`debug=full`, `release=none`). Selecting debug information
  preserves symbols even for release builds.

For example, enable **no-default-features** and enter `shadowquic-quinn,ring,mixed`
for a minimal ShadowQUIC build with a mixed inbound. See the
[Cargo feature list](../shadowquic/Cargo.toml) for available features. Feature and
platform requirements still apply; use `plugin-vendored` for bundled Lua instead
of `plugin-system`, which requires a system Lua installation.

When the run succeeds, download its **Artifacts** archive and extract the enclosed
`.tar.gz`. It contains the binary, any Windows/macOS debug symbols, the license,
and the build settings. Artifacts are retained for 14 days.
