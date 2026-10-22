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


RECALL = {
    "name": "recall",
    "description": "What do I already know about this — ranked nodes.\nSecond line.",
    "inputSchema": {
        "type": "object",
        "properties": {
            "topic": {"type": "string", "description": "Free text | any words."},
            "json": {"type": "boolean"},
        },
        "required": ["topic"],
    },
}
WAS_LINKED = {
    "name": "was_linked",
    "description": "Advanced: Were A and B linked on DATE.",
    "inputSchema": {
        "type": "object",
        "properties": {
            "at_commit": {"anyOf": [{"type": "string"}, {"type": "integer"}], "description": "When."},
            "dir": {"enum": ["out", "in"]},
            "keys": {"type": "array", "items": {"type": "string"}},
        },
        "required": ["at_commit"],
    },
}
STATS = {"name": "stats", "description": "Advanced: Live counts.", "inputSchema": {"type": "object", "properties": {}}}
MCP_TEMPLATE = "# Tools\n\n{{counts}}\n\n## Task\n\n{{task_tools}}\n\n## Graph\n\n{{graph_tools}}\n"


class Mcp(unittest.TestCase):
    def test_a_type_is_what_the_schema_accepts(self):
        self.assertEqual(gen.schema_type({"type": "string"}), "string")
        self.assertEqual(gen.schema_type({"anyOf": [{"type": "string"}, {"type": "integer"}]}), "string or integer")
        self.assertEqual(gen.schema_type({"enum": ["out", "in"]}), 'one of `"out"`, `"in"`')
        self.assertEqual(gen.schema_type({"type": "array", "items": {"type": "string"}}), "array of string")
        self.assertEqual(gen.schema_type({}), "any")

    def test_a_tool_has_its_arguments_in_a_table(self):
        text = gen.render_tool(RECALL, listed=True)
        self.assertIn("### `recall`", text)
        self.assertIn("| `topic` | string | yes | Free text \\| any words. |", text)
        self.assertIn("| `json` | boolean | no |  |", text)
        self.assertNotIn("--all-tools", text)

    def test_the_advanced_prefix_is_not_printed(self):
        text = gen.render_tool(WAS_LINKED, listed=True)
        self.assertIn("\nWere A and B linked on DATE.\n", text)
        self.assertIn("| `at_commit` | string or integer | yes | When. |", text)

    def test_an_unlisted_tool_says_how_to_list_it(self):
        self.assertIn("`--all-tools`", gen.render_tool(STATS, listed=False))

    def test_a_tool_with_no_arguments_says_so(self):
        self.assertIn("Takes no arguments.", gen.render_tool(STATS, listed=True))

    def test_the_page_sorts_tools_by_the_surface_they_belong_to(self):
        page = gen.render_mcp(MCP_TEMPLATE, [RECALL, WAS_LINKED], [RECALL, WAS_LINKED, STATS])
        self.assertIn("The server answers 3 tools and lists 2 of them by default.", page)
        task, graph = page.split("## Graph")
        self.assertIn("### `recall`", task)
        self.assertNotIn("### `was_linked`", task)
        self.assertIn("### `was_linked`", graph)
        self.assertIn("### `stats`", graph)


