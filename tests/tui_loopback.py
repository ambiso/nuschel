#!/usr/bin/env python3
"""The terminal client (apps/tui) against a loopback server, through
pseudo-terminals: Ada uses the full screen (a small VT100 emulator reads
what it draws), Andi the plain lines a screen reader reads. They connect,
trust the certificate on first use, join a channel, send and receive; on a
Nuschel server they also register, edit, see each other typing, delete,
search and write directly. Voice is off (--no-audio), as over SSH.

    cargo build -p nuschel-tui -p nuscheld -p nuschel-cli -p nuschel-content-worker \\
        -p nuschel-storage-worker -p nuschel-crypto-worker
    python3 tests/with_local_nuscheld.py <binaries> tests/tui_loopback.py <binaries> --port PORT
    python3 tests/with_local_murmur.py <mumble-server> tests/tui_loopback.py <binaries> --port PORT --stock

<binaries> is the cargo target folder (target/debug): the workers are found
beside nuschel-tui there. Only loopback, temporary profiles and temporary
HOME and XDG folders are used.
"""

import argparse
import codecs
import fcntl
import os
import pty
import re
import select
import signal
import struct
import sys
import tempfile
import termios
import time
import unicodedata
from pathlib import Path

TIMEOUT = 30


class Screen:
    """The part of a VT100 that crossterm and ratatui use: cursor moves,
    erasing, and text; colours and modes are ignored."""

    def __init__(self, rows, cols):
        self.rows, self.cols = rows, cols
        self.grid = [[" "] * cols for _ in range(rows)]
        self.y = self.x = 0
        self.pending = ""

    def put(self, char):
        width = 2 if unicodedata.east_asian_width(char) in "WF" else 1
        if unicodedata.combining(char):
            return
        if self.x + width > self.cols:
            self.x = 0
            self.y = min(self.y + 1, self.rows - 1)
        self.grid[self.y][self.x] = char
        if width == 2 and self.x + 1 < self.cols:
            self.grid[self.y][self.x + 1] = ""
        self.x += width

    def erase(self, y, start, end):
        for x in range(max(start, 0), min(end, self.cols)):
            self.grid[y][x] = " "

    def csi(self, params, final):
        private = params.startswith("?")
        numbers = [int(part) if part.isdigit() else 0 for part in params.lstrip("?").split(";")] if params.lstrip("?") else []
        first = numbers[0] if numbers else 0
        if private:
            return
        if final in "Hf":
            row = (numbers[0] if numbers and numbers[0] else 1) - 1
            col = (numbers[1] if len(numbers) > 1 and numbers[1] else 1) - 1
            self.y, self.x = min(row, self.rows - 1), min(col, self.cols - 1)
        elif final == "J":
            if first == 0:
                self.erase(self.y, self.x, self.cols)
                for y in range(self.y + 1, self.rows):
                    self.erase(y, 0, self.cols)
            elif first == 1:
                for y in range(0, self.y):
                    self.erase(y, 0, self.cols)
                self.erase(self.y, 0, self.x + 1)
            else:
                for y in range(self.rows):
                    self.erase(y, 0, self.cols)
        elif final == "K":
            if first == 0:
                self.erase(self.y, self.x, self.cols)
            elif first == 1:
                self.erase(self.y, 0, self.x + 1)
            else:
                self.erase(self.y, 0, self.cols)
        elif final == "X":
            self.erase(self.y, self.x, self.x + max(first, 1))
        elif final == "A":
            self.y = max(self.y - max(first, 1), 0)
        elif final == "B":
            self.y = min(self.y + max(first, 1), self.rows - 1)
        elif final == "C":
            self.x = min(self.x + max(first, 1), self.cols - 1)
        elif final == "D":
            self.x = max(self.x - max(first, 1), 0)
        elif final == "G":
            self.x = min(max(first, 1) - 1, self.cols - 1)
        elif final == "d":
            self.y = min(max(first, 1) - 1, self.rows - 1)

    def feed(self, text):
        text = self.pending + text
        self.pending = ""
        index = 0
        while index < len(text):
            char = text[index]
            if char == "\x1b":
                rest = text[index + 1:]
                if not rest:
                    self.pending = text[index:]
                    return
                if rest[0] == "[":
                    match = re.match(r"\[([0-9;?<>=]*)[ -/]*([@-~])", rest)
                    if not match:
                        self.pending = text[index:]
                        return
                    self.csi(match.group(1), match.group(2))
                    index += 1 + match.end()
                    continue
                if rest[0] == "]":
                    end = re.search(r"\x07|\x1b\\", rest)
                    if not end:
                        self.pending = text[index:]
                        return
                    index += 1 + end.end()
                    continue
                if rest[0] in "()":
                    index += 3
                    continue
                index += 2
                continue
            if char == "\r":
                self.x = 0
            elif char == "\n":
                self.y = min(self.y + 1, self.rows - 1)
            elif char == "\b":
                self.x = max(self.x - 1, 0)
            elif char >= " ":
                self.put(char)
            index += 1

    def text(self):
        return "\n".join("".join(row) for row in self.grid)


