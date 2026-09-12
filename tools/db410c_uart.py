#!/usr/bin/env python3
"""Capture the lab UART at 115200 8N1; optionally send text or a serial break.

The output is the exact received byte stream. A caller can run fastboot in a
separate process while this bounded capture is running. Nothing is transmitted
unless --send or --break is explicitly supplied.
"""

import argparse
import fcntl
import os
from pathlib import Path
import select
import sys
import termios
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--uart", required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--seconds", type=float, default=30)
    parser.add_argument("--send", help="literal text to transmit, including any newline")
    parser.add_argument("--tx-delay", type=float, default=0.005,
                        help="seconds between transmitted bytes for bootloader UART FIFOs")
    parser.add_argument("--break", dest="send_break", action="store_true")
    args = parser.parse_args()
    if args.tx_delay < 0:
        parser.error("--tx-delay cannot be negative")
    if not 0 < args.seconds <= 600:
        parser.error("--seconds must be between 0 and 600")
    args.output.parent.mkdir(parents=True, exist_ok=True)
    with args.output.open("xb", buffering=0) as log:
        fd = os.open(args.uart, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            fcntl.ioctl(fd, termios.TIOCEXCL)
            settings = termios.tcgetattr(fd)
            settings[0] = settings[1] = settings[3] = 0
            settings[2] = termios.CS8 | termios.CREAD | termios.CLOCAL
            settings[4] = settings[5] = termios.B115200
            settings[6][termios.VMIN] = settings[6][termios.VTIME] = 0
            termios.tcsetattr(fd, termios.TCSANOW, settings)
            if args.send_break:
                termios.tcsendbreak(fd, 0)
            if args.send is not None:
                for byte in args.send.encode():
                    if not select.select([], [fd], [], 5)[1]:
                        raise TimeoutError("UART write timed out")
                    if os.write(fd, bytes([byte])) != 1:
                        raise OSError("short UART write")
                    time.sleep(args.tx_delay)
                termios.tcdrain(fd)
            print(f"Capturing {args.uart} to {args.output}", file=sys.stderr, flush=True)
            deadline = time.monotonic() + args.seconds
            while (remaining := deadline - time.monotonic()) > 0:
                if select.select([fd], [], [], min(remaining, 0.25))[0]:
                    data = os.read(fd, 65536)
                    if not data:
                        raise OSError("UART disconnected")
                    log.write(data)
                    sys.stdout.buffer.write(data)
                    sys.stdout.buffer.flush()
        finally:
            os.close(fd)


if __name__ == "__main__":
    main()
