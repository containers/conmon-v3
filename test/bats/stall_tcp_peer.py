#!/usr/bin/env python3
"""TCP peers for the non-blocking syslog tests.

`free-port` prints an unused localhost port and exits.

`serve PORTFILE STATUSFILE` accepts one connection and never reads it.
The chosen port is written to PORTFILE. STATUSFILE is rewritten with the
peak unread byte count (`unread=N`) and `reads=0`.
"""

import array
import fcntl
import os
import socket
import sys
import time

# Linux FIONREAD / SIOCINQ. Reports bytes the application has not read.
FIONREAD = 0x541B
TCP_WINDOW_CLAMP = 10


def free_port() -> None:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        print(sock.getsockname()[1])


def write_status(status_path: str, unread: int) -> None:
    tmp = status_path + ".tmp"
    with open(tmp, "w", encoding="ascii") as fh:
        fh.write(f"unread={unread}\nreads=0\n")
    os.replace(tmp, status_path)


def serve(port_path: str, status_path: str) -> None:
    sock = socket.socket()
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    try:
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 1024)
    except OSError:
        pass
    sock.bind(("127.0.0.1", 0))
    sock.listen(16)
    with open(port_path, "w", encoding="ascii") as fh:
        fh.write(str(sock.getsockname()[1]))
    sock.settimeout(30)
    conn, _ = sock.accept()
    try:
        conn.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 1024)
        conn.setsockopt(socket.IPPROTO_TCP, TCP_WINDOW_CLAMP, 2048)
    except OSError:
        pass
    # Do not recv(). Sample the unread queue until the test kills this process.
    peak = 0
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        buf = array.array("i", [0])
        try:
            fcntl.ioctl(conn.fileno(), FIONREAD, buf, True)
        except OSError:
            break
        if buf[0] > peak:
            peak = buf[0]
        write_status(status_path, peak)
        time.sleep(0.05)


def main() -> None:
    if len(sys.argv) == 2 and sys.argv[1] == "free-port":
        free_port()
        return
    if len(sys.argv) == 4 and sys.argv[1] == "serve":
        serve(sys.argv[2], sys.argv[3])
        return
    sys.exit(
        "usage: stall_tcp_peer.py free-port | stall_tcp_peer.py serve PORTFILE STATUSFILE"
    )


if __name__ == "__main__":
    main()