class Terminal:
    """A program on a pseudo-terminal of `rows` × `cols`."""

    def __init__(self, name, argv, env, rows=40, cols=120, plain=False):
        self.name, self.plain = name, plain
        self.screen = Screen(rows, cols)
        self.output = ""
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
            os.execve(argv[0], argv, env)
        self.status = None

    def pump(self, timeout=0.1):
        ready, _, _ = select.select([self.fd], [], [], timeout)
        if not ready:
            return False
        try:
            data = os.read(self.fd, 65536)
        except OSError:
            data = b""
        if not data:
            self.reap()
            return False
        text = self.decoder.decode(data)
        self.output += text
        self.screen.feed(text)
        return True

    def reap(self):
        if self.status is None:
            pid, status = os.waitpid(self.pid, os.WNOHANG)
            if pid:
                self.status = os.waitstatus_to_exitcode(status)
        return self.status

    def view(self):
        """What a reader sees: the screen, or the plain lines so far."""
        return self.output if self.plain else self.screen.text()

    def wait_for(self, what, timeout=TIMEOUT):
        """Until `what` (a string or a predicate on the view) holds."""
        check = (lambda view: what in view) if isinstance(what, str) else what
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if check(self.view()):
                return
            self.pump()
            if self.reap() is not None:
                break
        raise AssertionError(f"{self.name}: timed out waiting for {what!r}; it shows:\n{self.view()[-6000:]}")

    def send(self, data, pause=0.15):
        os.write(self.fd, data.encode() if isinstance(data, str) else data)
        # Give the program a moment to draw, then read what it drew.
        deadline = time.monotonic() + pause
        while time.monotonic() < deadline:
            self.pump(0.05)

    def line(self, text):
        self.send(text + "\r")

    def close(self, keys):
        if self.reap() is None:
            try:
                self.send(keys, pause=0.3)
            except OSError:
                pass
        deadline = time.monotonic() + 10
        while self.reap() is None and time.monotonic() < deadline:
            self.pump()
        if self.reap() is None:
            os.kill(self.pid, signal.SIGKILL)
            os.waitpid(self.pid, 0)
            raise AssertionError(f"{self.name} did not quit")
        return self.status


CTRL_K, CTRL_F, CTRL_Q, CTRL_U = "\x0b", "\x06", "\x11", "\x15"
ENTER, ALT_ENTER, ESC, UP, TAB = "\r", "\x1b\r", "\x1b", "\x1b[A", "\t"


def environment(root):
    env = {
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
        "TERM": "xterm-256color",
        "LANG": "C.UTF-8",
        "TZ": "UTC",
        "HOME": str(root / "home"),
        "XDG_CONFIG_HOME": str(root / "config"),
        "XDG_DATA_HOME": str(root / "data"),
        "XDG_STATE_HOME": str(root / "state"),
        "XDG_CACHE_HOME": str(root / "cache"),
        "XDG_RUNTIME_DIR": str(root / "run"),
    }
    for name in ("HOME", "XDG_RUNTIME_DIR"):
        Path(env[name]).mkdir(mode=0o700, parents=True, exist_ok=True)
    return env


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("binaries", type=Path)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=64738)
    parser.add_argument("--stock", action="store_true", help="a stock Mumble server: no edits, typing or history")
    parser.add_argument("--screens", type=Path, help="save Ada's screen as text after each step here")
    args = parser.parse_args()
    global SCREENS
    SCREENS = args.screens
    tui = str((args.binaries / "nuschel-tui").resolve())
    url = f"{args.host}:{args.port}"
    with tempfile.TemporaryDirectory(prefix="nuschel-tui-loopback-") as temporary:
        root = Path(temporary)
        env = environment(root)
        common = ["--no-audio", "--opener", "/bin/true"]
        andi = Terminal("Andi", [tui, "--plain", *common, "--profile-dir", str(root / "andi"),
                                 f"mumble://Andi@{url}/Lobby"], env, plain=True)
        ada = Terminal("Ada", [tui, *common, "--profile-dir", str(root / "ada"), f"mumble://Ada@{url}"], env)
        try:
            scenario(ada, andi, args.stock)
        except BaseException:
            print("--- Andi's lines ---\n" + andi.output[-4000:], file=sys.stderr)
            print("--- Ada's screen ---\n" + ada.screen.text(), file=sys.stderr)
            raise
        finally:
            statuses = (ada.close(CTRL_Q), andi.close("/quit\r"))
        assert statuses == (0, 0), statuses
    print("The terminal client over loopback: ok" + (" (stock server)" if args.stock else ""))


