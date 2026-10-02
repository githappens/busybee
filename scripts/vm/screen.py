"""Terminal recordings on the host: screen cells, text and images.

See docs/design/agent-lab.md §Make UI behavior observable. The guest records
each terminal with util-linux `script` (tests/scenarios/terminal.py): its
advanced timing log lists output and input chunks, every window-size change
and the exit code. Here a recording is replayed through pyte, at the recorded
sizes, into a cell grid: the screen's text and attributes at any point of the
output. Each capture's grid is compared with zellij's own screen of the same
moment and rendered to PNG by agg, with the font pinned in the development
shell (BUSYBEE_TERMINAL_FONTS). An image is always derived from a recording,
never drawn from a fixture, so the public export re-renders it from the
redacted copy.

A handle directory holds `state.json`, `recording/{output,input,timing}` and
`captures/NNNN.json` from the guest. `render_handle` adds, per capture,
`NNNN.cells.json` (CELLS_SCHEMA) and `NNNN.png`.
"""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

import pyte
from pyte import graphics

CELLS_SCHEMA = "busybee.terminal.cells/v1"
SCRIPT_HEADER = b"Script started on "
FONT_ENV = "BUSYBEE_TERMINAL_FONTS"
FONT_FAMILY = "DejaVu Sans Mono"
FONT_FILE = "DejaVuSansMono.ttf"
FONT_SIZE = 16
LINE_HEIGHT = 1.4
THEME = "asciinema"
RENDER_S = 60
ATTRS = ("bold", "italics", "underscore", "strikethrough", "reverse", "blink")
SGR_ATTRS = {"bold": 1, "italics": 3, "underscore": 4, "blink": 5, "reverse": 7, "strikethrough": 9}
FG = {name: code for code, name in {**graphics.FG_ANSI, **graphics.FG_AIXTERM}.items()}
BG = {name: code for code, name in {**graphics.BG_ANSI, **graphics.BG_AIXTERM}.items()}


class RecordingError(ValueError):
    """A recording this module cannot replay faithfully."""


class RenderError(RuntimeError):
    """An image could not be rendered; never replaced by a placeholder."""


# The recording

def parse_timing(text):
    """`script`'s advanced timing log: (events, info). Events carry their time
    since the start; info holds the header and closing fields (TERM, COLUMNS,
    LINES, EXIT_CODE ...)."""
    events, info, t = [], {}, 0.0
    for number, line in enumerate(text.splitlines(), 1):
        if not line.strip():
            continue
        kind, _, rest = line.partition(" ")
        delta, _, rest = rest.partition(" ")
        try:
            t += float(delta)
            if kind in ("I", "O"):
                events.append({"kind": kind, "t": round(t, 6), "bytes": int(rest)})
            elif kind == "S":
                name, _, args = rest.partition(" ")
                fields = dict(a.split("=", 1) for a in args.split() if "=" in a)
                events.append({"kind": "S", "t": round(t, 6), "signal": name,
                               **{k.lower(): int(v) for k, v in fields.items() if v.isdigit()}})
            elif kind == "H":
                name, _, value = rest.partition(" ")
                info[name] = value
            else:
                raise ValueError(f"unknown entry {kind!r}")
        except ValueError as err:
            raise RecordingError(f"timing line {number}: {err}") from err
    return events, info


