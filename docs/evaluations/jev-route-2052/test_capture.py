"""Capture failures must be observable and live runs must not inherit replay."""
from contextlib import redirect_stdout
import gzip
import io
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import capture
import gh_fixture


class CaptureTests(unittest.TestCase):
    def test_failed_cli_output_is_preserved_and_status_propagated(self):
        with tempfile.TemporaryDirectory() as directory:
            wt = Path(directory)
            binary = wt / "target/debug/omni-dev"
            binary.parent.mkdir(parents=True)
            binary.write_bytes(b"test binary")
            output = wt / "results"
            result = subprocess.CompletedProcess([], 7, b'{"partial":true}', b"fetch failed")
            with patch.dict(capture.os.environ, {"ROUTE_GH_REPLAY": "/stale/fixtures"}), \
                    patch.object(capture.subprocess, "run", return_value=result) as run, \
                    patch.object(capture.subprocess, "check_output", side_effect=["test-sha", "test-version"]), \
                    redirect_stdout(io.StringIO()):
                status = capture.capture(wt, wt / "succinctly", output)
            self.assertEqual(status, 7)
            self.assertNotIn("ROUTE_GH_REPLAY", run.call_args.kwargs["env"])
            self.assertEqual(gzip.decompress((output / "baseline.json.gz").read_bytes()), result.stdout)
            self.assertEqual((output / "baseline.stderr.txt").read_bytes(), result.stderr)
            self.assertEqual(json.loads((output / "run.json").read_text())["exit_code"], 7)

    def test_existing_output_directory_is_refused_before_api_call(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with patch.object(capture.subprocess, "run") as run:
                with self.assertRaises(FileExistsError):
                    capture.capture(root, root, root)
                run.assert_not_called()

    def test_fixture_refuses_graphql_mutation(self):
        with patch.object(gh_fixture.sys, "argv", ["gh_fixture.py", "api", "graphql", "-f",
                                                 "query=mutation{deleteIssue(input:{}){clientMutationId}}"]), \
                patch.object(gh_fixture.subprocess, "run") as run:
            with self.assertRaises(SystemExit):
                gh_fixture.main()
            run.assert_not_called()


if __name__ == "__main__":
    unittest.main()
