"""Recordings to cells, text and images, without a worker (pty_records_input_resize_and_cells).

fixtures/terminal is a real terminal from a lab worker (the run id replaced,
its raw recording base64-encoded):
tests/scenarios/pty_fixture.py at 100x30 was sent `hi`, Up, the bytes of Up,
resized to 60x20 and sent Ctrl-C. Its captures hold zellij's own screen at
each moment; the replay of the recording must reproduce it.
"""
from pathlib import Path
import base64
import json
import os
import shutil
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import screen



def _materialize():
    """The fixture as a handle directory. Its raw recording is stored in
    base64 (`*.b64`) so the repository holds no terminal escape bytes."""
    root = Path(_FIXTURES.name) / "terminal"
    shutil.copytree(Path(__file__).resolve().parent / "fixtures" / "terminal", root)
    for encoded in root.rglob("*.b64"):
        encoded.with_suffix("").write_bytes(base64.b64decode(encoded.read_text()))
        encoded.unlink()
    return root


_FIXTURES = tempfile.TemporaryDirectory()
FIXTURE = _materialize()


def fixture_dir(test):
    tmp = tempfile.TemporaryDirectory()
    test.addCleanup(tmp.cleanup)
    hdir = Path(tmp.name) / "0001"
    shutil.copytree(FIXTURE, hdir)
    return hdir


def capture(name):
    return json.loads((FIXTURE / "captures" / f"{name}.json").read_text())


class RecordingTests(unittest.TestCase):
    def setUp(self):
        self.recording = screen.Recording.load(FIXTURE / "recording")

    def test_timing_records_input_resize_and_exit(self):
        kinds = [e["kind"] for e in self.recording.events]
        self.assertEqual(kinds, ["O", "I", "O", "I", "O", "I", "O", "S", "O", "I", "O"])
        inputs = [e["bytes"] for e in self.recording.events if e["kind"] == "I"]
        self.assertEqual(inputs, [2, 3, 3, 1])  # hi, Up, the bytes of Up, Ctrl-C
        self.assertEqual(self.recording.info["TERM"], "xterm-256color")
        self.assertEqual(self.recording.exit_code(), 130)
        self.assertEqual([(s["cols"], s["rows"]) for s in self.recording.sizes()], [(100, 30), (60, 20)])
        times = [e["t"] for e in self.recording.events]
        self.assertEqual(times, sorted(times))

    def test_input_log_holds_the_exact_bytes(self):
        raw = (FIXTURE / "recording" / "input").read_bytes()
        body = raw[raw.index(b"\n") + 1:]
        self.assertTrue(body.startswith(b"hi\x1b[A\x1b[A\x03"), body[:20])

    def test_script_header_is_not_program_output(self):
        output = (FIXTURE / "recording" / "output").read_bytes()
        self.assertTrue(output.startswith(screen.SCRIPT_HEADER))
        self.assertTrue(self.recording.body.startswith(b"\x1b[H\x1b[2JSIZE 100x30"))
        self.assertEqual(self.recording.offset(self.recording.skip), 0)
        self.assertEqual(self.recording.offset(len(output)), self.recording.total)

    def test_replay_agrees_with_zellij_at_every_capture(self):
        for name in ("0001", "0002", "0003"):
            with self.subTest(capture=name):
                found = capture(name)
                replayed = self.recording.screen(self.recording.offset(found["output_bytes"]))
                self.assertEqual((replayed.columns, replayed.lines), (found["cols"], found["rows"]))
                self.assertEqual(screen.screen_text(replayed), screen.text(found["screen"].split("\n")))
        self.assertIn("KEYS 68 69\nKEYS 1b 5b 41\nKEYS 1b 5b 41",
                      screen.screen_text(self.recording.screen(self.recording.offset(capture("0002")["output_bytes"]))))
        self.assertTrue(screen.screen_text(self.recording.screen()).endswith("KEYS 03\nINTERRUPTED"))

    def test_a_resize_after_the_last_output_counts_only_at_the_end(self):
        timing = (FIXTURE / "recording" / "timing").read_text().replace(
            "H 0.000000 DURATION", "S 0.100000 SIGWINCH ROWS=10 COLS=40\nH 0.000000 DURATION")
        recording = screen.Recording(timing, (FIXTURE / "recording" / "output").read_bytes())
        end = recording.screen()
        self.assertEqual((end.columns, end.lines), (40, 10))
        self.assertEqual(recording.sizes()[-1]["cols"], 40)
        before = recording.screen(recording.total - 1)
        self.assertEqual((before.columns, before.lines), (60, 20))

    def test_a_pane_started_before_it_had_a_size_is_sized_by_its_first_resize(self):
        # zellij can start the pane's command before sizing its PTY: script
        # then records -1x-1, and the real size arrives as a resize.
        timing = (FIXTURE / "recording" / "timing").read_text()
        unsized = timing.replace("H 0.000000 COLUMNS 100\nH 0.000000 LINES 30\n",
                                 "H 0.000000 COLUMNS -1\nH 0.000000 LINES -1\n")
        output = (FIXTURE / "recording" / "output").read_bytes()
        late = unsized.replace("O 0.008405 119", "S 0.000100 SIGWINCH ROWS=30 COLS=100\nO 0.008405 119")
        recording = screen.Recording(late, output)
        self.assertEqual([(x["cols"], x["rows"]) for x in recording.sizes()], [(100, 30), (60, 20)])
        first = recording.screen(recording.offset(capture("0001")["output_bytes"]))
        self.assertEqual(screen.screen_text(first), screen.text(capture("0001")["screen"].split("\n")))
        with self.assertRaises(screen.RecordingError):
            screen.Recording(unsized, output).screen()

    def test_a_capture_right_after_a_resize_replays_at_the_new_size(self):
        resize = next(x for x in self.recording.sizes() if x["cols"] == 60)["offset"]
        self.assertEqual(self.recording.screen(resize).columns, 100)  # no capture size: before the resize
        at = self.recording.screen(resize, (60, 20))
        self.assertEqual((at.columns, at.lines), (60, 20))

    def test_a_recording_that_does_not_add_up_is_refused(self):
        timing = (FIXTURE / "recording" / "timing").read_text()
        output = (FIXTURE / "recording" / "output").read_bytes()
        with self.assertRaises(screen.RecordingError):
            screen.Recording(timing, output[:200])
        with self.assertRaises(screen.RecordingError):
            screen.Recording(timing.replace("H 0.000000 COLUMNS 100\n", ""), output)
        with self.assertRaises(screen.RecordingError):
            screen.Recording(timing + "X 0.1 3\n", output)


