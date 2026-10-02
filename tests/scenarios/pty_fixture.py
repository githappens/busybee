#!/usr/bin/env python3
"""A deterministic terminal program for the terminal tests (pty_records_input_resize_and_cells).

In raw mode it draws its size, TERM and LANG with a line of colours and a wide
character, redraws on every window-size change, and reports each read of its
input as hex. `f` floods the terminal with numbered lines first, so quitting
must drain them; `q` prints BYE and exits 0; Ctrl-C (0x03, which raw mode
delivers as a byte) prints INTERRUPTED and exits 130.
"""
import os
import signal
import sys
import termios
import tty

FLOOD_LINES = 20000


def write(text):
    os.write(1, text.encode())


def draw(*_):
    cols, rows = os.get_terminal_size(0)
    write(f"\x1b[H\x1b[2JSIZE {cols}x{rows} TERM={os.environ.get('TERM')} LANG={os.environ.get('LANG')}\r\n"
          "\x1b[1;31mRED\x1b[0m \x1b[42mGREEN\x1b[0m \x1b[38;5;208mORANGE\x1b[0m wide:界 end\r\n")


def main():
    saved = termios.tcgetattr(0)
    tty.setraw(0)
    signal.signal(signal.SIGWINCH, draw)
    try:
        draw()
        while True:
            data = os.read(0, 64)
            write("KEYS " + data.hex(" ") + "\r\n")
            if data == b"q":
                write("BYE\r\n")
                return 0
            if data == b"\x03":
                write("INTERRUPTED\r\n")
                return 130
            if data == b"f":
                write("".join(f"flood {n:05d}\r\n" for n in range(FLOOD_LINES)))
    finally:
        termios.tcsetattr(0, termios.TCSADRAIN, saved)


if __name__ == "__main__":
    sys.exit(main())
