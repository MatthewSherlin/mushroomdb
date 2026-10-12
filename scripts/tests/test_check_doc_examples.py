"""Tests for scripts/check-doc-examples.py. Run: python3 -m unittest scripts/tests/test_check_doc_examples.py"""

from __future__ import annotations

import importlib.util
import stat
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent
spec = importlib.util.spec_from_file_location("check_doc_examples", ROOT / "scripts" / "check-doc-examples.py")
chk = importlib.util.module_from_spec(spec)
spec.loader.exec_module(chk)

F = "```"
PAGE = f"""# Page

{F}sh
mushroomdb demo ./db
mushroomdb stats ./db
{F}

{F}text
nodes: 60 live, 0 tombstoned
…
rules: 7
{F}

Prose in between.

{F}sh no-run
mushroomdb serve ./db
{F}

{F}sh
mushroomdb why ./db a b
{F}

{F}python
print("not a shell block")
{F}
"""


def fake_binary(directory: Path, script: str) -> str:
    """An executable that stands in for mushroomdb."""
    path = directory / "fake-mushroomdb"
    path.write_text("#!/bin/sh\n" + script, encoding="utf-8")
    path.chmod(path.stat().st_mode | stat.S_IXUSR)
    return str(path)


class Parsing(unittest.TestCase):
    def test_blocks_carry_their_info_string_and_line(self):
        found = chk.blocks(PAGE)
        self.assertEqual([b[0] for b in found], ["sh", "text", "sh no-run", "sh", "python"])
        self.assertEqual(found[0][1], 3)
        self.assertEqual(found[0][2], ["mushroomdb demo ./db", "mushroomdb stats ./db"])

    def test_an_example_is_an_sh_block_and_the_text_block_right_after_it(self):
        found = chk.examples(PAGE)
        self.assertEqual(len(found), 2)
        self.assertEqual(found[0][1], ["mushroomdb demo ./db", "mushroomdb stats ./db"])
        self.assertEqual(found[0][2], ["nodes: 60 live, 0 tombstoned", "rules: 7"])
        self.assertEqual(found[1][2], [])

    def test_a_block_marked_no_run_is_not_an_example(self):
        self.assertNotIn(["mushroomdb serve ./db"], [e[1] for e in chk.examples(PAGE)])

    def test_a_text_block_after_prose_belongs_to_no_command(self):
        page = f"{F}sh\nmushroomdb stats ./db\n{F}\n\nWhich prints:\n\n{F}text\nnodes: 1\n{F}\n"
        self.assertEqual(chk.examples(page)[0][2], [])

    def test_a_command_that_is_not_mushroomdb_is_refused(self):
        with self.assertRaises(chk.Problem) as raised:
            chk.examples(f"{F}sh\nrm -rf ./db\n{F}\n")
        self.assertIn("rm -rf ./db", str(raised.exception))


class Running(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.dir = Path(self.tmp.name)

    def page(self, text: str) -> str:
        path = self.dir / "page.md"
        path.write_text(text, encoding="utf-8")
        return str(path)

    def test_output_the_page_shows_and_the_command_prints_passes(self):
        binary = fake_binary(self.dir, 'echo "  nodes: 60 live, 0 tombstoned"; echo "edges: 334"\n')
        page = self.page(f"{F}sh\nmushroomdb stats ./db\n{F}\n\n{F}text\nnodes: 60 live, 0 tombstoned\n{F}\n")
        self.assertEqual(chk.main(["--binary", binary, page]), 0)

    def test_output_the_command_does_not_print_fails(self):
        binary = fake_binary(self.dir, 'echo "nodes: 61 live, 0 tombstoned"\n')
        page = self.page(f"{F}sh\nmushroomdb stats ./db\n{F}\n\n{F}text\nnodes: 60 live, 0 tombstoned\n{F}\n")
        self.assertEqual(chk.main(["--binary", binary, page]), 1)

    def test_a_command_that_exits_non_zero_fails(self):
        binary = fake_binary(self.dir, "echo broken >&2; exit 3\n")
        page = self.page(f"{F}sh\nmushroomdb stats ./db\n{F}\n")
        self.assertEqual(chk.main(["--binary", binary, page]), 1)

    def test_quoted_arguments_reach_the_binary_as_one_argument(self):
        binary = fake_binary(self.dir, 'echo "argc=$#"; echo "last=$3"\n')
        page = self.page(
            f"{F}sh\nmushroomdb query ./db \"MATCH (p {{id: 'a'}}) RETURN p\"\n{F}\n\n"
            f"{F}text\nargc=3\nlast=MATCH (p {{id: 'a'}}) RETURN p\n{F}\n"
        )
        self.assertEqual(chk.main(["--binary", binary, page]), 0)

    def test_commands_on_one_page_share_a_directory(self):
        binary = fake_binary(self.dir, 'if [ "$1" = demo ]; then mkdir "$2"; else ls -d "$2"; fi\n')
        page = self.page(f"{F}sh\nmushroomdb demo ./db\n{F}\n\n{F}sh\nmushroomdb stats ./db\n{F}\n\n{F}text\n./db\n{F}\n")
        self.assertEqual(chk.main(["--binary", binary, page]), 0)

    def test_a_page_that_is_not_there_is_one_line_and_not_a_traceback(self):
        self.assertEqual(chk.main(["--binary", "/bin/echo", str(self.dir / "nope.md")]), 1)


if __name__ == "__main__":
    unittest.main()
