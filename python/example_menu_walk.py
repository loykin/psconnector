"""psconnector example: move n tiles right on the home screen game row, then back.

Run it from the psconnector-view bar, or with psconnector-view running: `python3 example_menu_walk.py 3`.
"""
import sys

from psconnector import PS

n = int(sys.argv[1]) if len(sys.argv) > 1 else 2
with PS() as ps:
    try:
        for i in range(n):
            ps.osd(f'right {i + 1}/{n}')
            ps.tap('right')
            ps.sleep(0.5)
        w, h, _ = ps.frame()
        print(f'screen {w}x{h}, center pixel {ps.pixel(w // 2, h // 2)}')
        for i in range(n):
            ps.osd(f'left {i + 1}/{n}')
            ps.tap('left')
            ps.sleep(0.5)
    finally:
        ps.osd('')
