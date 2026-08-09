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
CONFIG_SRC_PREFIX = "shadowquic/src/config/"

# ----------------------------------------------------------------------------
# i18n: two complete sites, one per language
# ----------------------------------------------------------------------------
#
# Zensical builds a single-language site per config file. For bilingual
# support we generate two full doc trees (docs/en + docs/zh) and render each
# into its own site via zensical.toml / zensical.zh.toml. Both configs carry
# `extra.alternate` so the header shows a language switcher.
#
# The Chinese tree is produced by translating the English rustdoc doc
# comments plus a set of UI labels (headings, "required"/"default" markers,
# nav section names). Doc comments are keyed by exact string so untranslated
# entries fall back to English rather than breaking the build.

LANG_EN = "en"
LANG_ZH = "zh"

#: Which language this invocation renders. Set by `--lang` in main().
LANG = LANG_EN


def docs_root() -> Path:
    """Per-language docs directory (docs/en or docs/zh)."""
    return SITE_ROOT / "docs" / LANG


def config_file() -> Path:
    """Per-language Zensical config whose nav block we patch."""
    return SITE_ROOT / "zensical.toml" if LANG == LANG_EN else SITE_ROOT / "zensical.zh.toml"


# English rustdoc doc comment -> Chinese translation. Exact-match only.
# Anything not listed here stays in English.
ZH_DOCS: dict[str, str] = {
    "0-RTT handshake.\nSet to true to enable zero rtt.\nEnabled by default":
        "0-RTT 握手。\n设为 true 开启零往返（0-RTT）。\n默认开启",
    "Alpn of tls, default is \\[\"h3\"\\], must have common element with server":
        "TLS 的 ALPN，默认是 [\"h3\"]，必须与服务器有公共元素",
    "Alpn of tls. Default is `[\"h3\"]`, must have common element with client":
        "TLS 的 ALPN，默认是 [\"h3\"]，必须与客户端有公共元素",
    "Android Only. the unix socket path for protecting android socket":
        "仅 Android。用于保护 Android socket 的 Unix socket 路径",
    "Binding address. e.g. `0.0.0.0:443`, `[::1]:443`":
        "绑定地址。例如 `0.0.0.0:443`、`[::1]:443`",
    "Brutal server configuration":
        "Brutal 服务器配置",
    "Certificate path for tls":
        "TLS 证书路径",
    "Congestion control, default to \"bbr\", supported: \"bbr\", \"new-reno\", \"cubic\"":
        "拥塞控制算法，默认是 \"bbr\"，支持：\"bbr\"、\"new-reno\"、\"cubic\"",
    "Enable MTU black-hole detection. When enabled, the current MTU is reset to `min_mtu` once\na black hole is detected (standard PLPMTUD behavior). When disabled (default), the\npreviously discovered MTU is kept after a black hole is detected.\nControls quinn-jls `MtuDiscoveryConfig::blackhole_reset_mtu`.\nOnly takes effect when `mtu_discovery` is enabled.\n\nIn high packet loss network, it's better to disable black hole detection to avoid unnecessary mtu reset.":
        "启用 MTU 黑洞检测。启用时，一旦检测到黑洞，当前 MTU 会重置为 `min_mtu`（标准 PLPMTUD 行为）。停用时（默认），检测到黑洞后仍保留先前发现的 MTU。\n控制 quinn-jls 的 `MtuDiscoveryConfig::blackhole_reset_mtu`。\n仅在 `mtu_discovery` 启用时生效。\n\n在高丢包网络中，建议关闭黑洞检测以避免不必要的 MTU 重置。",
    "Enable QUIC Generic Segmentation Offload (GSO).\nControls [`quinn::TransportConfig::enable_segmentation_offload`]. When supported, GSO reduces\nCPU usage for bulk sends; unsupported environments may see transient startup packet loss.\nEnabled by default":
        "启用 QUIC 通用分段卸载（GSO）。\n控制 [`quinn::TransportConfig::enable_segmentation_offload`]。在支持时，GSO 能降低批量发送的 CPU 占用；不支持的環境可能出现启动阶段的瞬时丢包。\n默认开启",
    "Enable auto MTU discovery, default to true\nFor stable udp network, it's better to disable it and set a proper initial mtu":
        "启用自动 MTU 探测，默认开启。\n对于稳定的 UDP 网络，建议关闭它并设置合适的 initial-mtu",
    "Initial mtu, must be larger than min mtu, at least to be 1200.\n1400 is recommended for high packet loss network. default to be 1300":
        "初始 MTU，必须大于 min-mtu，至少为 1200。\n高丢包网络推荐 1400，默认 1300",
    "Jls upstream address, e.g. `codepn.io:443`, `google.com:443`, `127.0.0.1:443`":
        "JLS 上游地址，例如 `codepn.io:443`、`google.com:443`、`127.0.0.1:443`",
    "Jls upstream configuration":
        "JLS 上游配置",
    "Jls upstream, camouflage server, must be address with port. e.g.: `codepn.io:443`,`google.com:443`,`127.0.0.1:443`":
        "JLS 上游（伪装服务器），必须是带端口的地址。例如：`codepn.io:443`、`google.com:443`、`127.0.0.1:443`",
    "Keep alive interval in milliseconds\n0 means disable keep alive, should be smaller than 30_000(idle time).\nDisabled by default.":
        "保活间隔（毫秒）。\n0 表示关闭保活，应小于 30_000（空闲超时）。\n默认关闭。",
    "Log level of shadowquic\nDefault level is info.":
        "shadowquic 的日志级别。\n默认是 info。",
    "Maximum number of paths for multipath quic, 0 for disabling multipath":
        "多路径 QUIC 的最大路径数，0 表示关闭多路径",
    "Maximum rate for JLS forwarding in unit of bps, default is disabled.":
        "JLS 转发速率上限（单位 bps），默认不限制。",
    "Minimum mtu, must be smaller than initial mtu, at least to be 1200.\n1400 is recommended for high packet loss network. default to be 1290":
        "最小 MTU，必须小于 initial-mtu，至少为 1200。\n高丢包网络推荐 1400，默认 1290",
    "Optional TLS 1.3 cipher suite preference.\nIf unset, use rustls/ring default preference order.":
        "可选的 TLS 1.3 密码套件偏好。\n不设置时使用 rustls/ring 的默认优先级顺序。",
    "Private key path for tls":
        "TLS 私钥路径",
    "SOCKS5 password, optional":
        "SOCKS5 密码，可选",
    "SOCKS5 username, optional":
        "SOCKS5 用户名，可选",
    "Server binding address. e.g. `0.0.0.0:1080`, `[::]:1080`":
        "服务器绑定地址。例如 `0.0.0.0:1080`、`[::]:1080`",
    "Server binding address. e.g. `0.0.0.0:1089`, `[::1]:1089`":
        "服务器绑定地址。例如 `0.0.0.0:1089`、`[::1]:1089`",
    "Server name of the certificates":
        "证书的服务器名",
    "Server name used to check client. Must be the same as client\nIf empty, server name will be parsed from jls_upstream\nIf not available, server name check will be skipped":
        "用于校验客户端的服务器名，必须与客户端一致。\n为空时从 jls_upstream 解析服务器名。\n无法获得时跳过服务器名校验。",
    "Server name, must be the same as the server jls_upstream\ndomain name":
        "服务器名，必须与服务端 jls_upstream 的域名一致",
    "Set to true to enable zero rtt, default to true":
        "设为 true 开启零往返（0-RTT），默认 true",
    "Socket options":
        "Socket 选项",
    "Socket options like bind interface and fwmark":
        "Socket 选项，例如绑定接口和 fwmark",
    "Socks5 username, optional\nLeft empty to disable authentication":
        "SOCKS5 用户名，可选。留空表示关闭认证",
    "Transfer udp over stream or over datagram.\nIf true, use quic stream to send UDP, otherwise use quic datagram\nextension, similar to native UDP in TUIC":
        "UDP 走 QUIC 流（stream）还是数据报（datagram）。\n为 true 时用 QUIC 流发送 UDP，否则用 QUIC 数据报扩展，类似 TUIC 的原生 UDP",
    "Transfer udp over stream or over datagram.\nIf true, use quic stream to send UDP, otherwise use quic datagram\nextension, similar to native UDP in TUIC\n\n### Proxy HTTP3\nTo proxy HTTP3 traffic, recommend to disable over-stream and blackhole-detection.\n\nOver-stream will retransmit lost packets conflicting shadowquic's inner congestion controller. This is famous [*TCP in TCP*(TCP meltdown)\n](https://web.archive.org/web/20230228035749/http://sites.inka.de/%7EW1011/devel/tcp-tcp.html) problem.\n\nOver-stream also breaks HTTP3's mtu discovery leading to probe wrong MTU.":
        "UDP 走 QUIC 流（stream）还是数据报（datagram）。\n为 true 时用 QUIC 流发送 UDP，否则用 QUIC 数据报扩展，类似 TUIC 的原生 UDP\n\n### 代理 HTTP3\n代理 HTTP3 流量时，建议关闭 over-stream 和 blackhole-detection。\n\nOver-stream 会重传丢失的数据包，这与 shadowquic 内部的拥塞控制冲突，是著名的 [*TCP in TCP*（TCP 崩溃）](https://web.archive.org/web/20230228035749/http://sites.inka.de/%7EW1011/devel/tcp-tcp.html) 问题。\n\nOver-stream 还会破坏 HTTP3 的 MTU 探测，导致探测到错误的 MTU。",
    "Users for client authentication":
        "用于客户端认证的用户",
    "binding interface of this outgoing packet.\n\nIf `bind_interface` is set, the outgoing packet will be sent from the\nspecified interface. Recommend to use to cooporate with other tun based proxy like sing-box/mihomo\n\nExample:\n```yaml\n# by ip address\nbind-interface: \"127.0.0.1\"\n# by interface name\nbind-interface: \"eth0\"\n```":
        "此出站数据包的绑定接口。\n\n设置了 `bind_interface` 后，出站数据包将从指定接口发出。建议配合 sing-box/mihomo 等基于 TUN 的代理一起使用。\n\n示例：\n```yaml\n# 按 IP 地址\nbind-interface: \"127.0.0.1\"\n# 按接口名\nbind-interface: \"eth0\"\n```",
    "fw_mark on linux":
        "Linux 上的 fw_mark",
    "only use ipv4 address":
        "只使用 IPv4 地址",
    "only use ipv6 address":
        "只使用 IPv6 地址",
    "password, must be the same as the server":
        "密码，必须与服务器一致",
    "Shadowquic server address. example: `127.0.0.0.1:443`, `www.server.com:443`, `[ff::f1]:4443`":
        "Shadowquic 服务器地址。例如：`127.0.0.0.1:443`、`www.server.com:443`、`[ff::f1]:4443`",
    "Additional paths for multipath quic\nIPV4 or IPv6 path are all fine.\nRight now only one path is used to sending data, the rest paths are backup paths.\nSee https://github.com/n0-computer/quinn/issues/389 for more details.\n\nIt's recommended to use IPV4 and IPV6 path together for dual stack network.\n```yaml\nextra-paths:\n  - \"[12:ff::ff]:1089\"\n```":
        "多路径 QUIC 的附加路径。\nIPv4 或 IPv6 路径都可以。\n目前只有一条路径用于发送数据，其余路径是备用路径。\n详见 https://github.com/n0-computer/quinn/issues/389。\n\n建议同时使用 IPv4 和 IPv6 路径以支持双栈网络。\n```yaml\nextra-paths:\n  - \"[12:ff::ff]:1089\"\n```",
    "Path of a YAML file persisting users and their traffic statistics.\nLoaded and merged with `users` at startup; written on every user\nchange via API, periodically and on graceful shutdown.\nOptional, persistence is disabled when omitted.\n```yaml\nuser-store: \"users.yaml\"\n```":
        "用于持久化用户及其流量统计的 YAML 文件路径。\n启动时与 `users` 合并加载；在通过 API 修改用户、周期性刷新以及优雅关闭时写入。\n可选，省略时关闭持久化。\n```yaml\nuser-store: \"users.yaml\"\n```",
    "Interval in seconds for periodically flushing users and traffic\nstatistics to the `user-store` file. Default is 60, set to 0 to\ndisable periodic flushing (only API changes and shutdown persist).\nOnly takes effect when `user-store` is set.":
        "把用户和流量统计周期性刷入 `user-store` 文件的间隔（秒）。\n默认 60，设为 0 关闭周期性刷新（仅 API 变更和关闭时持久化）。\n仅在设置了 `user-store` 时生效。",
    "Jls upstream configuration":
        "JLS 上游配置",
    "try to use ipv4 first, if no ipv4 address, use ipv6":
        "优先使用 IPv4，若无 IPv4 地址则使用 IPv6",
    "try to use ipv6 first, if no ipv6 address, use ipv4":
        "优先使用 IPv6，若无 IPv6 地址则使用 IPv4",
    "user authentication":
        "用户认证",
    "username, must be the same as the server":
        "用户名，必须与服务器一致",
}

