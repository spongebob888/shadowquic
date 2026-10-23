"""Regression checks for configuration discovery and navigation."""
import tempfile
import tomllib
import unittest
from pathlib import Path

from gen_docs import (
    DNS_CONFIG_SRC, REPO_ROOT, Item, RenderContext, SourceAttrs, collect_default_fn_values,
    config_sources, emit_struct_page,
    parse_container_serde, parse_field_serde, parse_source_attrs, plan_pages, render_nav,
)


class DnsDocsTests(unittest.TestCase):
    def index(self, include_dns=True):
        def item(id, name, inner, filename="shadowquic/src/config/mod.rs"):
            return Item(dict(id=id, name=name, inner=inner, span=dict(filename=filename)))

        items = [
            item(0, "Config", {"struct": {"kind": {"plain": {"fields": [3] if include_dns else []}}}}),
            item(1, "InboundCfg", {"enum": {"variants": []}}),
            item(2, "OutboundCfg", {"enum": {"variants": []}}),
        ]
        if include_dns:
            items.extend([
                item(3, "dns", {"struct_field": {"resolved_path": {
                    "id": 99, "args": {"angle_bracketed": {"args": [
                        {"type": {"resolved_path": {"id": 4}}},
                    ]}},
                }}}),
                item(4, "DnsCfg", {"enum": {"variants": [5]}}, DNS_CONFIG_SRC),
                item(5, "Udp", {"variant": {"kind": {"tuple": [6]}}}, DNS_CONFIG_SRC),
                item(6, None, {"struct_field": {"resolved_path": {"id": 7}}}, DNS_CONFIG_SRC),
                item(7, "DnsUdpServerCfg", {"struct": {"kind": {"plain": {"fields": []}}}}, DNS_CONFIG_SRC),
            ])
        return {it.id: it for it in items}

    def test_dns_and_variant_configuration_are_discovered(self):
        pages = plan_pages(self.index(), parse_source_attrs(REPO_ROOT))
        paths = {p.item_id: p.rel_path for p in pages}
        self.assertEqual(paths[4], "configuration/dns/index.md")
        self.assertEqual(paths[7], "configuration/dns/dns-udp.md")
        nav = tomllib.loads(render_nav(pages))["nav"]
        configuration = next(entry["Configuration"] for entry in nav if "Configuration" in entry)
        dns = next(entry["DNS"] for entry in configuration if "DNS" in entry)
        self.assertEqual(dns, [
            {"Overview": "configuration/dns/index.md"},
            {"DNS UDP server": "configuration/dns/dns-udp.md"},
        ])
        self.assertFalse(any(p.rel_path.startswith("configuration/shared/") for p in pages))

    def test_historical_configuration_without_dns_omits_navigation(self):
        pages = plan_pages(self.index(include_dns=False), SourceAttrs())
        self.assertNotIn('"DNS"', render_nav(pages))
        with tempfile.TemporaryDirectory() as directory:
            self.assertEqual(list(config_sources(Path(directory))), [])

    def test_dns_serde_names_are_recovered_from_source(self):
        attrs = parse_source_attrs(REPO_ROOT)
        self.assertEqual(parse_container_serde(attrs.container["DnsCfg"]).tag, "type")
        self.assertEqual(parse_field_serde(attrs.member["DnsCfg", "Udp"]).rename, "dns-udp")
        server = parse_container_serde(attrs.container["DnsUdpServerCfg"])
        self.assertEqual(server.rename_all, "kebab-case")
        self.assertTrue(server.deny_unknown_fields)


