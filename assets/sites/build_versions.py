#!/usr/bin/env python3
"""Build tagged documentation or refresh main, preserving other published versions."""
from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

SITE_ROOT = Path(__file__).resolve().parent
REPO_ROOT = SITE_ROOT.parents[1]
MIN_VERSION = (0, 3, 0)


def release_tags(tags: list[str]) -> list[str]:
    """Select stable release tags, newest first (numeric, not lexical order)."""
    versions = []
    for tag in tags:
        match = re.fullmatch(r"v(\d+)\.(\d+)\.(\d+)", tag)
        if match:
            version = tuple(map(int, match.groups()))
            if version >= MIN_VERSION:
                versions.append((version, tag))
    return [tag for _, tag in sorted(versions, reverse=True)]


def run(*args: str, cwd: Path = REPO_ROOT, **kwargs) -> subprocess.CompletedProcess:
    return subprocess.run(args, cwd=cwd, check=True, **kwargs)


def build_version(ref: str, version: str, branch: str, *, latest: bool = False) -> None:
    # Use today's renderer with that revision's Rust sources and protocol. Early
    # releases do not contain the site tooling (or the user-management API).
    with tempfile.TemporaryDirectory(prefix="shadowquic-docs-") as temp:
        worktree = Path(temp) / "source"
        run("git", "worktree", "add", "--detach", str(worktree), ref)
        try:
            site = worktree / "assets" / "sites"
            if site.exists():
                shutil.rmtree(site)
            site.mkdir(parents=True)
            for name in ("gen_docs.py", "zensical.toml"):
                shutil.copy2(SITE_ROOT / name, site / name)
            run(
                sys.executable, str(site / "gen_docs.py"), "--clean",
                "--source-ref", "main" if version == "main" else ref,
                cwd=worktree,
            )
            command = [
                "mike", "deploy", "--config-file", str(site / "zensical.toml"),
                "--branch", branch, "--alias-type", "redirect", "--update-aliases",
                "--title", version, version,
            ]
            if latest:
                command.append("latest")
            run(*command, cwd=worktree)
            if version == "main":
                # Keep latest as the default on main-only updates. A first
                # deployment without any release pages defaults to main.
                versions = json.loads(run(
                    "git", "show", f"{branch}:versions.json",
                    capture_output=True, text=True,
                ).stdout)
                has_latest = any("latest" in v["aliases"] for v in versions)
                run(
                    "mike", "set-default", "--config-file", str(site / "zensical.toml"),
                    "--branch", branch, "latest" if has_latest else "main", cwd=worktree,
                )
        finally:
            run("git", "worktree", "remove", "--force", str(worktree))


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mode", choices=("releases", "main"), required=True)
    parser.add_argument("--main-ref", default="origin/main")
    parser.add_argument("--branch", default="gh-pages")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    # Reuse dependencies; any Cargo lockfile updates stay in temporary checkouts.
    os.environ.setdefault("CARGO_TARGET_DIR", str(REPO_ROOT / "target" / "docs-versions"))
    if args.mode == "releases":
        tag_list = run("git", "tag", "--list", capture_output=True, text=True).stdout
        tags = release_tags(tag_list.splitlines())
        if not tags:
            parser.error("no stable release tags at or after v0.3.0")
        for tag in reversed(tags):
            build_version(tag, tag.removeprefix("v"), args.branch, latest=tag == tags[0])

    build_version(args.main_ref, "main", args.branch)
    args.output.mkdir(parents=True, exist_ok=True)
    # git archive excludes repository metadata. Redirect aliases avoid symlinks,
    # which GitHub Pages artifacts do not support.
    with tempfile.TemporaryFile() as archive:
        run("git", "archive", args.branch, stdout=archive)
        archive.seek(0)
        run("tar", "-xf", "-", "-C", str(args.output.resolve()), stdin=archive)


if __name__ == "__main__":
    main()