# Longer top-level container docs (struct/enum pages). Kept separately so the
# per-field table above stays readable.
ZH_CONTAINER_DOCS: dict[str, str] = {
    "Config": "shadowquic 的整体配置。\n\n示例：\n```yaml\ninbound:\n  type: xxx\n  xxx: xxx\noutbound:\n  type: xxx\n  xxx: xxx\nlog-level: trace # 或 debug、info、warn、error\n```\n支持的入站类型见 [`InboundCfg`]\n\n支持的出站类型见 [`OutboundCfg`]",
    "InboundCfg": "入站配置\n示例：\n```yaml\ntype: socks # 或 shadowquic\nbind-addr: \"0.0.0.0:443\" # \"[::]:443\"\nxxx: xxx # 其他字段取决于类型\n```\n各类型对应的配置字段见 [`SocksServerCfg`] 和 [`ShadowQuicServerCfg`]",
    "OutboundCfg": "出站配置\n示例：\n```yaml\ntype: socks # 或 shadowquic、direct\naddr: \"127.0.0.1:443\" # \"[::1]:443\"\nxxx: xxx # 其他字段取决于类型\n```\n各类型对应的配置字段见 [`SocksClientCfg`] 和 [`ShadowQuicClientCfg`]",
    "ShadowQuicServerCfg": "shadowquic 入站配置\n\n示例：\n```yaml\nbind-addr: \"0.0.0.0:1443\"\nusers:\n  - username: \"zhangsan\"\n    password: \"12345678\"\njls-upstream:\n  addr: \"echo.free.beeceptor.com:443\" # 域名/IP + 端口，域名必须与客户端一致\n  rate-limit: 1000000 # 可选：限制转发速率（bps），默认不限制\nserver-name: \"echo.free.beeceptor.com\" # 必须与客户端一致\nalpn: [\"h3\"]\ncongestion-control: bbr\nzero-rtt: true\n```",
    "SunnyQuicServerCfg": "sunnyquic 入站配置\n\n示例：\n```yaml\nbind-addr: \"0.0.0.0:1443\"\nusers:\n  - username: \"zhangsan\"\n    password: \"12345678\"\nserver-name: \"echo.free.beeceptor.com\" # 必须与客户端一致\nalpn: [\"h3\"]\ncongestion-control: bbr\nzero-rtt: true\n```",
    "ShadowQuicClientCfg": "shadowquic 出站配置\n\n示例：\n```yaml\naddr: \"12.34.56.7:1089\" # 或 \"[12:ff::ff]:1089\"（双栈）\npassword: \"12345678\"\nusername: \"87654321\"\nserver-name: \"echo.free.beeceptor.com\" # 必须与服务端 jls_upstream 一致\nalpn: [\"h3\"]\ninitial-mtu: 1400\ncongestion-control: bbr\nzero-rtt: true\nover-stream: false  # true 表示 UDP 走流，false 表示 UDP 走数据报\n```",
    "SunnyQuicClientCfg": "sunnyquic 出站配置\n\n示例：\n```yaml\naddr: \"12.34.56.7:1089\" # 或 \"[12:ff::ff]:1089\"（双栈）\npassword: \"12345678\"\nusername: \"87654321\"\nserver-name: \"echo.free.beeceptor.com\"\nalpn: [\"h3\"]\ninitial-mtu: 1400\ncongestion-control: bbr\nzero-rtt: true\nover-stream: false  # true 表示 UDP 走流，false 表示 UDP 走数据报\n```",
    "SocksServerCfg": "socks 入站配置\n\n示例：\n```yaml\nbind-addr: \"0.0.0.0:1089\" # 或 \"[::]:1089\"（双栈）\nusers:\n - username: \"username\"\n   password: \"password\"\n```",
    "SocksClientCfg": "socks 出站配置\n示例：\n```yaml\naddr: \"12.34.56.7:1089\" # 或 \"[12:ff::ff]:1089\"（双栈）\n```",
    "MixedServerCfg": "mixed 入站配置\n\n同一端口同时支持 SOCKS5 和 HTTP 代理（CONNECT + 普通 HTTP 转发）。\n\n示例：\n```yaml\ntype: mixed\nbind-addr: \"0.0.0.0:1080\"\n```",
    "TproxyServerCfg": "tproxy 入站配置\n\n示例：\n```yaml\nbind-addr: \"0.0.0.0:1089\" # 或 \"[::]:1089\"（双栈）\n```",
    "DirectOutCfg": "直连出站配置\n示例：\n```yaml\ndns-strategy: prefer-ipv4 # 或 prefer-ipv6、ipv4-only、ipv6-only\n```",
    "CongestionControl": "拥塞控制算法\n示例：\n```yaml\ncongestion-control: bbr # 或 cubic、new-reno、brutal\n```\n使用 `brutal` 时，配置形如：\n```yaml\ncongestion-control:\n  brutal:\n    bandwidth: 10000000 # 默认 10000000 bps\n```\nBrutal 的 bandwidth 指上行带宽。若要设置下行带宽，请设置对端（例如客户端对应服务器端）的 bandwidth。",
    "DnsStrategy": "DNS 解析策略\n默认是 `prefer-ipv4`\n\n- `prefer-ipv4`：优先 IPv4，无 IPv4 地址时用 IPv6\n- `prefer-ipv6`：优先 IPv6，无 IPv6 地址时用 IPv4\n- `ipv4-only`：只用 IPv4 地址\n- `ipv6-only`：只用 IPv6 地址",
    "LogLevel": "shadowquic 的日志级别\n默认是 info。",
    "AuthUser": "用户认证",
    "SocketOpt": "Socket 选项",
    "BrutalParams": "Brutal 参数",
    "CipherSuitePreference": "TLS 1.3 密码套件偏好",
    "Interface": "出站数据包的绑定接口",
}

