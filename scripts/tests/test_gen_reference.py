"""Tests for scripts/gen-reference.py. Run: python3 -m unittest scripts/tests/test_gen_reference.py"""

from __future__ import annotations

import importlib.util
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent
spec = importlib.util.spec_from_file_location("gen_reference", ROOT / "scripts" / "gen-reference.py")
gen = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gen)

STUB = '''
Row = dict[str, Any]
"""One result row."""

class MushroomError(RuntimeError):
    """Base class."""

    code: str | None

class KeyNotFound(MushroomError):
    """No node with this key.

    More detail that is not the summary.
    """

    code: str
    key: str

class GraphDb:
    """A store."""

    @staticmethod
    def open(path: str, read_only: bool = False) -> GraphDb:
        """Open the store at `path`."""

    def find(
        self,
        field: str,
        k: int = 10,
    ) -> list[tuple[str, float]]:
        """Find things.

        Returns a Vec<u32> of hits, or `Option<f64>` when asked.
        """

    def __enter__(self) -> GraphDb: ...
'''

TEMPLATE = "# T\n\n{{types}}\n\n{{methods}}\n\n{{errors}}\n"


class Helpers(unittest.TestCase):
    def test_a_bare_angle_bracket_is_escaped_and_one_in_code_is_not(self):
        self.assertEqual(gen.escape_html("a Vec<u32> and `Option<f64>`"), "a Vec\\<u32> and `Option<f64>`")

    def test_a_fenced_block_and_a_code_span_that_wraps_are_left_alone(self):
        fenced = "before <a>\n\n```python\nx: list[Vec<u32>] = []\n```\n\nrun `apply <db>\n--now` after"
        self.assertEqual(
            gen.escape_html(fenced),
            "before \\<a>\n\n```python\nx: list[Vec<u32>] = []\n```\n\nrun `apply <db>\n--now` after",
        )

    def test_a_table_cell_keeps_its_pipes_and_loses_its_newlines(self):
        self.assertEqual(gen.cell("one | two\n  three"), "one \\| two three")

    def test_a_fence_names_its_language(self):
        self.assertEqual(gen.fence("x", "python"), "```python\nx\n```")


class Stub(unittest.TestCase):
    def setUp(self):
        self.stub = gen.parse_stub(STUB)

    def test_methods_are_read_in_order_and_dunders_are_left_out(self):
        self.assertEqual([m[0] for m in self.stub["methods"]], ["open", "find"])

    def test_a_signature_is_the_stubs_own_text_with_its_decorator(self):
        self.assertEqual(self.stub["methods"][0][1], "@staticmethod\ndef open(path: str, read_only: bool = False) -> GraphDb")

    def test_a_signature_that_wraps_keeps_its_lines(self):
        self.assertEqual(
            self.stub["methods"][1][1],
            "def find(\n    self,\n    field: str,\n    k: int = 10,\n) -> list[tuple[str, float]]",
        )

    def test_an_error_lists_its_fields_without_code(self):
        self.assertEqual(self.stub["errors"][1][0], "KeyNotFound")
        self.assertEqual(self.stub["errors"][1][3], [("key", "str")])

    def test_an_alias_carries_the_docstring_under_it(self):
        self.assertEqual(self.stub["aliases"], [("Row", "dict[str, Any]", "One result row.")])


class Python(unittest.TestCase):
    def setUp(self):
        self.stub = gen.parse_stub(STUB)

    def test_the_page_has_each_method_under_its_group(self):
        page = gen.render_python(TEMPLATE, self.stub, {"Opening": ["open"], "Reading": ["find"]})
        self.assertIn("## Opening\n\n### `open`", page)
        self.assertIn("```python\n@staticmethod\ndef open(path: str, read_only: bool = False) -> GraphDb\n```", page)
        self.assertIn("Returns a Vec\\<u32> of hits", page)

    def test_a_method_in_no_group_fails_by_name(self):
        with self.assertRaises(gen.Problem) as raised:
            gen.render_python(TEMPLATE, self.stub, {"Opening": ["open"]})
        self.assertIn("find", str(raised.exception))

    def test_a_group_naming_a_method_the_stub_lacks_fails_by_name(self):
        with self.assertRaises(gen.Problem) as raised:
            gen.render_python(TEMPLATE, self.stub, {"All": ["open", "find", "gone"]})
        self.assertIn("gone", str(raised.exception))

    def test_an_error_row_is_its_summary_line_only(self):
        page = gen.render_python(TEMPLATE, self.stub, {"All": ["open", "find"]})
        self.assertIn("| `KeyNotFound` | `key: str` | No node with this key. |", page)
        self.assertNotIn("More detail", page)

    def test_a_template_missing_a_marker_fails(self):
        with self.assertRaises(gen.Problem):
            gen.render_python("# T\n\n{{methods}}\n", self.stub, {"All": ["open", "find"]})