class ReviewFindings(unittest.TestCase):
    def test_a_method_written_on_one_line_keeps_its_signature(self):
        stub = gen.parse_stub("class GraphDb:\n    def close(self) -> None: ...\n")
        self.assertEqual(stub["methods"], [("close", "def close(self) -> None", "")])

    def test_an_async_method_fails_by_name(self):
        with self.assertRaises(gen.Problem) as raised:
            gen.parse_stub('class GraphDb:\n    async def later(self) -> None:\n        """Doc."""\n')
        self.assertIn("later", str(raised.exception))

    def test_a_method_declared_twice_fails_by_name(self):
        source = 'class GraphDb:\n    def get(self, a: int) -> int:\n        """One."""\n\n    def get(self, a: str) -> str:\n        """Two."""\n'
        with self.assertRaises(gen.Problem) as raised:
            gen.parse_stub(source)
        self.assertIn("get", str(raised.exception))

    def test_a_module_level_function_fails_by_name(self):
        with self.assertRaises(gen.Problem) as raised:
            gen.parse_stub('def connect(path: str) -> None:\n    """Doc."""\n')
        self.assertIn("connect", str(raised.exception))

    def test_a_class_that_is_not_an_error_is_not_listed_as_one(self):
        source = 'class MushroomError(RuntimeError):\n    """Base."""\n\nclass Report(TypedDict):\n    """A shape."""\n\n    rows: int\n'
        with self.assertRaises(gen.Problem) as raised:
            gen.parse_stub(source)
        self.assertIn("Report", str(raised.exception))

    def test_a_value_class_is_listed_under_types_with_its_constructor(self):
        source = 'class Duration:\n    """A span."""\n\n    months: int\n    def __init__(self, months: int = 0) -> None: ...\n\nclass GraphDb:\n    def close(self) -> None: ...\n'
        stub = gen.parse_stub(source)
        self.assertEqual(stub["classes"], [("Duration", "A span.", [("months", "int")], "months: int = 0")])
        page = gen.render_python("{{types}}\n{{methods}}\n{{errors}}", stub, {"All": ["close"]})
        self.assertIn("```python\nclass Duration(months: int = 0)\n```", page)
        self.assertIn("Fields: `months: int`.", page)

    def test_an_error_two_levels_down_is_still_an_error(self):
        source = 'class MushroomError(RuntimeError):\n    """Base."""\n\nclass Busy(MushroomError):\n    """Busy."""\n\nclass VeryBusy(Busy):\n    """Very."""\n'
        self.assertEqual([e[0] for e in gen.parse_stub(source)["errors"]], ["MushroomError", "Busy", "VeryBusy"])

    def test_a_subcommand_indented_as_a_continuation_fails_by_name(self):
        text = HELP.replace("  mushroomdb --version", "    mushroomdb frobnicate <db-dir>\n  mushroomdb --version")
        with self.assertRaises(gen.Problem) as raised:
            gen.parse_usage(text)
        self.assertIn("frobnicate", str(raised.exception))

    def test_a_blank_line_inside_the_usage_fails_and_says_so(self):
        text = HELP.replace("  mushroomdb --version", "\n  mushroomdb --version")
        with self.assertRaises(gen.Problem) as raised:
            gen.parse_usage(text)
        self.assertIn("blank line", str(raised.exception))

    def test_a_binary_that_is_not_there_is_one_line_and_not_a_traceback(self):
        self.assertEqual(gen.main(["--check", "--binary", "/nonexistent/mushroomdb", "cli"]), 1)

    def test_the_fields_of_a_nested_argument_are_listed_under_it(self):
        tool = {
            "name": "remember",
            "description": "Write a note.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "entities": {
                        "type": "array",
                        "description": "What it names.",
                        "items": {
                            "type": "object",
                            "properties": {"key": {"type": "string", "description": "Its key."}, "aliases": {"type": "array", "items": {"type": "string"}}},
                            "required": ["key"],
                        },
                    },
                    "fact": {"type": "object", "properties": {"subject": {"type": "string"}}, "required": ["subject"]},
                },
                "required": [],
            },
        }
        text = gen.render_tool(tool, listed=True)
        self.assertIn("| `entities` | array of object | no | What it names. |", text)
        self.assertIn("| `entities[].key` | string | yes | Its key. |", text)
        self.assertIn("| `entities[].aliases` | array of string | no |  |", text)
        self.assertIn("| `fact.subject` | string | yes |  |", text)


class RealTree(unittest.TestCase):
    def test_every_method_in_the_real_stub_has_a_group(self):
        stub = gen.parse_stub((ROOT / "bindings" / "python" / "mushroomdb.pyi").read_text(encoding="utf-8"))
        names = [m[0] for m in stub["methods"]] + [m[0] for m in stub["memory_methods"]]
        grouped = [n for members in gen.GROUPS.values() for n in members]
        self.assertEqual(sorted(names), sorted(grouped))

    def test_the_memory_methods_come_from_the_accessor_and_the_aliases_are_a_table(self):
        stub = gen.parse_stub((ROOT / "bindings" / "python" / "mushroomdb.pyi").read_text(encoding="utf-8"))
        memory = [m[0] for m in stub["memory_methods"]]
        self.assertEqual(sorted(memory), sorted(gen.GROUPS["Memory"]))
        self.assertEqual(sorted(m[0] for m in stub["deprecated"]), sorted(memory))
        page = gen.generate("python-api", None)
        self.assertIn("### `memory.remember`", page)
        self.assertIn("| `remember` | `memory.remember` |", page)
        self.assertNotIn("### `remember`", page)

    def test_the_committed_python_page_is_current(self):
        self.assertEqual(gen.main(["--check", "python-api"]), 0)


if __name__ == "__main__":
    unittest.main()