# Chinese labels for shared config types (friendly page titles).
ZH_SHARED_LABELS = {
    "LogLevel": "日志级别",
    "AuthUser": "认证用户",
    "JlsUpstream": "JLS 上游",
    "CongestionControl": "拥塞控制",
    "BrutalParams": "Brutal 参数",
    "SocketOpt": "Socket 选项",
    "CipherSuitePreference": "密码套件偏好",
    "DnsStrategy": "DNS 策略",
    "Interface": "接口",
}
# UI labels used by the emitters, per language.
ZH_UI = {
    "title_fields": "字段",
    "title_variants": "变体",
    "meta_type": "类型",
    "meta_required": "必填",
    "meta_default": "默认值",
    "meta_required_yes": "是",
    "meta_required_no": "否",
    "meta_optional": "可选",
    "meta_type_default": "（类型默认值）",
    "meta_list_of": "列表",
    "meta_serialized_tag": "此枚举通过 YAML 键 `{tag}` 选择变体。",
    "nav_home": "首页",
    "nav_guide": "入门指南",
    "nav_guide_label": "入门指南",
    "nav_config": "配置",
    "nav_config_overview": "概览",
    "nav_inbound": "入站",
    "nav_outbound": "出站",
    "nav_shared": "共享类型",
    "nav_api": "API",
    "nav_protocol": "协议",
    "variant_carries": "携带",
    "variant_desc": "说明",
}

