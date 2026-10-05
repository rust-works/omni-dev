"""Tests for check_glibc_floor.py: the parser, on readelf output."""

from __future__ import annotations

import pathlib
import re
import shutil
import sys
import unittest

import check_glibc_floor as cgf

# The needs table is verbatim from `readelf -V /bin/ls` (binutils 2.38, Ubuntu
# 22.04), trimmed to four libc entries. The `.gnu.version` table before it is cut
# down, and its GLIBC_2.99 is invented: that table lists names per symbol and must
# be ignored, so a parser that read it would see 2.99.
REAL = """
Version symbols section '.gnu.version' contains 123 entries:
 Addr: 0x000000000000152e  Offset: 0x00152e  Link: 6 (.dynsym)
  000:   0 (*local*)       2 (GLIBC_2.3)     3 (GLIBC_2.2.5)   5 (GLIBC_2.34)
  004:   3 (GLIBC_2.2.5)   9 (GLIBC_2.99)    3 (GLIBC_2.2.5)

Version needs section '.gnu.version_r' contains 2 entries:
 Addr: 0x0000000000001628  Offset: 0x001628  Link: 7 (.dynstr)
  000000: Version: 1  File: libselinux.so.1  Cnt: 1
  0x0010:   Name: LIBSELINUX_1.0  Flags: none  Version: 8
  0x0020: Version: 1  File: libc.so.6  Cnt: 4
  0x0030:   Name: GLIBC_2.28  Flags: none  Version: 11
  0x0040:   Name: GLIBC_2.33  Flags: none  Version: 9
  0x0050:   Name: GLIBC_2.4  Flags: none  Version: 6
  0x0060:   Name: GLIBC_2.2.5  Flags: none  Version: 3
"""

# The shape the issue reports for v0.45.0 (x86_64): hard 2.38, weak 2.39.
WEAK = """
Version needs section '.gnu.version_r' contains 1 entries:
 Addr: 0x0000000000001628  Offset: 0x001628  Link: 7 (.dynstr)
  000000: Version: 1  File: libc.so.6  Cnt: 3
  0x0010:   Name: GLIBC_2.2.5  Flags: none  Version: 3
  0x0020:   Name: GLIBC_2.38  Flags: none  Version: 4
  0x0030:   Name: GLIBC_2.39  Flags: WEAK  Version: 2
"""

NEXT_SECTION = """
Version needs section '.gnu.version_r' contains 1 entries:
  0x0020: Version: 1  File: libc.so.6  Cnt: 1
  0x0030:   Name: GLIBC_2.30  Flags: none  Version: 11

Version definition section '.gnu.version_d' contains 1 entries:
  0x0000:   Name: GLIBC_2.99  Flags: none  Version: 1
"""


class ParseTests(unittest.TestCase):
    def test_reads_only_the_version_needs_table(self) -> None:
        hard, weak = cgf.glibc_needs(REAL)
        self.assertEqual(sorted(hard), [(2, 2, 5), (2, 4), (2, 28), (2, 33)])
        self.assertEqual(weak, [])

    def test_stops_at_the_next_section(self) -> None:
        hard, _ = cgf.glibc_needs(NEXT_SECTION)
        self.assertEqual(hard, [(2, 30)])

    def test_separates_weak_from_hard(self) -> None:
        hard, weak = cgf.glibc_needs(WEAK)
        self.assertEqual(max(hard), (2, 38))
        self.assertEqual(weak, [(2, 39)])

    def test_versions_compare_numerically(self) -> None:
        self.assertLess(cgf.parse_version("2.9"), cgf.parse_version("2.38"))
        self.assertLess(cgf.parse_version("2.2.5"), cgf.parse_version("2.3"))

    def test_rejects_a_malformed_floor(self) -> None:
        with self.assertRaises(ValueError):
            cgf.parse_version("2.x")

    def test_no_needs_table_means_no_requirement(self) -> None:
        self.assertEqual(cgf.glibc_needs("There are no version sections."), ([], []))


class CheckTests(unittest.TestCase):
    def test_within_the_floor_passes(self) -> None:
        ok, detail = cgf.check(REAL, (2, 35))
        self.assertTrue(ok)
        self.assertIn("GLIBC_2.33", detail)

    def test_exactly_at_the_floor_passes(self) -> None:
        self.assertTrue(cgf.check(REAL, (2, 33))[0])

    def test_above_the_floor_fails(self) -> None:
        ok, detail = cgf.check(REAL, (2, 31))
        self.assertFalse(ok)
        self.assertIn("highest hard requirement GLIBC_2.33", detail)
        self.assertIn("floor GLIBC_2.31", detail)

    def test_the_issue_binary_fails_a_2_35_floor(self) -> None:
        ok, detail = cgf.check(WEAK, (2, 35))
        self.assertFalse(ok)
        self.assertIn("GLIBC_2.38", detail)

    def test_a_weak_requirement_above_the_floor_is_reported_not_fatal(self) -> None:
        ok, detail = cgf.check(WEAK, (2, 38))
        self.assertTrue(ok)
        self.assertIn("highest weak requirement GLIBC_2.39", detail)

    def test_no_glibc_requirement_fails_closed(self) -> None:
        # Output this does not recognise must not read as "within the floor".
        for output in ("", "There are no version sections.", "Version needs section 'x'\n"):
            ok, detail = cgf.check(output, (2, 35))
            self.assertFalse(ok, output)
            self.assertIn("no GLIBC_ requirement found", detail)

    def test_a_blank_line_ends_the_table(self) -> None:
        output = NEXT_SECTION.replace("Version definition", "\n  0x0030:   Name: GLIBC_2.31  Flags: none  Version: 1\nVersion definition")
        self.assertEqual(cgf.glibc_needs(output)[0], [(2, 30)])


class RealReadelfTests(unittest.TestCase):
    @unittest.skipUnless(shutil.which("readelf"), "needs readelf")
    def test_parses_the_real_tool_output_for_a_real_binary(self) -> None:
        # Only a glibc-linked ELF has the table; skip where the interpreter is not.
        try:
            output = cgf.readelf_versions(sys.executable)
        except cgf.ReadelfError:
            self.skipTest("interpreter is not an ELF binary")
        hard, _ = cgf.glibc_needs(output)
        if not hard:
            self.skipTest("interpreter is not linked against glibc")
        self.assertTrue(cgf.check(output, (99, 0))[0])
        self.assertFalse(cgf.check(output, (1, 0))[0])

    def test_a_missing_readelf_or_binary_is_exit_status_2(self) -> None:
        self.assertEqual(cgf.main(["--floor", "2.35", "/nonexistent/omni-dev"]), 2)


class DocsTests(unittest.TestCase):
    """The floor is stated in four places; they must not drift apart (#2178)."""

    ROOT = pathlib.Path(__file__).resolve().parent.parent

    def read(self, rel: str) -> str:
        return (self.ROOT / rel).read_text(encoding="utf-8")

    def test_the_documented_floor_matches_glibc_floor(self) -> None:
        match = re.search(r"GLIBC_FLOOR:\s*'([\d.]+)'", self.read(".github/workflows/release.yml"))
        self.assertIsNotNone(match, "release.yml has no GLIBC_FLOOR")
        floor = match.group(1)
        self.assertIn(f"glibc {floor} or newer", self.read("README.md"))
        self.assertIn(f"**glibc {floor}**", self.read("docs/RELEASE.md"))


if __name__ == "__main__":
    unittest.main()
