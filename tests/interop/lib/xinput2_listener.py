#!/usr/bin/env python3
"""Tiny XInput2 listener for the rust-side Xvfb.

Connects to an X server (DISPLAY env or argv[1]), selects XI2 events on the
root window, and appends every received Motion/Button/Key event as one
JSON line to argv[2] (default stdout).

Used by M4 Spike A (remotekeyboard/mousepad RECEIVE). Without xinput/xev
on the host, the listener itself is the only oracle that observes what
the rust daemon's XTest injection produced.

Run: python3 xinput2_listener.py <display> <output_log>
"""

import json
import os
import sys
import time

from Xlib import X, display as xdisplay, ext as xext
from Xlib.ext import xinput


def main():
    disp_arg = sys.argv[1] if len(sys.argv) > 1 else os.environ.get("DISPLAY")
    log_path = sys.argv[2] if len(sys.argv) > 2 else "/dev/stdout"
    if not disp_arg:
        print("xinput2_listener: no DISPLAY given", file=sys.stderr)
        return 2

    d = xdisplay.Display(disp_arg)
    root = d.screen().root

    # Select Motion, Button, Key on all master devices (covers XTest's
    # fake input devices). XI2 device_id=AllMasterDevices is the constant
    # in this python-xlib version.
    mask = (
        xinput.Motion
        | xinput.ButtonPress
        | xinput.ButtonRelease
        | xinput.KeyPress
        | xinput.KeyRelease
    )
    xext.xinput.select_events(root, [(xinput.AllMasterDevices, mask)])

    out = open(log_path, "a", buffering=1) if log_path != "/dev/stdout" else sys.stdout
    out.write(
        json.dumps({"event": "xinput2_listener_ready", "display": disp_arg}) + "\n"
    )
    out.flush()

    try:
        while True:
            e = d.next_event()
            # XI2 events come through as type 25 (GenericEvent).
            # python-xlib 0.33 on Fedora 43 doesn't expose GenericEvent
            # via X.X; the events still arrive in d.next_event(), but the
            # C-level `type` field may surface as 25 directly.
            row = {
                "event": "xinput2",
                "ts": time.time(),
                "raw_type": int(getattr(e, "type", 0)),
            }
            for attr in (
                "evtype", "detail", "values", "axisvalues",
                "flags", "keycode", "button", "root_x", "root_y",
            ):
                if hasattr(e, attr):
                    v = getattr(e, attr)
                    if attr in ("values",):
                        try:
                            row[attr] = {k: float(val) for k, val in v.items()}
                        except Exception:
                            row[attr] = str(v)
                    elif attr == "axisvalues":
                        try:
                            row[attr] = [int(x) for x in v]
                        except Exception:
                            row[attr] = str(v)
                    elif attr in ("root_x", "root_y", "keycode", "button",
                                  "detail", "evtype", "flags"):
                        try:
                            row[attr] = int(v)
                        except Exception:
                            row[attr] = str(v)
                    else:
                        row[attr] = str(v)
            out.write(json.dumps(row) + "\n")
            out.flush()
    except KeyboardInterrupt:
        return 0
    except Exception as ex:
        out.write(json.dumps({"event": "xinput2_listener_error", "error": str(ex)}) + "\n")
        out.flush()
        return 1
    finally:
        if out is not sys.stdout:
            out.close()
        d.close()


if __name__ == "__main__":
    sys.exit(main())