# English equivalents, used when rendering the English tree.
EN_UI = {
    "title_fields": "Fields",
    "title_variants": "Variants",
    "meta_type": "Type",
    "meta_required": "Required",
    "meta_default": "Default",
    "meta_required_yes": "yes",
    "meta_required_no": "no",
    "meta_optional": "optional",
    "meta_type_default": "type default",
    "meta_list_of": "list of",
    "meta_serialized_tag": "This enum is serialized with the YAML key **`{tag}`** selecting the variant.",
    "nav_home": "Home",
    "nav_guide": "Guide",
    "nav_guide_label": "Guide",
    "nav_config": "Configuration",
    "nav_config_overview": "Overview",
    "nav_inbound": "Inbound",
    "nav_outbound": "Outbound",
    "nav_shared": "Shared types",
    "nav_api": "API",
    "nav_protocol": "Protocol",
    "variant_carries": "Carries",
    "variant_desc": "Description",
}


def _(text: str) -> str:
    """Translate a doc comment for the active language (zh only)."""
    if LANG != LANG_ZH:
        return text
    return ZH_DOCS.get(text, text)


def ui(key: str, **fmt: object) -> str:
    """Return a UI label for the active language."""
    table = ZH_UI if LANG == LANG_ZH else EN_UI
    label = table.get(key, key)
    return label.format(**fmt) if fmt else label

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
# PROTOCOL.typ -> SVG -> protocol page
# ----------------------------------------------------------------------------


PROTOCOL_NAV_LABEL = "Protocol"
PROTOCOL_REL_DIR = "protocol"  # docs/protocol/
PROTOCOL_SOURCE_NAME = "PROTOCOL.typ"
PROTOCOL_PDF_NAME = "PROTOCOL.pdf"
API_NAV_LABEL = "API"
API_SOURCE_NAME = "document/api.md"
API_REL_PATH = "api.md"


def build_protocol_pages(repo_root: Path) -> list[Path]:
    """Render `PROTOCOL.typ` to one SVG per page under `docs/protocol/`.

    Returns the paths (relative to docs/) of the generated SVGs in page order.
    The current `typst compile` build available on most distros doesn't yet
    enable the experimental HTML target, so we use SVG and embed each page in
    a generated markdown file.
    """
    src = repo_root / PROTOCOL_SOURCE_NAME
    if not src.exists():
        raise SystemExit(f"missing {src}")

    out_dir = docs_root() / PROTOCOL_REL_DIR
    out_dir.mkdir(parents=True, exist_ok=True)
    # Wipe any prior pages so deletions in PROTOCOL.typ don't leave stragglers.
    for stale in out_dir.glob("page-*.svg"):
        stale.unlink()

    pattern = out_dir / "page-{p}.svg"
    cmd = ["typst", "compile", str(src), str(pattern)]
    print(f"$ {' '.join(cmd)}", file=sys.stderr)
    res = subprocess.run(cmd)
    if res.returncode != 0:
        raise SystemExit(f"typst compile failed with exit {res.returncode}")

    pages = sorted(out_dir.glob("page-*.svg"), key=lambda p: int(re.search(r"(\d+)", p.stem).group(1)))
    return pages


def write_protocol_page(svg_pages: list[Path], pdf_link: str | None) -> Path:
    """Write `docs/protocol/index.md` embedding each SVG page in order."""
    out_path = docs_root() / PROTOCOL_REL_DIR / "index.md"
    body: list[str] = ["# shadowquic protocol\n"]
    body.append(
        "Specification of the wire protocol between shadowquic clients and servers. "
        "Rendered from "
        "[`PROTOCOL.typ`](https://github.com/spongebob888/shadowquic/blob/main/PROTOCOL.typ).\n"
    )
    if pdf_link:
        body.append(f"[Download PDF]({pdf_link})\n")
    for i, svg in enumerate(svg_pages, start=1):
        rel = svg.name  # SVGs sit alongside this index.md
        body.append(
            f'![Page {i}]({rel}){{ .protocol-page loading=lazy }}'
        )
        body.append("")
    out_path.write_text("\n".join(body) + "\n")
    return out_path


