#!/usr/bin/env python3
"""A minimal WebSocket client for bench/websocket.sh.

Written by hand rather than pulled from PyPI so the benchmark has no
dependency beyond the Python that ships with the machine, and so the
handshake it performs is visible in the repository rather than hidden behind
a library that might paper over a proxy bug.

Usage:
    ws-client.py <host> <port> <path> <message>
    ws-client.py <host> <port> <path> --hold <seconds>

Prints one line to stdout:
    status=<http status>  echo=<what came back>
so the shell can assert on both the handshake and the tunnel.
"""

import base64
import hashlib
import os
import socket
import sys
import time


def handshake(sock, host, port, path):
    """Send the RFC 6455 opening handshake and verify the response."""
    key = base64.b64encode(os.urandom(16)).decode()
    request = (
        f"GET {path} HTTP/1.1\r\n"
        f"Host: {host}:{port}\r\n"
        "Upgrade: websocket\r\n"
        "Connection: Upgrade\r\n"
        f"Sec-WebSocket-Key: {key}\r\n"
        "Sec-WebSocket-Version: 13\r\n"
        "\r\n"
    )
    sock.sendall(request.encode())

    raw = b""
    while b"\r\n\r\n" not in raw:
        chunk = sock.recv(4096)
        if not chunk:
            break
        raw += chunk
    head, _, rest = raw.partition(b"\r\n\r\n")
    lines = head.decode("latin-1").split("\r\n")
    status = int(lines[0].split()[1]) if len(lines[0].split()) > 1 else 0

    if status != 101:
        return status, None, rest

    # The accept key is the only part of the handshake that proves the origin
    # — not the proxy — completed it. A proxy that answered 101 on its own
    # would fail here.
    headers = {}
    for line in lines[1:]:
        name, _, value = line.partition(":")
        headers[name.strip().lower()] = value.strip()
    expected = base64.b64encode(
        hashlib.sha1((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()
    ).decode()
    if headers.get("sec-websocket-accept") != expected:
        return status, "BAD-ACCEPT", rest
    return status, None, rest


def send_text(sock, message):
    """One masked text frame, which is the only kind a client may send."""
    payload = message.encode()
    mask = os.urandom(4)
    masked = bytes(byte ^ mask[i % 4] for i, byte in enumerate(payload))
    sock.sendall(bytes([0x81, 0x80 | len(payload)]) + mask + masked)


def read_text(sock, buffered=b""):
    """Read one unmasked server text frame."""
    data = buffered
    while len(data) < 2:
        chunk = sock.recv(4096)
        if not chunk:
            return ""
        data += chunk
    length = data[1] & 0x7F
    while len(data) < 2 + length:
        chunk = sock.recv(4096)
        if not chunk:
            return ""
        data += chunk
    return data[2 : 2 + length].decode("utf-8", "replace")


def main():
    host, port, path = sys.argv[1], int(sys.argv[2]), sys.argv[3]
    sock = socket.create_connection((host, port), timeout=20)
    try:
        status, problem, rest = handshake(sock, host, port, path)
        if status != 101:
            print(f"status={status} echo=")
            return
        if problem:
            print(f"status={status} echo={problem}")
            return

        if sys.argv[4] == "--hold":
            # Hold the tunnel open so the caller can observe how many sockets
            # are live and whether pages still render alongside them.
            time.sleep(float(sys.argv[5]))
            print(f"status={status} echo=held")
            return

        send_text(sock, sys.argv[4])
        print(f"status={status} echo={read_text(sock, rest)}")
    finally:
        sock.close()


if __name__ == "__main__":
    main()
