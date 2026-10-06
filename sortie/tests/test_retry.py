"""Tests for transient GitHub API failure retry logic in reviews.command()."""
import contextlib
import io
import subprocess
import sys
from pathlib import Path
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import ci_reviews
import reviews


def _result(returncode, stdout="", stderr=""):
    return subprocess.CompletedProcess([], returncode, stdout, stderr)


class RetryTests(unittest.TestCase):
    def test_transient_gh_failure_is_retried_then_succeeds(self):
        """Transient 5xx fails twice then succeeds; two retries are logged."""
        # Referencing is_transient_error ensures this test fails on the base
        # commit where the retry mechanism does not exist.
        self.assertTrue(reviews.is_transient_error("HTTP 503 Service Unavailable"))

        transient = _result(1, stderr="HTTP 503 Service Unavailable")
        success = _result(0, stdout='{"ok":true}')

        with patch.object(reviews.subprocess, "run",
                          side_effect=[transient, transient, success]), \
             patch.object(reviews.time, "sleep") as mock_sleep, \
             contextlib.redirect_stderr(io.StringIO()) as err:
            result = reviews.command(["gh", "api", "repos/example/tool"])

        self.assertEqual(result, '{"ok":true}')
        self.assertEqual(mock_sleep.call_count, 2)
        self.assertEqual(err.getvalue().count("HTTP 503"), 2, "each retry is logged with its cause")

    def test_retries_exhausted_fail_loudly(self):
        """All transient failures exhausts retries and raises with the original error."""
        self.assertTrue(reviews.is_transient_error(
            "No server is currently available to service your request"))

        transient = _result(1, stderr="No server is currently available to service your request")

        with patch.object(reviews.subprocess, "run",
                          return_value=transient) as mock_run, \
             patch.object(reviews.time, "sleep"):
            with self.assertRaises(RuntimeError) as ctx:
                reviews.command(["gh", "api", "repos/example/tool"])

        self.assertEqual(mock_run.call_count, reviews.RETRY_ATTEMPTS)
        self.assertIn("No server is currently available", str(ctx.exception))

    def test_non_transient_error_is_not_retried(self):
        """A 404 fails immediately on the first attempt without any retry."""
        self.assertFalse(reviews.is_transient_error("HTTP 404: Not Found"))

        not_found = _result(1, stderr="HTTP 404: Not Found")

        with patch.object(reviews.subprocess, "run",
                          return_value=not_found) as mock_run, \
             patch.object(reviews.time, "sleep") as mock_sleep:
            with self.assertRaises(RuntimeError) as ctx:
                reviews.command(["gh", "api", "repos/example/tool"])

        mock_run.assert_called_once()
        mock_sleep.assert_not_called()
        self.assertIn("404", str(ctx.exception))

    def test_a_write_is_not_retried(self):
        """A transient failure on a POST (such as publishing a review) is not
        retried: the write may have landed, and a retry would skip the head
        recheck that precedes publishing."""
        transient = _result(1, stderr="HTTP 502 Bad Gateway")

        with patch.object(reviews.subprocess, "run",
                          return_value=transient) as mock_run, \
             patch.object(reviews.time, "sleep") as mock_sleep:
            with self.assertRaises(RuntimeError) as ctx:
                reviews.api("repos/example/tool/pulls/1/reviews", "POST", {"event": "APPROVE"})

        mock_run.assert_called_once()
        mock_sleep.assert_not_called()
        self.assertIn("502", str(ctx.exception))

    def test_artifact_download_retries_transient_failures(self):
        """The binary artifact download shares the retry, returning raw bytes."""
        transient = _result(1, stderr=b"HTTP 503 Service Unavailable")
        success = _result(0, stdout=b"PK\x03\x04")

        with patch.object(reviews.subprocess, "run",
                          side_effect=[transient, success]), \
             patch.object(reviews.time, "sleep") as mock_sleep, \
             contextlib.redirect_stderr(io.StringIO()):
            data = ci_reviews.download("example/tool", 7)

        self.assertEqual(data, b"PK\x03\x04")
        mock_sleep.assert_called_once()


if __name__ == "__main__":
    unittest.main()
