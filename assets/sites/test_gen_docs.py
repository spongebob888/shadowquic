"""Regression checks for DNS configuration discovery and navigation."""
import tempfile
import tomllib
import unittest
from pathlib import Path

from gen_docs import (
    DNS_CONFIG_SRC, REPO_ROOT, Item, SourceAttrs, config_sources,
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


if __name__ == "__main__":
    unittest.main()
