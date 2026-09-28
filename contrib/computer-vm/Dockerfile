# The desktop `wizard computer vm up` runs for the computer tool's vm backend:
# Xvfb, the fluxbox window manager, xterm, and x11vnc serving the display
# without a password. `vm up` publishes the VNC port on 127.0.0.1 only.
FROM alpine:3.20

RUN apk add --no-cache xvfb x11vnc fluxbox xterm xsetroot font-dejavu font-misc-misc \
    && adduser -D -s /bin/sh wizard \
    && mkdir -p /tmp/.X11-unix && chmod 1777 /tmp/.X11-unix

COPY start.sh /usr/local/bin/start
COPY menu /home/wizard/.fluxbox/menu
RUN chmod 0755 /usr/local/bin/start \
    && printf 'session.menuFile: ~/.fluxbox/menu\nsession.screen0.toolbar.visible: true\n' \
        > /home/wizard/.fluxbox/init \
    && chown -R wizard:wizard /home/wizard/.fluxbox

USER wizard
WORKDIR /home/wizard
ENV DISPLAY=:0 WIDTH=1280 HEIGHT=800
EXPOSE 5900
CMD ["/usr/local/bin/start"]
