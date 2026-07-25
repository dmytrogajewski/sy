#!/usr/bin/env python3
"""Switch a Fibocom L850-GL / Intel XMM7360 USB modem's composition via
AT+GTUSBMODE, so ModemManager drives it over MBIM instead of legacy
AT+PPP dialup.

This is invoked by `sy wwan modeswitch`; it can also be run by hand. It
speaks raw AT to a CDC-ACM port using only the Python standard library
(termios), so it has no third-party dependencies. The caller is
responsible for stopping ModemManager first (it holds the AT port) and
restarting it afterwards — `sy wwan modeswitch` does that orchestration.

Usage:
  wwan_modeswitch.py --check              # query current mode, change nothing
  wwan_modeswitch.py --mode 7             # switch to MBIM, then reset
  wwan_modeswitch.py --revert             # switch back to mode 0 (NCM), reset
  wwan_modeswitch.py --port /dev/ttyACM0  # override port autodetection

MBIM is mode 7. The switch persists across power cycles (one-time per
modem). WARNING: on some XMM7360 firmware the mode-7 switch is reported
to cause a reboot loop; this tool prints the current firmware so the
caller can judge the risk before committing.
"""
import argparse
import glob
import os
import select
import sys
import termios
import time

MBIM_MODE = 7
NCM_MODE = 0
RESET_CMD = "AT+CFUN=15"


def at(port, cmd, read_secs=3.0):
    """Send one AT command, return the decoded reply text (or '' on error)."""
    try:
        fd = os.open(port, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
    except OSError as e:
        print(f"cannot open {port}: {e}", file=sys.stderr)
        return ""
    try:
        a = termios.tcgetattr(fd)
        a[0] = 0  # iflag
        a[1] = 0  # oflag
        a[3] = 0  # lflag (raw)
        a[2] |= termios.CLOCAL | termios.CREAD
        termios.tcsetattr(fd, termios.TCSANOW, a)
        termios.tcflush(fd, termios.TCIOFLUSH)
        os.write(fd, (cmd + "\r").encode())
        end = time.time() + read_secs
        buf = b""
        while time.time() < end:
            r, _, _ = select.select([fd], [], [], end - time.time())
            if r:
                try:
                    c = os.read(fd, 4096)
                except (BlockingIOError, OSError):
                    break
                if c:
                    buf += c
                    if b"OK" in buf or b"ERROR" in buf:
                        break
        return buf.decode(errors="replace")
    finally:
        try:
            os.close(fd)
        except OSError:
            pass


def find_port(explicit):
    if explicit:
        return explicit
    ports = sorted(glob.glob("/dev/ttyACM*"))
    if not ports:
        return None
    return ports[0]


def parse_mode(reply):
    for line in reply.splitlines():
        line = line.strip()
        if line.startswith("+GTUSBMODE:"):
            return line.split(":", 1)[1].strip()
    return None


def main():
    ap = argparse.ArgumentParser(description="XMM7360 USB mode switch")
    g = ap.add_mutually_exclusive_group()
    g.add_argument("--mode", type=int, help="GTUSBMODE value to set")
    g.add_argument("--check", action="store_true", help="query only")
    g.add_argument("--revert", action="store_true", help="set NCM mode 0")
    ap.add_argument("--port", help="AT port (default: first /dev/ttyACM*)")
    args = ap.parse_args()

    port = find_port(args.port)
    if not port:
        print("no /dev/ttyACM* AT port found; is the modem plugged in and "
              "ModemManager stopped?", file=sys.stderr)
        return 3

    if not at(port, "AT", 2.0).strip().endswith("OK") and "OK" not in at(port, "AT", 2.0):
        print(f"{port} did not answer AT", file=sys.stderr)
        return 4

    cur = parse_mode(at(port, "AT+GTUSBMODE?"))
    supported = parse_mode(at(port, "AT+GTUSBMODE=?"))
    fw = None
    fwr = at(port, "AT+CGMR")
    for line in fwr.splitlines():
        line = line.strip()
        if line and line != "OK" and not line.startswith("AT"):
            fw = line
            break
    print(f"port={port} current_mode={cur} supported={supported} firmware={fw}")

    if args.check:
        return 0

    target = NCM_MODE if args.revert else (args.mode if args.mode is not None else MBIM_MODE)
    if cur is not None and cur == str(target):
        print(f"already in mode {target}; nothing to do")
        return 0

    print(f"setting GTUSBMODE={target} ...")
    reply = at(port, f"AT+GTUSBMODE={target}")
    if "OK" not in reply:
        print(f"mode set failed: {reply!r}", file=sys.stderr)
        return 5
    print(f"resetting modem ({RESET_CMD}) — it will re-enumerate ...")
    at(port, RESET_CMD, 2.0)
    print("done; wait ~15s for re-enumeration, then restart ModemManager")
    return 0


if __name__ == "__main__":
    sys.exit(main())
