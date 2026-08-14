#!/usr/bin/env python3
"""Tiny org.freedesktop.Notifications stub for the M3 harness.

The kde private session bus (lib.sh:113, activation disabled) has no real
notification server. Without one, two M3 phases break:

  Phase 4 (kde SENDS): kdeconnectd's SendNotificationsPlugin uses
    BecomeMonitor with rule `interface='org.freedesktop.Notifications',
    member='Notify'`. BecomeMonitor sees messages at the bus level BEFORE
    destination dispatch — the message reaches the monitor even without
    a destination owner. So this direction CAN work without a server.

  Phase 5 (rust SENDS): kdeconnectd's notificationsplugin receives the
    rust packet and uses KNotification (KDE framework) to display it.
    KNotification calls Notify on the bus via the standard KDE
    notification service — it does NOT use BecomeMonitor, so the call
    fails when no one owns the org.freedesktop.Notifications name
    (`kf.notifications: Failed to notify ... The name
    org.freedesktop.Notifications was not provided by any .service files`).
    This helper claims the name so KNotification can succeed.

The script runs until SIGTERM; cleanup() in lib.sh kills it.

Usage:  notif_server.py <DBUS_SESSION_BUS_ADDRESS>
"""

import signal
import sys

import dbus
import dbus.service
from dbus.mainloop.glib import DBusGMainLoop
from gi.repository import GLib

DBusGMainLoop(set_as_default=True)


class NotificationsServer(dbus.service.Object):
    def __init__(self, bus):
        bus_name = dbus.service.BusName(
            "org.freedesktop.Notifications", bus=bus, allow_replacement=True
        )
        super().__init__(bus_name, "/org/freedesktop/Notifications")
        self._next_id = 1

    @dbus.service.method(
        "org.freedesktop.Notifications",
        in_signature="susssasa{sv}i",
        out_signature="u",
    )
    def Notify(
        self,
        app_name,
        replaces_id,
        app_icon,
        summary,
        body,
        actions,
        hints,
        expire_timeout,
    ):
        # Return a synthesized id; the test oracle is the dbus-monitor
        # capture, not the return value.
        rid = self._next_id
        self._next_id += 1
        return rid

    @dbus.service.method("org.freedesktop.Notifications", out_signature="ssss")
    def GetServerInformation(self):
        return ("notif_server.py", "M3 harness", "1.0", "1.0")

    @dbus.service.method("org.freedesktop.Notifications", out_signature="")
    def CloseNotification(self, notification_id):
        return


def main():
    if len(sys.argv) != 2:
        sys.stderr.write("usage: notif_server.py <DBUS_SESSION_BUS_ADDRESS>\n")
        sys.exit(2)
    bus = dbus.bus.BusConnection(sys.argv[1])
    NotificationsServer(bus)
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
    signal.signal(signal.SIGINT, lambda *_: sys.exit(0))
    GLib.MainLoop().run()


if __name__ == "__main__":
    main()