class Recording:
    """One terminal's recording: the timing log and the output it describes."""

    def __init__(self, timing_text, output):
        self.events, self.info = parse_timing(timing_text)
        # The output log opens with script's own header line, which no
        # program wrote and the timing does not count.
        self.skip = output.index(b"\n") + 1 if output.startswith(SCRIPT_HEADER) else 0
        self.body = output[self.skip:]
        self.total = sum(e["bytes"] for e in self.events if e["kind"] == "O")
        if self.total > len(self.body):
            raise RecordingError(f"the timing counts {self.total} output bytes; the log holds {len(self.body)}")
        try:
            start = (int(self.info["COLUMNS"]), int(self.info["LINES"]))
        except (KeyError, ValueError) as err:
            raise RecordingError("the timing log does not record the starting size") from err
        # zellij can start the pane's command before sizing its PTY: script
        # records -1x-1 and the real size arrives as the first resize.
        self.start = start if min(start) > 0 else None

    @classmethod
    def load(cls, rec_dir):
        rec_dir = Path(rec_dir)
        return cls((rec_dir / "timing").read_text(errors="replace"), (rec_dir / "output").read_bytes())

    def exit_code(self):
        code = self.info.get("EXIT_CODE")
        return int(code) if code is not None and code.lstrip("-").isdigit() else None

    def sizes(self):
        """Every size the terminal had, with the output offset it took effect at."""
        out, offset = [{"offset": 0, "t": 0.0, "cols": self.start[0], "rows": self.start[1]}] if self.start else [], 0
        for e in self.events:
            if e["kind"] == "O":
                offset += e["bytes"]
            elif e["kind"] == "S" and e["signal"] == "SIGWINCH" and e.get("cols", 0) > 0 and e.get("rows", 0) > 0:
                out.append({"offset": offset, "t": e["t"], "cols": e["cols"], "rows": e["rows"]})
        return out

    def offset(self, file_bytes):
        """A capture's size of the output file, as an offset into the program's output."""
        return max(0, min(self.total, file_bytes - self.skip))

    def screen(self, upto=None, size=None):
        """The pyte screen after the first `upto` bytes of output (all by
        default). `size` is the (cols, rows) a capture at `upto` saw: a resize
        right at that point applies when it was already in effect."""
        upto = self.total if upto is None else upto
        screen = pyte.Screen(*self.start) if self.start else None
        stream = pyte.ByteStream(screen) if screen else None
        fed = 0
        for e in self.events:
            if e["kind"] == "O":
                if fed >= upto:
                    break
                if screen is None:
                    raise RecordingError("output came before the terminal had a size")
                take = min(e["bytes"], upto - fed)
                stream.feed(self.body[fed:fed + take])
                fed += e["bytes"]
            elif e["kind"] == "S" and e["signal"] == "SIGWINCH" and e.get("cols", 0) > 0 and e.get("rows", 0) > 0:
                # A resize at the capture point counts only for the whole
                # recording: with output after it, it came after the capture.
                if screen is None:
                    screen = pyte.Screen(e["cols"], e["rows"])
                    stream = pyte.ByteStream(screen)
                elif fed < upto or upto == self.total or size == (e["cols"], e["rows"]):
                    screen.resize(lines=e["rows"], columns=e["cols"])
        if screen is None:
            raise RecordingError("the terminal never had a size")
        return screen


# Cells, text and the frame that draws them

def text(lines):
    """Screen lines as text: trailing blanks off each line, trailing empty lines dropped."""
    out = [line.rstrip() for line in lines]
    while out and not out[-1]:
        out.pop()
    return "\n".join(out)


def screen_text(screen):
    return text(screen.display)


def _attrs(char):
    found = {k: True for k in ATTRS if getattr(char, k)}
    if char.fg != "default":
        found["fg"] = char.fg
    if char.bg != "default":
        found["bg"] = char.bg
    return found


def cells(screen):
    """The grid as runs of characters sharing attributes, plus the cursor.
    Wide characters occupy two columns; their second cell is not repeated."""
    lines = []
    for y in range(screen.lines):
        runs, row = [], screen.buffer[y]
        for x in range(screen.columns):
            char = row[x]
            if char.data == "":
                continue  # the right half of a wide character
            attrs = _attrs(char)
            if runs and {k: v for k, v in runs[-1].items() if k != "text"} == attrs:
                runs[-1]["text"] += char.data
            else:
                runs.append({"text": char.data, **attrs})
        if runs and set(runs[-1]) == {"text"}:
            runs[-1]["text"] = runs[-1]["text"].rstrip()
            if not runs[-1]["text"]:
                runs.pop()
        lines.append(runs)
    return {"cols": screen.columns, "rows": screen.lines,
            "cursor": {"x": screen.cursor.x, "y": screen.cursor.y, "hidden": screen.cursor.hidden},
            "lines": lines}