class CellTests(unittest.TestCase):
    def setUp(self):
        recording = screen.Recording.load(FIXTURE / "recording")
        self.grid = screen.cells(recording.screen(recording.offset(capture("0001")["output_bytes"])))

    def test_cells_keep_attributes_and_wide_characters(self):
        self.assertEqual((self.grid["cols"], self.grid["rows"]), (100, 30))
        colours = self.grid["lines"][1]
        self.assertEqual(colours[0], {"text": "RED", "bold": True, "fg": "red"})
        self.assertEqual(colours[2], {"text": "GREEN", "bg": "green"})
        self.assertEqual(colours[4], {"text": "ORANGE", "fg": "ff8700"})
        self.assertEqual(colours[-1]["text"], " wide:界 end")
        self.assertEqual(len(self.grid["lines"]), 30)
        self.assertEqual(self.grid["cursor"], {"x": 0, "y": 2, "hidden": False})

    def test_frame_redraws_the_same_screen(self):
        import pyte
        again = pyte.Screen(self.grid["cols"], self.grid["rows"])
        pyte.ByteStream(again).feed(screen.frame(self.grid).encode())
        self.assertEqual(screen.cells(again)["lines"], self.grid["lines"])
        header, event = screen.cast(self.grid).splitlines()
        self.assertEqual(json.loads(header), {"version": 2, "width": 100, "height": 30})
        self.assertEqual(json.loads(event)[:2], [0.0, "o"])


class RenderTests(unittest.TestCase):
    def test_a_handle_renders_every_capture(self):
        hdir = fixture_dir(self)
        made = screen.render_handle(hdir)
        self.assertEqual([m["capture"] for m in made], ["0001", "0002", "0003", "0004"])
        for record in made[:3]:
            self.assertTrue(record["agrees"], record["capture"])
            self.assertTrue(record["size_matches_pane"])
        # A closed terminal has no zellij screen: nothing to agree with, said so.
        self.assertIsNone(made[3]["agrees"])
        wide, narrow = made[0]["render"], made[2]["render"]
        for name in ("0001", "0003"):
            self.assertTrue((hdir / "captures" / f"{name}.png").read_bytes().startswith(b"\x89PNG"))
        self.assertGreater(wide["image"]["width"], narrow["image"]["width"])
        self.assertGreater(wide["image"]["height"], narrow["image"]["height"])
        self.assertEqual(wide["font_family"], screen.FONT_FAMILY)
        self.assertRegex(wide["font_sha256"], r"^[0-9a-f]{64}$")
        self.assertEqual(wide["missing_glyphs"], ["界"])  # DejaVu has no CJK
        self.assertAlmostEqual(wide["cell"]["height"], screen.FONT_SIZE * screen.LINE_HEIGHT)
        # Rendered once: a second pass finds nothing new.
        self.assertEqual(screen.render_handle(hdir), [])
        summary = screen.summary(hdir)
        self.assertEqual(summary["exit_code"], 130)
        self.assertEqual(summary["app"], {"exited": True, "exit_status": 130})
        self.assertEqual([c["agrees"] for c in summary["captures"]], [True, True, True, None])

    def test_rendering_without_the_pinned_font_fails_loudly(self):
        grid = screen.cells(screen.Recording.load(FIXTURE / "recording").screen())
        with tempfile.TemporaryDirectory() as tmp, mock.patch.dict(os.environ, {screen.FONT_ENV: tmp}):
            with self.assertRaises(screen.RenderError):
                screen.render(grid, Path(tmp) / "x.png")
        with mock.patch.dict(os.environ), tempfile.TemporaryDirectory() as tmp:
            os.environ.pop(screen.FONT_ENV, None)
            with self.assertRaises(screen.RenderError):
                screen.render(grid, Path(tmp) / "x.png")


if __name__ == "__main__":
    unittest.main()