def write_api_page(repo_root: Path) -> Path:
    """Copy the user-management API reference into the site docs.

    English comes from `document/api.md`; the Chinese tree uses
    `document/api.zh.md` (hand-translated copy kept in the repo).
    """
    src = repo_root / (API_SOURCE_NAME if LANG == LANG_EN else "document/api.zh.md")
    if not src.exists():
        raise SystemExit(f"missing {src}")

    out_path = docs_root() / API_REL_PATH
    out_path.write_text(src.read_text())
    return out_path


def write_guide_pages(repo_root: Path) -> Path:
    """Copy the static beginner's guide (`guide/<lang>/**`) into docs/<lang>/.

    The guide is hand-written markdown, distinct from the auto-generated
    configuration reference. Each language has its own complete guide tree
    under `guide/{en,zh}/`, kept in the repo (outside the gitignored `docs/`)
    so edits are committed; this function stages the active language's guide
    into the site on every build.
    """
    src_dir = SITE_ROOT / "guide" / LANG
    if not src_dir.exists():
        raise SystemExit(f"missing {src_dir}")
    out_dir = docs_root() / "guide"
    if out_dir.exists():
        shutil.rmtree(out_dir)
    shutil.copytree(src_dir, out_dir)
    return out_dir


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
            return f"{render_type(inner_args[0], ctx, from_page)} *({ui('meta_optional')})*"
        if short == "Vec" and len(inner_args) == 1:
            return f"{ui('meta_list_of')} {render_type(inner_args[0], ctx, from_page)}"

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
        body.append(ui(extra_intro).rstrip() + "\n")

    docs = item.docs
    if LANG == LANG_ZH:
        docs = ZH_CONTAINER_DOCS.get(item.name) or _(item.docs)
    docs_md = rewrite_doc_links(docs, item.links, ctx, page_path)
    if docs_md:
        body.append(docs_md.rstrip() + "\n")

    body.append(f"## {ui('title_fields')}\n")
    _emit_struct_fields(item, serde, ctx, body, page_path, depth=0)
    return "\n".join(body) + "\n"


def _resolve_struct_target(ty: dict | None, ctx: RenderContext) -> Item | None:
    """If `ty` is a resolved_path to a config struct, return that struct."""
    if not isinstance(ty, dict) or "resolved_path" not in ty:
        return None
    tid = ty["resolved_path"].get("id")
    if tid is None:
        return None
    target = ctx.index.get(str(tid))
    if target is None or not target.is_config or "struct" not in target.inner:
        return None
    return target


def _emit_struct_fields(
    item: Item,
    container_serde: SerdeContainer,
    ctx: RenderContext,
    body: list[str],
    page_path: Path,
    depth: int,
    inlined_from: list[str] | None = None,
) -> None:
    """Emit each field of a struct as a heading section.

    Fields tagged `#[serde(flatten)]` whose type is another known config
    struct are expanded inline using *that* struct's serde container rules,
    so the rendered docs match the on-the-wire YAML shape.
    """
    if depth > 8:  # paranoia: don't loop forever on cyclic schemas
        return
    inlined_from = inlined_from or []

    fields = item.inner["struct"]["kind"]["plain"]["fields"]
    for fid in fields:
        f = ctx.index.get(str(fid))
        if f is None:
            continue
        fserde = parse_field_serde(attrs_for_member(f, item.name, ctx.src_attrs))
        if fserde.skip:
            continue

        ty = f.inner.get("struct_field")

        # `#[serde(flatten)]`: don't render this field. Recurse into the
        # carried struct so its fields appear as if declared here.
        if fserde.flatten:
            target = _resolve_struct_target(ty, ctx)
            if target is not None and target.name and target.name not in inlined_from:
                inner_serde = parse_container_serde(
                    attrs_for_container(target, ctx.src_attrs)
                )
                _emit_struct_fields(
                    target,
                    inner_serde,
                    ctx,
                    body,
                    page_path,
                    depth=depth + 1,
                    inlined_from=inlined_from + [target.name],
                )
                continue
            # Fallthrough: render as a regular field if we can't resolve.

        yaml_name = fserde.rename or apply_rename_all(
            f.name or "", container_serde.rename_all
        )
        type_md = render_type(ty, ctx, page_path) if isinstance(ty, dict) else "`?`"

        is_option_type = False
        if isinstance(ty, dict) and "resolved_path" in ty:
            is_option_type = (
                (ty["resolved_path"].get("path") or "").split("::")[-1] == "Option"
            )

        required = not (fserde.has_default or is_option_type)
        required_md = ui("meta_required_yes") if required else ui("meta_required_no")
        required_md = f"**{required_md}**" if required else required_md

        body.append(f"### `{yaml_name}`\n")
        meta: list[str] = [
            f"- **{ui('meta_type')}:** {type_md}",
            f"- **{ui('meta_required')}:** {required_md}",
        ]
        if fserde.default_fn:
            val = ctx.default_values.get(fserde.default_fn)
            display = render_default_value(val, ctx) if val else f"{fserde.default_fn}()"
            meta.append(f"- **{ui('meta_default')}:** `{display}`")
        elif fserde.has_default and not is_option_type:
            enum_default = None
            if isinstance(ty, dict) and "resolved_path" in ty:
                tid = ty["resolved_path"].get("id")
                if tid is not None:
                    enum_default = ctx.enum_defaults_by_id.get(int(tid))
            if enum_default is not None:
                meta.append(f"- **{ui('meta_default')}:** `{enum_default}`")
            else:
                meta.append(f"- **{ui('meta_default')}:** *{ui('meta_type_default')}*")
        body.append("\n".join(meta) + "\n")

        desc_md = rewrite_doc_links(_(f.docs or ""), f.links, ctx, page_path).rstrip()
        if desc_md:
            body.append(desc_md + "\n")


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
        body.append(ui(extra_intro).rstrip() + "\n")

    docs = item.docs
    if LANG == LANG_ZH:
        docs = ZH_CONTAINER_DOCS.get(item.name) or _(item.docs)
    docs_md = rewrite_doc_links(docs, item.links, ctx, page_path)
    if docs_md:
        body.append(docs_md.rstrip() + "\n")

    body.append(f"## {ui('title_variants')}\n")
    if serde.tag:
        body.append(ui("meta_serialized_tag", tag=serde.tag) + "\n")
    body.append(f"| Tag value | {ui('variant_carries')} | {ui('variant_desc')} |")
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

        desc_md = _(v.docs or "").strip().replace("|", "\\|").replace("\n", "<br>")
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


