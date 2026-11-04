"""Python macro interface for psconnector-view. Standard library only.

When started from the psconnector-view bar the port comes in via PSCONNECTOR_PORT; otherwise 8650 is used.

    from psconnector import PS

    ps = PS()
    ps.osd('starting')
    ps.tap('cross')
    ps.stick('left', 0, -1); ps.sleep(0.5); ps.stick('left', 0, 0)
    w, h, rgb = ps.frame()            # RGB bytes (ps.frame_array() if numpy is installed)
    ps.wait(lambda: ps.pixel(640, 360)[0] > 200, timeout=5)

The ■ Stop button in psconnector-view sends SIGINT, which arrives as KeyboardInterrupt, so wrap cleanup
in try/finally. However the script ends, psconnector-view releases all buttons.

When the run has a time limit, check ps.should_stop() at a safe point (for example after each lap) and exit
cleanly. Macros that never check it are interrupted as soon as the limit is reached.
"""
import json
import os
import socket
import time

BUTTONS = ('up', 'down', 'left', 'right', 'l1', 'r1', 'l2', 'r2', 'cross', 'circle', 'square', 'triangle',
           'options', 'share', 'ps', 'l3', 'r3', 'touchpad')


class PSError(RuntimeError):
    """psconnector-view answered 'err ...'."""


class PS:
    def __init__(self, port=None, host='127.0.0.1', timeout=10):
        port = int(port or os.environ.get('PSCONNECTOR_PORT', 8650))
        self._sock = socket.create_connection((host, port), timeout=timeout)
        self._r = self._sock.makefile('rb')
        self._size = None

    # ------------------------------------------------------------ low level
    def call(self, **msg):
        """Sends one command and returns the reply line ('ok ...'). Raises PSError on failure."""
        self._sock.sendall((json.dumps(msg, ensure_ascii=False) + '\n').encode())
        line = self._r.readline().decode().strip()
        if not line.startswith('ok'):
            raise PSError(line or 'no reply (is psconnector-view running?)')
        return line

    # ------------------------------------------------------------ input
    def tap(self, button, hold=0.08):
        """Presses, releases after `hold` seconds, and returns once released."""
        self.call(button=button, action='tap', hold=hold)

    def press(self, button):
        self.call(button=button, action='press')

    def release(self, button):
        self.call(button=button, action='release')

    def button(self, name, action='tap', hold=0.08):
        """Same call as the older ps5view/pscmd client: action is 'tap', 'press' or 'release'."""
        self.call(button=name, action=action, hold=hold)

    def taps(self, *buttons, hold=0.08, gap=0.05):
        """Taps several buttons in order."""
        for b in buttons:
            self.tap(b, hold)
            time.sleep(gap)

    def stick(self, which, x, y):
        """which: 'left'/'right', x and y in -1.0..1.0 (up is negative). Use 0, 0 to center."""
        self.call(stick=which, x=float(x), y=float(y))

    def release_all(self):
        """Releases all buttons and centers both sticks."""
        self.call(release_all=True)

    # ------------------------------------------------------------ screen
    def frame(self):
        """Current frame as (width, height, RGB bytes). Row-major, 3 bytes per pixel."""
        self._sock.sendall(b'{"frame": true}\n')
        head = self._r.readline().decode().strip()
        if not head.startswith('ok'):
            raise PSError(head)
        _, w, h, n = head.split()
        data = self._r.read(int(n))
        return int(w), int(h), data

    def frame_array(self, bgr=False):
        """numpy array of shape (height, width, 3); bgr=True gives OpenCV's BGR order. Requires numpy."""
        import numpy as np
        w, h, data = self.frame()
        img = np.frombuffer(data, np.uint8).reshape(h, w, 3)
        return img[:, :, ::-1].copy() if bgr else img

    def pixel(self, x, y):
        """(R, G, B) of one pixel. For many pixels, fetch frame() once instead."""
        w, h, data = self.frame()
        i = (y * w + x) * 3
        return data[i], data[i + 1], data[i + 2]

    def ocr(self, region=None, scale=1.0, rel=None):
        """Reads text in part of the screen (macOS Vision, Korean + English). Returns a list of lines.

        region: [x, y, w, h] in frame pixels, or rel: (x0, y0, x1, y1) as fractions of the screen.
        scale enlarges the crop before reading; 1.5-3 helps with small text.
        """
        if rel is not None:
            x0, y0, x1, y1 = rel
            w, h = self.size()
            region = [int(w * x0), int(h * y0), int(w * (x1 - x0)), int(h * (y1 - y0))]
        line = self.call(ocr=list(region or []), scale=float(scale))
        return json.loads(line[2:].strip() or '[]')

    def size(self):
        """(width, height) of the stream, cached after the first call."""
        if self._size is None:
            w, h, _ = self.frame()
            self._size = (w, h)
        return self._size

    def snap(self, path):
        """Saves the current frame as PNG."""
        self.call(snap=str(path))

    # ------------------------------------------------------------ other
    def osd(self, text):
        """Shows text in the top-left of the psconnector-view window (an empty string clears it)."""
        self.call(osd=str(text))

    def play(self, path, wait=True):
        """Plays a recording (JSON). With wait=True, returns when playback ends."""
        self.call(play=str(path))
        while wait and self.playing():
            time.sleep(0.05)

    def stop(self):
        """Stops playback."""
        self.call(stop=True)

    def playing(self):
        return self.call(status=True) == 'ok playing'

    def standby(self):
        """Puts the PS5 into rest mode (this also disconnects)."""
        self.call(standby=True)

    def should_stop(self):
        """True once the run time limit set in psconnector-view is up. Check it at a safe point (end of a lap)
        and exit; you then get 2 minutes before being interrupted. False when there is no limit."""
        try:
            return self.call(should_stop=True) == 'ok yes'
        except PSError:            # older psconnector-view without time limits
            return False

    @staticmethod
    def sleep(seconds):
        time.sleep(seconds)

    @staticmethod
    def wait(cond, timeout=10.0, interval=0.1):
        """Waits until cond() is true. Returns True if that happened within timeout."""
        t0 = time.monotonic()
        while time.monotonic() - t0 < timeout:
            if cond():
                return True
            time.sleep(interval)
        return False

    def close(self):
        self._sock.close()

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()