def _color(value, names, base):
    if value in names:
        return [names[value]]
    try:
        r, g, b = bytes.fromhex(value)
    except ValueError:
        raise RenderError(f"unknown colour {value!r}") from None
    return [base, 2, r, g, b]


def sgr(attrs):
    codes = [0] + [SGR_ATTRS[k] for k in ATTRS if attrs.get(k)]
    if "fg" in attrs:
        codes += _color(attrs["fg"], FG, 38)
    if "bg" in attrs:
        codes += _color(attrs["bg"], BG, 48)
    return "\x1b[" + ";".join(map(str, codes)) + "m"


def frame(grid):
    """ANSI text that draws `grid` (from `cells`) on a cleared screen of its size."""
    out = ["\x1b[?25l\x1b[H\x1b[2J"]
    for y, runs in enumerate(grid["lines"]):
        out.append(f"\x1b[{y + 1};1H")
        for run in runs:
            out.append(sgr(run) + run["text"])
        out.append("\x1b[0m")
    cursor = grid["cursor"]
    out.append(f"\x1b[{cursor['y'] + 1};{cursor['x'] + 1}H")
    if not cursor["hidden"]:
        out.append("\x1b[?25h")
    return "".join(out)


def cast(grid):
    """A one-frame asciicast v2 of the grid."""
    header = {"version": 2, "width": grid["cols"], "height": grid["rows"]}
    return json.dumps(header) + "\n" + json.dumps([0.0, "o", frame(grid)]) + "\n"


# Images

