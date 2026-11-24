# Spike payload standing in for a Pylair backend. The updater must
# replace this file with the bundle; the app dumps it to the state dir
# on every boot so CI can assert the installed copy changed.
VERSION = "0.1.0"
BROKEN = False


def on_startup():
    if BROKEN:
        raise RuntimeError("v3 fails on_startup deliberately")