HELP = """mushroomdb — embedded graph database

Usage:
  mushroomdb serve <db-dir> [--addr 127.0.0.1:8080]
                     --addr is where it listens
  mushroomdb schema apply <db-dir> <schema.json>
  mushroomdb schema apply <db-dir> --memory-defaults
                                   applies the built-in memory schema
  mushroomdb --version

Default serve address is 127.0.0.1:8080.
A second line of notes.
"""

CLI_TEMPLATE = "# CLI\n\n{{usage serve}}\n\nServes.\n\n{{usage schema}}\n\n{{usage --version}}\n\n{{notes}}\n"


class Cli(unittest.TestCase):
    def test_entries_are_read_by_subcommand_with_their_continuation_lines(self):
        entries, _ = gen.parse_usage(HELP)
        self.assertEqual(list(entries), ["serve", "schema", "--version"])
        self.assertEqual(
            entries["serve"],
            ["mushroomdb serve <db-dir> [--addr 127.0.0.1:8080]\n                   --addr is where it listens"],
        )

    def test_a_subcommand_listed_twice_keeps_both_forms(self):
        entries, _ = gen.parse_usage(HELP)
        self.assertEqual(len(entries["schema"]), 2)
        self.assertTrue(entries["schema"][1].endswith("applies the built-in memory schema"))

    def test_the_notes_are_what_follows_the_usage(self):
        _, notes = gen.parse_usage(HELP)
        self.assertEqual(notes, "Default serve address is 127.0.0.1:8080.\nA second line of notes.")

    def test_help_with_no_usage_line_fails(self):
        with self.assertRaises(gen.Problem):
            gen.parse_usage("mushroomdb\n")

    def test_the_page_carries_each_usage_in_a_text_block(self):
        page = gen.render_cli(CLI_TEMPLATE, HELP)
        self.assertIn("```text\nmushroomdb schema apply <db-dir> <schema.json>\nmushroomdb schema apply <db-dir> --memory-defaults\n", page)
        self.assertIn("Serves.", page)
        self.assertIn("```text\nDefault serve address is 127.0.0.1:8080.\nA second line of notes.\n```", page)
        self.assertNotIn("{{", page)

    def test_a_subcommand_the_template_omits_fails_by_name(self):
        with self.assertRaises(gen.Problem) as raised:
            gen.render_cli(CLI_TEMPLATE.replace("{{usage schema}}\n\n", ""), HELP)
        self.assertIn("schema", str(raised.exception))

    def test_a_marker_for_a_subcommand_the_cli_lacks_fails_by_name(self):
        with self.assertRaises(gen.Problem) as raised:
            gen.render_cli(CLI_TEMPLATE + "\n{{usage gone}}\n", HELP)
        self.assertIn("gone", str(raised.exception))

    def test_a_binary_backed_page_without_a_binary_says_what_it_needs(self):
        self.assertEqual(gen.main(["--check", "cli"]), 1)


class RealTree(unittest.TestCase):
    def test_every_method_in_the_real_stub_has_a_group(self):
        stub = gen.parse_stub((ROOT / "bindings" / "python" / "mushroomdb.pyi").read_text(encoding="utf-8"))
        names = [m[0] for m in stub["methods"]]
        grouped = [n for members in gen.GROUPS.values() for n in members]
        self.assertEqual(sorted(names), sorted(grouped))

    def test_the_committed_python_page_is_current(self):
        self.assertEqual(gen.main(["--check", "python-api"]), 0)


if __name__ == "__main__":
    unittest.main()