def _sha256(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def fonts():
    """The pinned font directory and the face terminals are rendered with."""
    root = os.environ.get(FONT_ENV)
    if not root:
        raise RenderError(f"{FONT_ENV} is not set; run inside the project development shell")
    face = Path(root) / FONT_FILE
    if not face.is_file():
        raise RenderError(f"{FONT_FILE} is not in {FONT_ENV}")
    return Path(root), face


def coverage(grid, face):
    """Characters on the screen the pinned font has no glyph for."""
    from fontTools.ttLib import TTFont
    cmap = TTFont(face).getBestCmap()
    used = {ch for runs in grid["lines"] for run in runs for ch in run["text"] if ord(ch) > 0x7e}
    return sorted(ch for ch in used if ord(ch) not in cmap)


def cell_size(face):
    from fontTools.ttLib import TTFont
    font = TTFont(face)
    advance = font["hmtx"]["M"][0] / font["head"].unitsPerEm * FONT_SIZE
    return {"width": round(advance, 2), "height": round(FONT_SIZE * LINE_HEIGHT, 2)}


def render(grid, png):
    """Render `grid` to `png` with agg; returns the rendering metadata."""
    font_dir, face = fonts()
    agg = shutil.which("agg")
    if agg is None:
        raise RenderError("agg is not on PATH; run inside the project development shell")
    from PIL import Image
    with tempfile.TemporaryDirectory() as tmp:
        source, gif = Path(tmp) / "frame.cast", Path(tmp) / "frame.gif"
        source.write_text(cast(grid))
        argv = [agg, "--quiet", "--font-dir", str(font_dir), "--font-family", FONT_FAMILY, "--font-size",
                str(FONT_SIZE), "--line-height", str(LINE_HEIGHT), "--theme", THEME, "--last-frame-duration", "0",
                str(source), str(gif)]
        done = subprocess.run(argv, capture_output=True, text=True, timeout=RENDER_S)
        if done.returncode != 0 or not gif.is_file():
            raise RenderError(f"agg exited {done.returncode}: {done.stderr.strip()[-500:]}")
        with Image.open(gif) as image:
            image.seek(image.n_frames - 1)
            png.parent.mkdir(parents=True, exist_ok=True)
            image.convert("RGB").save(png, "PNG")
            size = image.size
    version = subprocess.run([agg, "--version"], capture_output=True, text=True, timeout=RENDER_S).stdout.strip()
    return {"renderer": version, "font_family": FONT_FAMILY, "font_file": FONT_FILE, "font_sha256": _sha256(face),
            "font_size": FONT_SIZE, "line_height": LINE_HEIGHT, "theme": THEME, "cell": cell_size(face),
            "image": {"width": size[0], "height": size[1]}, "missing_glyphs": coverage(grid, face)}


# A handle

def render_capture(recording, capture, png):
    """The pyte view of one capture: its cells, text, whether they agree with
    zellij's screen of that moment, and the image rendered from them."""
    upto = recording.offset(capture["output_bytes"])
    screen = recording.screen(upto, (capture.get("cols"), capture.get("rows")))
    grid, seen = cells(screen), screen_text(screen)
    zellij = text(capture["screen"].split("\n")) if capture.get("screen") is not None else None
    return {"schema": CELLS_SCHEMA, "capture": capture["capture"], "output_offset": upto,
            "size": {"cols": grid["cols"], "rows": grid["rows"]},
            "size_matches_pane": (grid["cols"], grid["rows"]) == (capture.get("cols"), capture.get("rows")),
            "text": seen, "zellij_text": zellij, "agrees": None if zellij is None else zellij == seen,
            "settled": capture.get("settled"), "cells": grid, "render": render(grid, png), "image": png.name}


def render_handle(hdir):
    """Render every capture of the handle in `hdir` that has no cells yet;
    returns the new cell records."""
    hdir = Path(hdir)
    recording = Recording.load(hdir / "recording")
    made = []
    for path in sorted((hdir / "captures").glob("[0-9][0-9][0-9][0-9].json")):
        out = path.with_name(f"{path.stem}.cells.json")
        if out.is_file():
            continue
        record = render_capture(recording, json.loads(path.read_text()), path.with_suffix(".png"))
        out.write_text(json.dumps(record, indent=2, ensure_ascii=False) + "\n")
        made.append(record)
    return made


def summary(hdir):
    """What a handle directory holds, for the evidence manifest."""
    hdir = Path(hdir)
    state = json.loads((hdir / "state.json").read_text()) if (hdir / "state.json").is_file() else {}
    try:
        recording = Recording.load(hdir / "recording")
        sizes, exit_code, error = recording.sizes(), recording.exit_code(), None
    except (OSError, RecordingError) as err:
        sizes, exit_code, error = [], None, str(err) if not isinstance(err, OSError) else err.strerror
    captures = []
    for path in sorted((hdir / "captures").glob("[0-9][0-9][0-9][0-9].json")):
        cells_path = path.with_name(f"{path.stem}.cells.json")
        rendered = json.loads(cells_path.read_text()) if cells_path.is_file() else None
        captures.append({"capture": path.stem, "label": json.loads(path.read_text()).get("label"),
                         "agrees": rendered["agrees"] if rendered else None,
                         "image": path.with_suffix(".png").name if rendered else None})
    return {"status": state.get("status"), "argv": state.get("argv"), "term": state.get("term"),
            "lang": state.get("lang"), "zellij": state.get("zellij"), "app": state.get("app"),
            "sizes": sizes, "exit_code": exit_code, "recording_error": error, "captures": captures}


def handles(rdir):
    """Every terminal of a run (`vmctl terminal` handles and scenarios'), summarised."""
    root = Path(rdir) / "terminal"
    return {h.name: summary(h) for h in sorted(root.iterdir()) if h.is_dir()} if root.is_dir() else {}