def _walk_referenced_config_types(ty, index: dict[str, Item]):
    """Yield names of config-resident types referenced inside a rustdoc Type."""
    if not isinstance(ty, dict):
        return
    if "resolved_path" in ty:
        rp = ty["resolved_path"]
        target = index.get(str(rp.get("id"))) if rp.get("id") is not None else None
        if target is not None and target.is_config and target.name:
            yield target.name
        args = rp.get("args")
        if isinstance(args, dict) and "angle_bracketed" in args:
            for a in args["angle_bracketed"].get("args", []):
                if isinstance(a, dict) and "type" in a:
                    yield from _walk_referenced_config_types(a["type"], index)


def discover_config_types(index: dict[str, Item], roots: list[str]) -> list[Item]:
    """BFS from each root through fields/variants and return reachable config types.

    Result is in discovery order, with `roots` first.
    """
    by_name = {it.name: it for it in index.values() if it.is_config and it.name}
    seen: set[str] = set()
    out: list[Item] = []
    queue: list[str] = list(roots)
    while queue:
        name = queue.pop(0)
        if name in seen:
            continue
        seen.add(name)
        item = by_name.get(name)
        if item is None:
            continue
        out.append(item)
        if "struct" in item.inner:
            for fid in item.inner["struct"]["kind"]["plain"]["fields"]:
                f = index.get(str(fid))
                if f is None:
                    continue
                ty = f.inner.get("struct_field")
                for ref in _walk_referenced_config_types(ty, index):
                    if ref not in seen:
                        queue.append(ref)
        elif "enum" in item.inner:
            for vid in item.inner["enum"]["variants"]:
                v = index.get(str(vid))
                if v is None:
                    continue
                kind = v.inner["variant"]["kind"]
                if isinstance(kind, dict) and "tuple" in kind:
                    for fid in kind["tuple"]:
                        f = index.get(str(fid))
                        if f is None:
                            continue
                        ty = f.inner.get("struct_field")
                        for ref in _walk_referenced_config_types(ty, index):
                            if ref not in seen:
                                queue.append(ref)
    return out


def _enum_tuple_variant_targets(
    enum_item: Item,
    index: dict[str, Item],
    src: SourceAttrs,
) -> dict[str, tuple[str, str]]:
    """For each tuple-variant of an enum, return a map of carried-type name to
    (variant tag, variant friendly label)."""
    out: dict[str, tuple[str, str]] = {}
    container = parse_container_serde(attrs_for_container(enum_item, src))
    for vid in enum_item.inner["enum"]["variants"]:
        v = index.get(str(vid))
        if v is None:
            continue
        kind = v.inner["variant"]["kind"]
        if not (isinstance(kind, dict) and "tuple" in kind and kind["tuple"]):
            continue
        f = index.get(str(kind["tuple"][0]))
        if f is None:
            continue
        ty = f.inner.get("struct_field")
        if not (isinstance(ty, dict) and "resolved_path" in ty):
            continue
        target_id = ty["resolved_path"].get("id")
        target = index.get(str(target_id)) if target_id is not None else None
        if target is None or not target.name:
            continue
        vserde = parse_field_serde(attrs_for_member(v, enum_item.name, src))
        tag = vserde.rename or apply_rename_all(v.name or "", container.rename_all)
        # Variant name is the brand (e.g. `ShadowQuic`, `SunnyQuic`), so keep
        # its casing rather than camel-splitting into "Shadow quic".
        out[target.name] = (tag, v.name or "")
    return out


_CAMEL_SPLIT_RE = re.compile(r"[A-Z][a-z0-9]*|[A-Z]+(?=[A-Z]|$)|\d+")

# Acronyms in our config type names that should be uppercased even when split
# from the surrounding camel-case identifier. Keys are lowercase tokens.
_ACRONYMS = {
    "jls": "JLS",
    "dns": "DNS",
    "tls": "TLS",
    "mtu": "MTU",
    "url": "URL",
    "ip": "IP",
    "udp": "UDP",
    "tcp": "TCP",
    "rtt": "RTT",
}


def _capitalize_token(token: str, first: bool) -> str:
    low = token.lower()
    if low in _ACRONYMS:
        return _ACRONYMS[low]
    return token if first else low


def _friendly_label(name: str) -> str:
    """Turn `BrutalParams` / `SocksServerCfg` into a human-readable label.

    Common acronyms are preserved (so `DnsStrategy` -> `DNS strategy`,
    `JlsUpstream` -> `JLS upstream`).
    """
    if not name:
        return name
    if name.endswith("Cfg"):
        name = name[: -len("Cfg")]
    parts = _CAMEL_SPLIT_RE.findall(name) or [name]
    if not parts:
        return name
    out = [_capitalize_token(parts[0], first=True)]
    for p in parts[1:]:
        out.append(_capitalize_token(p, first=False))
    return " ".join(out)