class TunDocsTests(unittest.TestCase):
    def test_tun_defaults_render_as_values_from_public_helpers(self):
        filename = "shadowquic/src/config/tun.rs"
        fields = [
            ("ipv4", {"resolved_path": {"path": "Ipv4Net"}}, "172.19.0.1/30"),
            ("ipv6", {"resolved_path": {
                "path": "Option", "args": {"angle_bracketed": {"args": [
                    {"type": {"resolved_path": {"path": "Ipv6Net"}}},
                ]}},
            }}, "fdfe:dcba:9876::1/126"),
            ("mtu", {"primitive": "u16"}, "1500"),
            ("max_sessions", {"primitive": "usize"}, "1024"),
            ("idle_timeout", {"primitive": "u64"}, "300"),
        ]
        config = Item(dict(
            id=0, name="TunServerCfg", span=dict(filename=filename),
            inner={"struct": {"kind": {"plain": {"fields": list(range(1, 6))}}}},
        ))
        index = {config.id: config}
        for id, (name, ty, _) in enumerate(fields, start=1):
            index[str(id)] = Item(dict(
                id=id, name=name, span=dict(filename=filename), inner={"struct_field": ty},
            ))
        ctx = RenderContext(
            index=index, page_for_type={}, pages_dir_for={},
            default_values=collect_default_fn_values(REPO_ROOT),
            src_attrs=parse_source_attrs(REPO_ROOT),
        )
        rendered = emit_struct_page(config, Path("configuration/inbound/tun.md"), "Tun server", ctx)
        for name, _, expected in fields:
            with self.subTest(field=name):
                section = rendered.split(f"### `{name.replace('_', '-')}`\n", 1)[1].split("\n### ", 1)[0]
                self.assertIn(f"- **Default:** `{expected}`", section)
                self.assertNotIn("default_", section)
                if name == "ipv6":
                    self.assertIn("*(optional)*", section)


class RestApiDocsTests(unittest.TestCase):
    def test_dashboard_defaults_are_rendered_from_public_helpers(self):
        filename = "shadowquic/src/config/mod.rs"
        config = Item(dict(
            id=0, name="ApiConfig", span=dict(filename=filename),
            inner={"struct": {"kind": {"plain": {"fields": [1, 2]}}}},
        ))
        index = {config.id: config}
        for id, name in enumerate(["tag", "ui_download_url"], start=1):
            index[str(id)] = Item(dict(
                id=id, name=name, span=dict(filename=filename),
                inner={"struct_field": {"resolved_path": {"path": "String"}}},
            ))
        ctx = RenderContext(
            index=index, page_for_type={}, pages_dir_for={},
            default_values=collect_default_fn_values(REPO_ROOT),
            src_attrs=parse_source_attrs(REPO_ROOT),
        )
        rendered = emit_struct_page(config, Path("configuration/restful-api.md"), "REST API", ctx)
        self.assertIn("- **Default:** `restful-api`", rendered)
        self.assertIn("- **Default:** `https://github.com/spongebob888/squirrelboard/releases/download/v0.1.2/index.html`", rendered)
        self.assertNotIn(".into()", rendered)

    def index(self, include_api=True):
        def item(id, name, inner):
            return Item(dict(id=id, name=name, inner=inner,
                             span=dict(filename="shadowquic/src/config/mod.rs")))

        items = [
            item(0, "Config", {"struct": {"kind": {"plain": {"fields": [3] if include_api else []}}}}),
            item(1, "InboundCfg", {"enum": {"variants": []}}),
            item(2, "OutboundCfg", {"enum": {"variants": []}}),
            item(3, "restful_api", {"struct_field": {"resolved_path": {
                "path": "Option", "id": 99, "args": {"angle_bracketed": {"args": [
                    {"type": {"resolved_path": {"id": 4}}},
                ]}},
            }}}),
            item(4, "ApiConfig", {"struct": {"kind": {"plain": {"fields": []}}}}),
        ]
        return {it.id: it for it in items}

    def test_rest_api_has_dedicated_configuration_page_and_navigation(self):
        pages = plan_pages(self.index(), SourceAttrs())
        paths = {p.item_id: p.rel_path for p in pages}
        self.assertEqual(paths[4], "configuration/restful-api.md")
        nav = tomllib.loads(render_nav(pages))["nav"]
        configuration = next(entry["Configuration"] for entry in nav if "Configuration" in entry)
        self.assertIn({"REST API": "configuration/restful-api.md"}, configuration)
        self.assertFalse(any(p.rel_path == "configuration/shared/apiconfig.md" for p in pages))

    def test_unreachable_api_config_omits_page_and_navigation(self):
        pages = plan_pages(self.index(include_api=False), SourceAttrs())
        self.assertFalse(any(p.item_id == 4 for p in pages))
        self.assertNotIn('"REST API"', render_nav(pages))


if __name__ == "__main__":
    unittest.main()
