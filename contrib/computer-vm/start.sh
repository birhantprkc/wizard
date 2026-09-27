#!/bin/sh
# Start the virtual display, a window manager, and a VNC server on it.
set -eu

Xvfb :0 -screen 0 "${WIDTH}x${HEIGHT}x24" -nolisten tcp &

i=0
while [ ! -e /tmp/.X11-unix/X0 ] && [ "$i" -lt 100 ]; do
    sleep 0.1
    i=$((i + 1))
done

xsetroot -solid '#2e3440'
fluxbox >/tmp/fluxbox.log 2>&1 &

exec x11vnc -display :0 -rfbport 5900 -forever -shared -nopw -noxdamage -xkb -quiet