def plan_pages(
    index: dict[str, Item],
    src_attrs: SourceAttrs,
) -> list[PageSpec]:
    """Decide which config items get their own page.

    Discovers the page set by walking the type graph from `Config` /
    `InboundCfg` / `OutboundCfg`. Tuple-variant targets of the two
    dispatcher enums become inbound/outbound pages; everything else
    reachable becomes a shared page. Adding a new struct/enum to
    `shadowquic/src/config/` is a no-op for this generator — re-running
    picks it up automatically.
    """
    by_name: dict[str, Item] = {
        it.name: it for it in index.values() if it.is_config and it.name
    }

    def need(name: str) -> Item:
        if name not in by_name:
            raise SystemExit(f"missing expected config type: {name}")
        return by_name[name]

    config = need("Config")
    inbound = need("InboundCfg")
    outbound = need("OutboundCfg")

    inbound_targets = _enum_tuple_variant_targets(inbound, index, src_attrs)
    outbound_targets = _enum_tuple_variant_targets(outbound, index, src_attrs)

    # BFS the full graph, including every variant target so we can classify them.
    discovered = discover_config_types(
        index,
        roots=["Config", "InboundCfg", "OutboundCfg"],
    )

    pages: list[PageSpec] = []
    placed: set[str] = set()

    def page_title(english: str, zh: str | None = None) -> str:
        return zh if LANG == LANG_ZH and zh else english

    pages.append(PageSpec(
        title="Config",
        nav_label="Overview",
        rel_path="configuration/index.md",
        item_id=int(config.id),
        intro=page_title(
            "Top-level configuration object. Every shadowquic config file deserializes into this struct.",
            "顶层配置对象。每个 shadowquic 配置文件都会反序列化成这个结构体。",
        ),
    ))
    placed.add("Config")

    pages.append(PageSpec(
        title="Inbound",
        nav_label="Overview",
        rel_path="configuration/inbound/index.md",
        item_id=int(inbound.id),
        intro=page_title(
            "Selects an inbound listener. Pick a variant via the `type` key.",
            "选择入站监听器。通过 `type` 键选择变体。",
        ),
    ))
    placed.add("InboundCfg")

    # Inbound variants, in the order rustdoc reports them.
    for it in discovered:
        if it.name in inbound_targets and it.name not in placed:
            tag, label = inbound_targets[it.name]
            pages.append(PageSpec(
                title=page_title(f"{label} server", f"{label} 服务端"),
                nav_label=page_title(f"{label} server", f"{label} 服务端"),
                rel_path=f"configuration/inbound/{tag}.md",
                item_id=int(it.id),
            ))
            placed.add(it.name)

    pages.append(PageSpec(
        title="Outbound",
        nav_label="Overview",
        rel_path="configuration/outbound/index.md",
        item_id=int(outbound.id),
        intro=page_title(
            "Selects the upstream the inbound forwards to. Pick a variant via the `type` key.",
            "选择入站转发到的上游。通过 `type` 键选择变体。",
        ),
    ))
    placed.add("OutboundCfg")

    for it in discovered:
        if it.name in outbound_targets and it.name not in placed:
            tag, label = outbound_targets[it.name]
            pages.append(PageSpec(
                title=page_title(f"{label} outbound", f"{label} 出站"),
                nav_label=page_title(f"{label} outbound", f"{label} 出站"),
                rel_path=f"configuration/outbound/{tag}.md",
                item_id=int(it.id),
            ))
            placed.add(it.name)

    # Everything else reachable from the roots becomes a shared type page.
    for it in discovered:
        if it.name in placed or not it.name:
            continue
        label = _friendly_label(it.name)
        zh_label = ZH_SHARED_LABELS.get(it.name, label)
        pages.append(PageSpec(
            title=page_title(label, zh_label),
            nav_label=page_title(label, zh_label),
            rel_path=f"configuration/shared/{it.name.lower()}.md",
            item_id=int(it.id),
        ))
        placed.add(it.name)

    return pages


# ----------------------------------------------------------------------------
# nav.yml rendering
# ----------------------------------------------------------------------------


NAV_BEGIN = "# >>> generated nav"
NAV_END = "# <<< generated nav"

# Static beginner's guide pages (docs/<lang>/guide/**). The generator owns the
# nav block, so guide entries are defined here instead of hand-editing the
# zensical config. In the bilingual layout each language has its own guide
# tree (guide/what-is.md, guide/quickstart.md, ...), so the nav differs per
# language.
def guide_nav_block() -> str:
    if LANG == LANG_ZH:
        return """  { "入门指南" = [
    { "入门指南" = "guide/index.md" },
    { "什么是 ShadowQUIC？" = "guide/what-is.md" },
    { "快速上手" = "guide/quickstart.md" },
    { "配置大白话" = "guide/configuration.md" },
    { "常见问题" = "guide/faq.md" }
  ] },"""
    return """  { "Guide" = [
    { "Guide" = "guide/index.md" },
    { "What is ShadowQUIC?" = "guide/what-is.md" },
    { "Quick Start" = "guide/quickstart.md" },
    { "Configuration explained" = "guide/configuration.md" },
    { "FAQ" = "guide/faq.md" }
  ] },"""