SCREENS = None
STEPS = []


def step(what, ada=None):
    print(f"  {time.strftime('%H:%M:%S')} {what}", flush=True)
    if SCREENS and ada:
        SCREENS.mkdir(parents=True, exist_ok=True)
        STEPS.append(what)
        (SCREENS / f"{len(STEPS):02}.txt").write_text(f"{what}\n{ada.screen.text()}\n")


def scenario(ada, andi, stock):
    # Trust on first use: Andi answers in a line, Ada in the dialog.
    andi.wait_for("Trust it with /trust")
    andi.line("/trust")
    andi.wait_for(re.compile(r"Connected").search)
    ada.wait_for("First connection to")
    ada.send("y")
    ada.wait_for(lambda view: re.search(r": Connected", view))
    step("both connected after trusting the certificate", ada)
    if not stock:
        andi.line("/register")
        andi.wait_for("You are registered on")
        ada.line("/register")
        ada.wait_for("You are registered on")
        step("both registered", ada)
    # Andi's link named the channel; Ada joins through the switcher.
    andi.wait_for("Your voice is in Lobby")
    ada.send(CTRL_K)
    ada.wait_for("Find:")
    ada.send("Lobby")
    ada.wait_for("Lobby, channel")
    step("Ada finds Lobby with Ctrl+K", ada)
    ada.send(ALT_ENTER)
    ada.wait_for("Lobby on 127.0.0.1, your voice channel")
    ada.wait_for("Andi (deafened)")
    step("both in Lobby: Ada sees Andi in the tree", ada)

    andi.line("Hello from plain lines")
    ada.wait_for("Andi: Hello from plain lines")
    ada.line("Hello from the full screen")
    andi.wait_for("Ada: Hello from the full screen")
    step("sent and received both ways", ada)
    if stock:
        return

    # Typing shows on the other side (typing/1).
    ada.send("still typ")
    andi.wait_for("* Ada is typing…")
    ada.send(CTRL_U)
    step("Andi saw Ada typing", ada)
    # Ada edits her message: Up in the empty composer.
    ada.send(UP)
    ada.wait_for("Editing your message")
    ada.send(CTRL_U + "Edited in the full screen" + ENTER)
    andi.wait_for("Ada (edited): Edited in the full screen")
    ada.wait_for("Ada (edited): Edited in the full screen")
    andi.line("/edit Hello again, edited")
    ada.wait_for("Andi (edited): Hello again, edited")
    step("both edited, and each saw the other's edit", ada)
    # Ada deletes hers after the question.
    ada.line("/delete")
    ada.wait_for("Delete this message?")
    ada.send("y")
    andi.wait_for("Ada: (deleted)")
    step("Ada deleted her message; Andi saw the tombstone", ada)
    # Search in local history, through the dialog.
    ada.send(CTRL_F)
    ada.wait_for("Search:")
    ada.send("again")
    ada.wait_for("Andi in #Lobby: Hello again, edited")
    step("Ada found Andi's message by search", ada)
    ada.send(ESC)
    # A direct conversation: Andi sees it unread, then reads it.
    ada.line("/dm Andi")
    ada.wait_for("@ Andi on 127.0.0.1")
    ada.line("Just between us")
    andi.wait_for(lambda view: re.search(r"unread in @ Ada", view))
    andi.line("/go Ada")
    andi.wait_for("Ada: Just between us")
    andi.line("Got it")
    ada.wait_for("Andi: Got it")
    step("a direct conversation, unread until opened", ada)
    # Andi's message about Ada's in Lobby is unread for Ada.
    andi.line("/go Lobby")
    andi.wait_for("--- Lobby on 127.0.0.1 ---")
    andi.line("Back in the channel")
    ada.wait_for("Lobby (your voice) (1 unread)")
    step("Ada's tree counts the unread message in Lobby", ada)
    # The help, from the channel pane.
    ada.send(TAB)
    ada.send("?")
    ada.wait_for("Help: Esc closes")
    step("the help lists the keys", ada)
    ada.send(ESC)


if __name__ == "__main__":
    main()
