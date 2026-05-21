#!/usr/bin/env python3
"""Generate shadowquic's Zensical configuration reference from rustdoc JSON.

Pipeline:

    cargo +nightly rustdoc -p shadowquic --lib                 \
        -- -Z unstable-options --output-format json
                          |
                          v
              target/doc/shadowquic.json
                          |
                          v
            walk Config / InboundCfg / OutboundCfg
                          |
                          v
         assets/sites/docs/configuration/**/*.md
         assets/sites/zensical.toml  (nav block rewritten in place)
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
from dataclasses import dataclass, field
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
SITE_ROOT = Path(__file__).resolve().parent
DOCS_ROOT = SITE_ROOT / "docs"
CONFIG_SRC_PREFIX = "shadowquic/src/config/"

# ----------------------------------------------------------------------------
# rustdoc invocation
# ----------------------------------------------------------------------------


def build_rustdoc_json(repo_root: Path) -> Path:
    """Invoke `cargo +nightly rustdoc` and return the JSON path."""
    out = repo_root / "target" / "doc" / "shadowquic.json"
    cmd = [
        "cargo",
        "+nightly",
        "rustdoc",
        "-p",
        "shadowquic",
        "--lib",
        "--",
        "-Z",
        "unstable-options",
        "--output-format",
        "json",
    ]
    print(f"$ {' '.join(cmd)}", file=sys.stderr)
    res = subprocess.run(cmd, cwd=repo_root)
    if res.returncode != 0:
        raise SystemExit(f"cargo rustdoc failed with exit {res.returncode}")
    if not out.exists():
        raise SystemExit(f"expected {out} to exist after rustdoc run")
    return out


# ----------------------------------------------------------------------------
# rustdoc JSON helpers
# ----------------------------------------------------------------------------


@dataclass
class Item:
    raw: dict

    @property
    def id(self) -> str:
        return str(self.raw["id"])

    @property
    def name(self) -> str | None:
        return self.raw.get("name")

    @property
    def docs(self) -> str:
        return self.raw.get("docs") or ""

    @property
    def links(self) -> dict[str, int]:
        return self.raw.get("links") or {}

    @property
    def attrs(self) -> list[str]:
        out: list[str] = []
        for a in self.raw.get("attrs", []) or []:
            if isinstance(a, dict):
                # rustdoc represents attrs as {"other": "..."} for unknown attrs
                if "other" in a:
                    out.append(a["other"])
            elif isinstance(a, str):
                out.append(a)
        return out

    @property
    def filename(self) -> str:
        return ((self.raw.get("span") or {}).get("filename")) or ""

    @property
    def inner(self) -> dict:
        return self.raw.get("inner") or {}

    @property
    def is_config(self) -> bool:
        return self.filename.startswith(CONFIG_SRC_PREFIX)

    @property
    def kind(self) -> str:
        for k in ("struct", "enum", "struct_field", "variant", "function", "module"):
            if k in self.inner:
                return k
        return next(iter(self.inner)) if self.inner else ""


def load_index(json_path: Path) -> dict[str, Item]:
    data = json.loads(json_path.read_text())
    return {str(k): Item(v) for k, v in data["index"].items()}


# ----------------------------------------------------------------------------
# serde attribute parsing
# ----------------------------------------------------------------------------


@dataclass
class SerdeContainer:
    rename_all: str | None = None
    tag: str | None = None
    deny_unknown_fields: bool = False


@dataclass
class SerdeField:
    rename: str | None = None
    has_default: bool = False
    default_fn: str | None = None
    flatten: bool = False
    skip: bool = False


_RENAME_ALL_RE = re.compile(r'rename_all\s*=\s*"([^"]+)"')
_RENAME_RE = re.compile(r'\brename\s*=\s*"([^"]+)"')
_TAG_RE = re.compile(r'\btag\s*=\s*"([^"]+)"')
_DEFAULT_FN_RE = re.compile(r'\bdefault\s*=\s*"([^"]+)"')


def parse_container_serde(attrs: list[str]) -> SerdeContainer:
    out = SerdeContainer()
    for a in attrs:
        if "serde" not in a:
            continue
        m = _RENAME_ALL_RE.search(a)
        if m:
            out.rename_all = m.group(1)
        m = _TAG_RE.search(a)
        if m:
            out.tag = m.group(1)
        if "deny_unknown_fields" in a:
            out.deny_unknown_fields = True
    return out


def parse_field_serde(attrs: list[str]) -> SerdeField:
    out = SerdeField()
    for a in attrs:
        if "serde" not in a:
            continue
        m = _RENAME_RE.search(a)
        if m:
            out.rename = m.group(1)
        m = _DEFAULT_FN_RE.search(a)
        if m:
            out.has_default = True
            out.default_fn = m.group(1)
        elif re.search(r'\bdefault\b', a):
            out.has_default = True
        if "flatten" in a:
            out.flatten = True
        if "skip" in a:
            out.skip = True
    return out


_PASCAL_SPLIT_RE = re.compile(r"(?<!^)(?=[A-Z])")


def _to_snake(name: str) -> str:
    # snake_case input stays snake_case; PascalCase becomes pascal_case.
    if "_" in name or name.islower():
        return name.lower()
    return _PASCAL_SPLIT_RE.sub("_", name).lower()


def apply_rename_all(name: str, rule: str | None) -> str:
    if rule is None:
        return name
    snake = _to_snake(name)
    if rule == "kebab-case":
        return snake.replace("_", "-")
    if rule == "snake_case":
        return snake
    if rule == "SCREAMING_SNAKE_CASE":
        return snake.upper()
    if rule == "SCREAMING-KEBAB-CASE":
        return snake.replace("_", "-").upper()
    if rule == "lowercase":
        return name.lower()
    if rule == "UPPERCASE":
        return name.upper()
    if rule == "PascalCase":
        return "".join(p.title() for p in snake.split("_"))
    if rule == "camelCase":
        parts = snake.split("_")
        return parts[0] + "".join(p.title() for p in parts[1:])
    return name


# ----------------------------------------------------------------------------
# default value extraction (from source)
# ----------------------------------------------------------------------------


# Matches:  pub fn name() -> T { expr }   and   pub(crate) fn name() -> T { expr }
# and any other `pub(...)` visibility modifier.
_DEFAULT_BODY_RE = re.compile(
    r"pub(?:\s*\([^)]*\))?\s+fn\s+(\w+)\s*\(\s*\)\s*->\s*[^\{]+\{\s*([^\n]+?)\s*\}",
    re.MULTILINE,
)


def collect_enum_variant_tags(
    index: dict[str, Item], src: SourceAttrs
) -> tuple[dict[str, str], dict[int, str]]:
    """Walk every config enum and return two maps.

    - `EnumName::VariantName -> serde tag` (used to translate Rust expressions
      like `CongestionControl::Bbr` into the on-the-wire YAML value).
    - `enum rustdoc id -> default variant tag`, populated from the
      `#[default]` attribute on the variant. Used when a struct field has
      `#[serde(default)]` and we want to show what value it resolves to.
    """
    variant_tags: dict[str, str] = {}
    enum_defaults_by_id: dict[int, str] = {}
    for item in index.values():
        if not item.is_config or "enum" not in item.inner:
            continue
        container = parse_container_serde(attrs_for_container(item, src))
        for vid in item.inner["enum"]["variants"]:
            v = index.get(str(vid))
            if v is None or not v.name:
                continue
            v_attrs = attrs_for_member(v, item.name, src)
            vserde = parse_field_serde(v_attrs)
            tag = vserde.rename or apply_rename_all(v.name, container.rename_all)
            variant_tags[f"{item.name}::{v.name}"] = tag
            if any("#[default]" in a or a.strip() == "#[default]" for a in v_attrs):
                enum_defaults_by_id[int(item.id)] = tag
    return variant_tags, enum_defaults_by_id


def collect_default_fn_values(repo_root: Path) -> dict[str, str]:
    """Scan src/config/*.rs for `pub fn default_xxx() -> T { expr }` bodies.

    Only matches single-line function bodies; anything more complex falls back
    to displaying the function name in the rendered docs.
    """
    out: dict[str, str] = {}
    for path in (repo_root / "shadowquic" / "src" / "config").glob("*.rs"):
        text = path.read_text()
        for m in _DEFAULT_BODY_RE.finditer(text):
            out[m.group(1)] = m.group(2).rstrip(";").strip()
    return out


# ----------------------------------------------------------------------------
# source-side attribute scan
# ----------------------------------------------------------------------------
#
# Recent nightly rustdoc no longer surfaces non-rustc attributes (`#[serde(...)]`,
# `#[default]`, etc.) in the JSON `attrs` field. Without those, every variant
# falls back to its PascalCase Rust name. We recover the attributes by parsing
# the source files directly with a deliberately small line-by-line state
# machine. Good enough for the well-formed config module we own; not a general
# Rust parser.


@dataclass
class SourceAttrs:
    container: dict[str, list[str]] = field(default_factory=dict)
    member: dict[tuple[str, str], list[str]] = field(default_factory=dict)


def _strip_line_comment(line: str) -> str:
    # Naive but adequate for our config files (no `//` inside string literals).
    idx = line.find("//")
    return line if idx < 0 else line[:idx]


def parse_source_attrs(repo_root: Path) -> SourceAttrs:
    """Return container & member attribute strings recovered from src/config/*.rs."""
    out = SourceAttrs()

    container_decl = re.compile(
        r"^\s*(?:pub(?:\s*\([^)]*\))?\s+)?(struct|enum)\s+([A-Za-z_]\w*)\b"
    )
    field_decl = re.compile(r"^\s*(?:pub(?:\s*\([^)]*\))?\s+)?([a-z_]\w*)\s*:")
    variant_decl = re.compile(r"^\s*([A-Z][A-Za-z0-9_]*)\s*[\(\{,=]?")

    for path in (repo_root / "shadowquic" / "src" / "config").glob("*.rs"):
        text = path.read_text()
        pending: list[str] = []
        container_name: str | None = None
        container_kind: str | None = None
        body_depth = -1  # brace depth where the container body lives
        depth = 0  # brace depth at *start* of current line

        for raw in text.split("\n"):
            line = _strip_line_comment(raw)
            stripped = line.strip()
            opens = line.count("{")
            closes = line.count("}")

            if not stripped:
                depth += opens - closes
                continue

            if stripped.startswith("#["):
                pending.append(stripped)
                depth += opens - closes
                continue

            if container_name is None:
                m = container_decl.match(stripped)
                if m:
                    container_kind = m.group(1)
                    container_name = m.group(2)
                    out.container[container_name] = pending[:]
                    pending.clear()
                    body_depth = depth + 1
                else:
                    # Anything else at the top level invalidates pending attrs.
                    pending.clear()
            else:
                # Inside a container: look for member decls at the body's depth.
                if depth == body_depth:
                    if container_kind == "struct":
                        m = field_decl.match(stripped)
                        if m and m.group(1) not in ("pub",):
                            out.member[(container_name, m.group(1))] = pending[:]
                            pending.clear()
                    elif container_kind == "enum":
                        m = variant_decl.match(stripped)
                        if m:
                            out.member[(container_name, m.group(1))] = pending[:]
                            pending.clear()

            depth += opens - closes

            # Pop out of the container.
            if container_name is not None and depth < body_depth:
                container_name = None
                container_kind = None
                body_depth = -1
                pending.clear()

    return out


def attrs_for_container(item: Item, src: SourceAttrs) -> list[str]:
    if item.attrs:
        return item.attrs
    if item.name and item.name in src.container:
        return src.container[item.name]
    return []


def attrs_for_member(
    member: Item, container_name: str | None, src: SourceAttrs
) -> list[str]:
    if member.attrs:
        return member.attrs
    if container_name and member.name:
        return src.member.get((container_name, member.name), [])
    return []


# ----------------------------------------------------------------------------
# type rendering
# ----------------------------------------------------------------------------


# Short labels for common standard paths that come back as resolved_path
STD_TYPE_LABELS = {
    "std::net::SocketAddr": "SocketAddr",
    "std::net::IpAddr": "IpAddr",
    "std::path::PathBuf": "path",
    "String": "string",
}


@dataclass
class RenderContext:
    """Global state used by the markdown emitters."""

    index: dict[str, Item]
    page_for_type: dict[int, str]  # type id -> markdown path (relative to docs/)
    default_values: dict[str, str]
    pages_dir_for: dict[int, Path]  # type id -> directory containing the page
    src_attrs: SourceAttrs = field(default_factory=SourceAttrs)
    enum_variant_tags: dict[str, str] = field(default_factory=dict)
    # ^ "EnumName::VariantName" -> kebab-case (or whatever serde tag) value
    enum_defaults_by_id: dict[int, str] = field(default_factory=dict)
    # ^ rustdoc enum id -> tag of the variant marked `#[default]`

    def link_for_type(self, type_id: int, from_page: Path) -> str | None:
        """Return a relative href from `from_page` to the page documenting type_id."""
        target = self.page_for_type.get(type_id)
        if target is None:
            return None
        # Both are relative to DOCS_ROOT.
        rel = os.path.relpath(target, start=str(from_page.parent))
        return rel.replace(os.sep, "/")


_ENUM_VARIANT_EXPR_RE = re.compile(r"^([A-Z][A-Za-z0-9_]*)::([A-Z][A-Za-z0-9_]*)$")


def render_default_value(expr: str, ctx: RenderContext) -> str:
    """Pretty-print a default expression for display in the YAML schema.

    Currently only rewrites `EnumName::VariantName` constructors into their
    serde tag value (`CongestionControl::Bbr` -> `bbr`). Anything else is
    returned unchanged.
    """
    if not expr:
        return expr
    m = _ENUM_VARIANT_EXPR_RE.match(expr.strip())
    if m:
        key = f"{m.group(1)}::{m.group(2)}"
        if key in ctx.enum_variant_tags:
            return ctx.enum_variant_tags[key]
    return expr


def render_type(ty: dict, ctx: RenderContext, from_page: Path) -> str:
    """Render a rustdoc Type (single field type) as a short markdown snippet."""
    if "primitive" in ty:
        return f"`{ty['primitive']}`"
    if "resolved_path" in ty:
        rp = ty["resolved_path"]
        path = rp.get("path") or ""
        type_id = rp.get("id")
        args = rp.get("args")

        # Local crate path: prefer the trailing name.
        short = path.split("::")[-1]

        # Generics
        inner_args: list[dict] = []
        if isinstance(args, dict) and "angle_bracketed" in args:
            for a in args["angle_bracketed"].get("args", []):
                if isinstance(a, dict) and "type" in a:
                    inner_args.append(a["type"])

        if short == "Option" and len(inner_args) == 1:
            return f"{render_type(inner_args[0], ctx, from_page)} *(optional)*"
        if short == "Vec" and len(inner_args) == 1:
            return f"list of {render_type(inner_args[0], ctx, from_page)}"

        # Display label (std types get friendlier names).
        label = STD_TYPE_LABELS.get(path, short)

        # If this resolves to a documented config type, link to its page.
        if type_id is not None:
            href = ctx.link_for_type(int(type_id), from_page)
            if href:
                return f"[`{label}`]({href})"
        return f"`{label}`"

    if "generic" in ty:
        return f"`{ty['generic']}`"
    if "borrowed_ref" in ty:
        return render_type(ty["borrowed_ref"]["type"], ctx, from_page)
    return f"`{json.dumps(ty)[:60]}`"


# ----------------------------------------------------------------------------
# doc-comment link rewriting
# ----------------------------------------------------------------------------


_LIST_ITEM_RE = re.compile(r"^\s*(?:[-*+]\s|\d+\.\s)")
_HEADING_RE = re.compile(r"^\s*#{1,6}\s")
_FENCE_RE = re.compile(r"^\s*(```|~~~)")


def normalize_block_breaks(docs: str) -> str:
    """Insert blank lines so rustdoc `///` blocks parse as proper markdown.

    Rust developers commonly write::

        /// Default is `foo`.
        /// - first
        /// - second

    which strict CommonMark renders as one paragraph because there is no blank
    line between the prose and the list. This walks the text and inserts a
    blank line before any list item, heading, or fenced code block whose
    previous non-blank line is *not* itself part of the same block.
    """
    if not docs:
        return docs

    out: list[str] = []
    in_fence = False
    prev_kind = "blank"  # blank | text | list | fence | heading

    for line in docs.splitlines():
        is_fence = bool(_FENCE_RE.match(line))
        if in_fence:
            out.append(line)
            if is_fence:
                in_fence = False
                prev_kind = "blank"  # closing fence acts as a hard break
            else:
                prev_kind = "fence"
            continue

        is_list = bool(_LIST_ITEM_RE.match(line))
        is_heading = bool(_HEADING_RE.match(line))
        is_blank = line.strip() == ""

        def insert_break_if_needed(prev: str, current: str) -> None:
            if prev == "blank":
                return
            if current == "list" and prev == "list":
                return
            out.append("")

        if is_fence:
            insert_break_if_needed(prev_kind, "fence")
            out.append(line)
            in_fence = True
            prev_kind = "fence"
        elif is_list:
            insert_break_if_needed(prev_kind, "list")
            out.append(line)
            prev_kind = "list"
        elif is_heading:
            insert_break_if_needed(prev_kind, "heading")
            out.append(line)
            prev_kind = "heading"
        elif is_blank:
            out.append(line)
            prev_kind = "blank"
        else:
            out.append(line)
            prev_kind = "text"

    return "\n".join(out)


def rewrite_doc_links(
    docs: str,
    links: dict[str, int],
    ctx: RenderContext,
    from_page: Path,
) -> str:
    """Resolve `[`TypeName`]` references in a doc comment to real links.

    Also normalizes blank lines around block elements so rustdoc-style
    comments (no blank line before lists/fences) render correctly.
    """
    if not docs:
        return docs

    out = normalize_block_breaks(docs)

    if links:
        appended_refs: list[str] = []
        for label, type_id in links.items():
            href = ctx.link_for_type(int(type_id), from_page)
            if not href:
                continue
            # Inline-rewrite `[`TypeName`]` -> `[`TypeName`](href)`. The label
            # already includes the surrounding backticks.
            pat = re.compile(r"\[" + re.escape(label) + r"\](?!\()")
            out = pat.sub(f"[{label}]({href})", out)
            # As a safety net, also queue a reference definition.
            appended_refs.append(f"[{label}]: {href}")
        if appended_refs:
            out = out + "\n\n" + "\n".join(appended_refs) + "\n"
    return out


# ----------------------------------------------------------------------------
# emitters
# ----------------------------------------------------------------------------


def emit_struct_page(
    item: Item,
    page_path: Path,
    title: str,
    ctx: RenderContext,
    extra_intro: str = "",
) -> str:
    """Return the markdown body for a struct page."""
    serde = parse_container_serde(attrs_for_container(item, ctx.src_attrs))
    body: list[str] = []
    body.append(f"# {title}\n")
    if extra_intro:
        body.append(extra_intro.rstrip() + "\n")

    docs_md = rewrite_doc_links(item.docs, item.links, ctx, page_path)
    if docs_md:
        body.append(docs_md.rstrip() + "\n")

    body.append("## Fields\n")

    fields = item.inner["struct"]["kind"]["plain"]["fields"]
    for fid in fields:
        f = ctx.index.get(str(fid))
        if f is None:
            continue
        fserde = parse_field_serde(attrs_for_member(f, item.name, ctx.src_attrs))
        if fserde.skip:
            continue
        yaml_name = fserde.rename or apply_rename_all(f.name or "", serde.rename_all)

        ty = f.inner.get("struct_field")
        type_md = render_type(ty, ctx, page_path) if isinstance(ty, dict) else "`?`"

        is_option_type = False
        if isinstance(ty, dict) and "resolved_path" in ty:
            is_option_type = (ty["resolved_path"].get("path") or "").split("::")[-1] == "Option"

        required = not (fserde.has_default or is_option_type)
        required_md = "no" if not required else "**yes**"

        # Per-field section: heading + metadata list + full-markdown description.
        # A heading per field is the only reliable way to render multi-paragraph
        # content (notably fenced code blocks) inside the docs.
        body.append(f"### `{yaml_name}`\n")
        meta: list[str] = [f"- **Type:** {type_md}", f"- **Required:** {required_md}"]
        if fserde.default_fn:
            val = ctx.default_values.get(fserde.default_fn)
            display = render_default_value(val, ctx) if val else f"{fserde.default_fn}()"
            meta.append(f"- **Default:** `{display}`")
        elif fserde.has_default and not is_option_type:
            # `#[serde(default)]` -> use the type's Default impl. For enums we
            # parsed `#[default]` on a variant, so we know the resolved value.
            enum_default = None
            if isinstance(ty, dict) and "resolved_path" in ty:
                tid = ty["resolved_path"].get("id")
                if tid is not None:
                    enum_default = ctx.enum_defaults_by_id.get(int(tid))
            if enum_default is not None:
                meta.append(f"- **Default:** `{enum_default}`")
            else:
                meta.append("- **Default:** *(type default)*")
        body.append("\n".join(meta) + "\n")

        desc_md = rewrite_doc_links(f.docs or "", f.links, ctx, page_path).rstrip()
        if desc_md:
            body.append(desc_md + "\n")

    return "\n".join(body) + "\n"


def emit_enum_page(
    item: Item,
    page_path: Path,
    title: str,
    ctx: RenderContext,
    extra_intro: str = "",
) -> str:
    serde = parse_container_serde(attrs_for_container(item, ctx.src_attrs))
    body: list[str] = []
    body.append(f"# {title}\n")
    if extra_intro:
        body.append(extra_intro.rstrip() + "\n")

    docs_md = rewrite_doc_links(item.docs, item.links, ctx, page_path)
    if docs_md:
        body.append(docs_md.rstrip() + "\n")

    body.append("## Variants\n")
    if serde.tag:
        body.append(
            f"This enum is serialized with the YAML key **`{serde.tag}`** "
            f"selecting the variant.\n"
        )
    body.append("| Tag value | Carries | Description |")
    body.append("|-----------|---------|-------------|")

    for vid in item.inner["enum"]["variants"]:
        v = ctx.index.get(str(vid))
        if v is None:
            continue
        vserde = parse_field_serde(attrs_for_member(v, item.name, ctx.src_attrs))
        tag_name = vserde.rename or apply_rename_all(v.name or "", serde.rename_all)

        # Inner type for tuple variants
        kind = v.inner["variant"]["kind"]
        carries_md = "—"
        if isinstance(kind, dict) and "tuple" in kind:
            tup = kind["tuple"]
            if tup:
                inner_field = ctx.index.get(str(tup[0]))
                if inner_field is not None:
                    ty = inner_field.inner.get("struct_field")
                    if isinstance(ty, dict):
                        carries_md = render_type(ty, ctx, page_path)

        desc_md = (v.docs or "").strip().replace("|", "\\|").replace("\n", "<br>")
        body.append(
            f"| `{tag_name}` | {carries_md} | {desc_md or '—'} |"
        )

    return "\n".join(body) + "\n"


# ----------------------------------------------------------------------------
# Page layout: decide which types get their own page
# ----------------------------------------------------------------------------


@dataclass
class PageSpec:
    title: str           # H1 + nav label
    nav_label: str       # short label for the sidebar
    rel_path: str        # relative to docs/, e.g. "configuration/inbound/socks.md"
    item_id: int
    intro: str = ""


def plan_pages(index: dict[str, Item]) -> list[PageSpec]:
    """Decide which config items get their own page.

    Ordering matters: the page_for_type map is built from this list, and
    later pages refer to earlier ones via that map (which is built before
    rendering).
    """
    by_name: dict[str, Item] = {
        it.name: it for it in index.values() if it.is_config and it.name
    }

    def need(name: str) -> Item:
        if name not in by_name:
            raise SystemExit(f"missing expected config type: {name}")
        return by_name[name]

    pages: list[PageSpec] = []

    # Top-level config
    pages.append(PageSpec(
        title="Config",
        nav_label="Overview",
        rel_path="configuration/index.md",
        item_id=int(need("Config").id),
        intro="Top-level configuration object. Every shadowquic config file deserializes into this struct.",
    ))

    # Inbound dispatcher + variants
    pages.append(PageSpec(
        title="Inbound",
        nav_label="Overview",
        rel_path="configuration/inbound/index.md",
        item_id=int(need("InboundCfg").id),
        intro="Selects an inbound listener. Pick a variant via the `type` key.",
    ))
    inbound_variants = [
        ("SocksServerCfg", "SOCKS5 server", "configuration/inbound/socks.md"),
        ("ShadowQuicServerCfg", "ShadowQuic server", "configuration/inbound/shadowquic.md"),
        ("SunnyQuicServerCfg", "SunnyQuic server", "configuration/inbound/sunnyquic.md"),
    ]
    for tname, label, path in inbound_variants:
        if tname in by_name:
            pages.append(PageSpec(
                title=label,
                nav_label=label,
                rel_path=path,
                item_id=int(by_name[tname].id),
            ))

    # Outbound dispatcher + variants
    pages.append(PageSpec(
        title="Outbound",
        nav_label="Overview",
        rel_path="configuration/outbound/index.md",
        item_id=int(need("OutboundCfg").id),
        intro="Selects the upstream the inbound forwards to. Pick a variant via the `type` key.",
    ))
    outbound_variants = [
        ("SocksClientCfg", "SOCKS5 outbound", "configuration/outbound/socks.md"),
        ("ShadowQuicClientCfg", "ShadowQuic outbound", "configuration/outbound/shadowquic.md"),
        ("SunnyQuicClientCfg", "SunnyQuic outbound", "configuration/outbound/sunnyquic.md"),
        ("DirectOutCfg", "Direct outbound", "configuration/outbound/direct.md"),
    ]
    for tname, label, path in outbound_variants:
        if tname in by_name:
            pages.append(PageSpec(
                title=label,
                nav_label=label,
                rel_path=path,
                item_id=int(by_name[tname].id),
            ))

    # Shared / supporting types
    shared = [
        ("AuthUser", "User authentication"),
        ("JlsUpstream", "JLS upstream"),
        ("CongestionControl", "Congestion control"),
        ("BrutalParams", "Brutal parameters"),
        ("CipherSuitePreference", "Cipher suite preference"),
        ("DnsStrategy", "DNS strategy"),
        ("LogLevel", "Log level"),
    ]
    for tname, label in shared:
        if tname in by_name:
            pages.append(PageSpec(
                title=label,
                nav_label=label,
                rel_path=f"configuration/shared/{tname.lower()}.md",
                item_id=int(by_name[tname].id),
            ))

    return pages


# ----------------------------------------------------------------------------
# nav.yml rendering
# ----------------------------------------------------------------------------


NAV_BEGIN = "# >>> generated nav"
NAV_END = "# <<< generated nav"


def render_nav(pages: list[PageSpec]) -> str:
    """Render a `nav = [...]` TOML block matching the page layout."""
    # Group pages by their second path component (configuration/<group>/...).
    cfg_pages = [p for p in pages if p.rel_path.startswith("configuration/")]

    # Overview page (configuration/index.md) is the section index.
    overview = next(p for p in cfg_pages if p.rel_path == "configuration/index.md")

    inbound_pages = [p for p in cfg_pages if p.rel_path.startswith("configuration/inbound/")]
    outbound_pages = [p for p in cfg_pages if p.rel_path.startswith("configuration/outbound/")]
    shared_pages = [p for p in cfg_pages if p.rel_path.startswith("configuration/shared/")]

    lines = ["nav = ["]
    lines.append('  { "Home" = "index.md" },')
    lines.append('  { "Configuration" = [')
    lines.append(f'    {{ "Overview" = "{overview.rel_path}" }},')

    if inbound_pages:
        lines.append('    { "Inbound" = [')
        for i, p in enumerate(inbound_pages):
            label = "Overview" if p.rel_path.endswith("/index.md") else p.nav_label
            comma = "," if i < len(inbound_pages) - 1 else ""
            lines.append(f'      {{ "{label}" = "{p.rel_path}" }}{comma}')
        lines.append('    ] },')

    if outbound_pages:
        lines.append('    { "Outbound" = [')
        for i, p in enumerate(outbound_pages):
            label = "Overview" if p.rel_path.endswith("/index.md") else p.nav_label
            comma = "," if i < len(outbound_pages) - 1 else ""
            lines.append(f'      {{ "{label}" = "{p.rel_path}" }}{comma}')
        lines.append('    ] },')

    if shared_pages:
        lines.append('    { "Shared types" = [')
        for i, p in enumerate(shared_pages):
            comma = "," if i < len(shared_pages) - 1 else ""
            lines.append(f'      {{ "{p.nav_label}" = "{p.rel_path}" }}{comma}')
        lines.append('    ] }')

    lines.append('  ] }')
    lines.append("]")
    return "\n".join(lines) + "\n"


def patch_zensical_nav(zensical_toml: Path, new_nav_block: str) -> None:
    text = zensical_toml.read_text()
    if NAV_BEGIN not in text or NAV_END not in text:
        raise SystemExit(
            f"{zensical_toml} must contain `{NAV_BEGIN}` and `{NAV_END}` markers."
        )
    head, _, rest = text.partition(NAV_BEGIN)
    _, _, tail = rest.partition(NAV_END)
    new_text = head + NAV_BEGIN + "\n" + new_nav_block + NAV_END + tail
    zensical_toml.write_text(new_text)


# ----------------------------------------------------------------------------
# landing page
# ----------------------------------------------------------------------------


LANDING_PAGE = """# shadowquic

A 0-RTT QUIC proxy with SNI camouflage.

This site documents the **YAML configuration schema**. The pages under
[Configuration](configuration/index.md) are generated from the doc comments on
the Rust structs in [`shadowquic/src/config/`](
https://github.com/spongebob888/shadowquic/tree/main/shadowquic/src/config),
so they stay in sync with the actual deserializer.

## Quick links

- [Top-level `Config`](configuration/index.md)
- [Inbound types](configuration/inbound/index.md)
- [Outbound types](configuration/outbound/index.md)

## Example

```yaml
inbound:
  type: socks
  bind-addr: "127.0.0.1:1089"
outbound:
  type: shadowquic
  addr: "your.server.example:443"
  username: "alice"
  password: "secret"
  server-name: "your.server.example"
log-level: info
```

Save as `config.yaml` and run:

```sh
shadowquic -c config.yaml
```
"""


# ----------------------------------------------------------------------------
# main
# ----------------------------------------------------------------------------


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--no-build",
        action="store_true",
        help="reuse the existing target/doc/shadowquic.json instead of running cargo",
    )
    parser.add_argument(
        "--json",
        type=Path,
        default=None,
        help="path to a prebuilt rustdoc JSON file (implies --no-build)",
    )
    parser.add_argument(
        "--clean",
        action="store_true",
        help="wipe docs/configuration before regenerating",
    )
    args = parser.parse_args(argv)

    if args.json:
        json_path = args.json
    elif args.no_build:
        json_path = REPO_ROOT / "target" / "doc" / "shadowquic.json"
        if not json_path.exists():
            raise SystemExit(
                f"{json_path} not found; drop --no-build or pass --json"
            )
    else:
        json_path = build_rustdoc_json(REPO_ROOT)

    index = load_index(json_path)
    defaults = collect_default_fn_values(REPO_ROOT)
    src_attrs = parse_source_attrs(REPO_ROOT)
    pages = plan_pages(index)

    page_for_type = {p.item_id: p.rel_path for p in pages}
    variant_tags, enum_defaults_by_id = collect_enum_variant_tags(index, src_attrs)
    ctx = RenderContext(
        index=index,
        page_for_type=page_for_type,
        default_values=defaults,
        pages_dir_for={p.item_id: (DOCS_ROOT / p.rel_path).parent for p in pages},
        src_attrs=src_attrs,
        enum_variant_tags=variant_tags,
        enum_defaults_by_id=enum_defaults_by_id,
    )

    # Wipe & recreate
    cfg_dir = DOCS_ROOT / "configuration"
    if args.clean and cfg_dir.exists():
        shutil.rmtree(cfg_dir)
    DOCS_ROOT.mkdir(parents=True, exist_ok=True)

    # Landing page (only written if missing — user may want to customize).
    landing = DOCS_ROOT / "index.md"
    if not landing.exists():
        landing.write_text(LANDING_PAGE)

    # Per-type pages
    for p in pages:
        item = Item(index[str(p.item_id)].raw)
        out_path = DOCS_ROOT / p.rel_path
        out_path.parent.mkdir(parents=True, exist_ok=True)

        if "struct" in item.inner:
            body = emit_struct_page(item, Path(p.rel_path), p.title, ctx, p.intro)
        elif "enum" in item.inner:
            body = emit_enum_page(item, Path(p.rel_path), p.title, ctx, p.intro)
        else:
            print(f"skip {item.name}: unsupported kind {item.kind}", file=sys.stderr)
            continue

        out_path.write_text(body)
        print(f"wrote {out_path.relative_to(REPO_ROOT)}", file=sys.stderr)

    # Update nav
    patch_zensical_nav(SITE_ROOT / "zensical.toml", render_nav(pages))
    print(
        f"updated nav in {(SITE_ROOT / 'zensical.toml').relative_to(REPO_ROOT)}",
        file=sys.stderr,
    )

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