def render_nav(pages: list[PageSpec], include_protocol: bool = False) -> str:
    """Render a `nav = [...]` TOML block matching the page layout."""
    # Group pages by their second path component (configuration/<group>/...).
    cfg_pages = [p for p in pages if p.rel_path.startswith("configuration/")]

    # Overview page (configuration/index.md) is the section index.
    overview = next(p for p in cfg_pages if p.rel_path == "configuration/index.md")

    inbound_pages = [p for p in cfg_pages if p.rel_path.startswith("configuration/inbound/")]
    outbound_pages = [p for p in cfg_pages if p.rel_path.startswith("configuration/outbound/")]
    shared_pages = [p for p in cfg_pages if p.rel_path.startswith("configuration/shared/")]

    lines = ["nav = ["]
    lines.append(f'  {{ "{ui("nav_home")}" = "index.md" }},')
    lines.append(guide_nav_block())
    lines.append(f'  {{ "{ui("nav_config")}" = [')
    lines.append(f'    {{ "{ui("nav_config_overview")}" = "{overview.rel_path}" }},')

    if inbound_pages:
        lines.append(f'    {{ "{ui("nav_inbound")}" = [')
        for i, p in enumerate(inbound_pages):
            label = ui("nav_config_overview") if p.rel_path.endswith("/index.md") else p.nav_label
            comma = "," if i < len(inbound_pages) - 1 else ""
            lines.append(f'      {{ "{label}" = "{p.rel_path}" }}{comma}')
        lines.append('    ] },')

    if outbound_pages:
        lines.append(f'    {{ "{ui("nav_outbound")}" = [')
        for i, p in enumerate(outbound_pages):
            label = ui("nav_config_overview") if p.rel_path.endswith("/index.md") else p.nav_label
            comma = "," if i < len(outbound_pages) - 1 else ""
            lines.append(f'      {{ "{label}" = "{p.rel_path}" }}{comma}')
        lines.append('    ] },')

    if shared_pages:
        lines.append(f'    {{ "{ui("nav_shared")}" = [')
        for i, p in enumerate(shared_pages):
            comma = "," if i < len(shared_pages) - 1 else ""
            lines.append(f'      {{ "{p.nav_label}" = "{p.rel_path}" }}{comma}')
        lines.append('    ] }')

    lines.append('  ] },')
    lines.append(f'  {{ "{ui("nav_api")}" = "{API_REL_PATH}" }},')
    if include_protocol:
        lines.append(f'  {{ "{ui("nav_protocol")}" = "{PROTOCOL_REL_DIR}/index.md" }}')
    else:
        lines[-1] = lines[-1].rstrip(",")
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

## Getting started

New here? Read the [Beginner's Guide](guide/index.md) and follow the
[Quick Start](guide/quickstart.md).

## Configuration reference

This site documents the **YAML configuration schema**. The pages under
[Configuration](configuration/index.md) are generated from the doc comments on
the Rust structs in [`shadowquic/src/config/`](
https://github.com/spongebob888/shadowquic/tree/main/shadowquic/src/config),
so they stay in sync with the actual deserializer.

## Quick links

- [Beginner's Guide](guide/index.md)
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

LANDING_PAGE_ZH = """# shadowquic

一个带 SNI 伪装的 0-RTT QUIC 代理。

## 开始使用

初次接触？先读[零基础入门](guide/index.md)，然后跟着[快速上手](guide/quickstart.md)动手跑起来。

## 配置参考

本站整理了 **YAML 配置结构**。[配置](configuration/index.md) 下的页面由
[`shadowquic/src/config/`](https://github.com/spongebob888/shadowquic/tree/main/shadowquic/src/config)
中 Rust 结构体的文档注释自动生成，与实际反序列化器保持同步。

## 快速链接

- [零基础入门](guide/index.md)
- [顶层 `Config`](configuration/index.md)
- [入站类型](configuration/inbound/index.md)
- [出站类型](configuration/outbound/index.md)

## 示例

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

保存为 `config.yaml` 并运行：

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
        "--lang",
        choices=[LANG_EN, LANG_ZH],
        default=LANG_EN,
        help="which language tree to generate (docs/<lang>); nav goes to the matching zensical config",
    )
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
        help="wipe docs/<lang>/configuration before regenerating",
    )
    parser.add_argument(
        "--skip-protocol",
        action="store_true",
        help="don't render PROTOCOL.typ; useful when typst isn't installed",
    )
    args = parser.parse_args(argv)

    global LANG
    LANG = args.lang

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
    pages = plan_pages(index, src_attrs)

    page_for_type = {p.item_id: p.rel_path for p in pages}
    variant_tags, enum_defaults_by_id = collect_enum_variant_tags(index, src_attrs)
    ctx = RenderContext(
        index=index,
        page_for_type=page_for_type,
        default_values=defaults,
        pages_dir_for={p.item_id: (docs_root() / p.rel_path).parent for p in pages},
        src_attrs=src_attrs,
        enum_variant_tags=variant_tags,
        enum_defaults_by_id=enum_defaults_by_id,
    )

    # Wipe & recreate
    root = docs_root()
    cfg_dir = root / "configuration"
    if args.clean and cfg_dir.exists():
        shutil.rmtree(cfg_dir)
    root.mkdir(parents=True, exist_ok=True)

    # Landing page (only written if missing — user may want to customize).
    landing = root / "index.md"
    if not landing.exists():
        landing.write_text(LANDING_PAGE if LANG == LANG_EN else LANDING_PAGE_ZH)

    # Per-type pages
    for p in pages:
        item = Item(index[str(p.item_id)].raw)
        out_path = root / p.rel_path
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

    # User-management API reference
    out_path = write_api_page(REPO_ROOT)
    print(f"wrote {out_path.relative_to(REPO_ROOT)}", file=sys.stderr)

    # Beginner's guide (static markdown for the active language)
    guide_dir = write_guide_pages(REPO_ROOT)
    print(f"copied {guide_dir.relative_to(REPO_ROOT)}", file=sys.stderr)

    # Protocol spec (PROTOCOL.typ -> SVG pages -> markdown)
    include_protocol = False
    if not args.skip_protocol:
        if shutil.which("typst") is None:
            print(
                "warn: `typst` not found on PATH; skipping PROTOCOL.typ render. "
                "Install typst or pass --skip-protocol to silence.",
                file=sys.stderr,
            )
        else:
            svg_pages = build_protocol_pages(REPO_ROOT)
            pdf_link = (
                f"https://github.com/spongebob888/shadowquic/raw/main/{PROTOCOL_PDF_NAME}"
                if (REPO_ROOT / PROTOCOL_PDF_NAME).exists()
                else None
            )
            out_path = write_protocol_page(svg_pages, pdf_link)
            print(f"wrote {out_path.relative_to(REPO_ROOT)}", file=sys.stderr)
            include_protocol = True

    # Update nav in the active language's config.
    cfg = config_file()
    patch_zensical_nav(cfg, render_nav(pages, include_protocol=include_protocol))
    print(f"updated nav in {cfg.relative_to(REPO_ROOT)}", file=sys.stderr)

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
