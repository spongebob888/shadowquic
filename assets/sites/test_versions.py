"""Regression checks for historical version selection and navigation."""
import unittest

from build_versions import release_tags
from gen_docs import PageSpec, render_nav


class VersionTests(unittest.TestCase):
    def test_release_order_is_numeric_and_starts_at_030(self):
        self.assertEqual(
            release_tags(["v0.3.9", "v0.2.9", "v0.3.0", "v0.3.13", "v1.0.0"]),
            ["v1.0.0", "v0.3.13", "v0.3.9", "v0.3.0"],
        )

    def test_main_aliases_and_prereleases_are_not_stable_tags(self):
        self.assertEqual(
            release_tags(["main", "latest", "v0.3.14-rc.1", "v0.3", "v-next"]),
            [],
        )

    def test_historical_navigation_omits_unavailable_sections(self):
        overview = PageSpec(
            title="Config", nav_label="Overview",
            rel_path="configuration/index.md", item_id=0,
        )
        for api in (False, True):
            for protocol in (False, True):
                with self.subTest(api=api, protocol=protocol):
                    nav = render_nav([overview], include_api=api, include_protocol=protocol)
                    self.assertEqual('"api.md"' in nav, api)
                    self.assertEqual('"protocol/index.md"' in nav, protocol)
                    self.assertIn('"configuration/index.md"', nav)


if __name__ == "__main__":
    unittest.main()
