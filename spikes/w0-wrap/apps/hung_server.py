"""W0 fixture: binds the port, answers the FIRST request (so ready passes),
then accepts connections but never responds - alive but stuck. Proves why
pongs alone can't detect a wedged server (W1's HTTP probe rationale).

Windows correctness: recv() the request before answering. close() on a
socket that still holds unread request data is abortive (RST) there and
the 200 never lands. Keep accepted sockets referenced too: rebinding the
variable would close the previous one at GC time, turning "held open"
into "reset" instead of the intended hang.
"""

import os
import socket

with socket.socket() as s:
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("127.0.0.1", int(os.environ["PYLAIR_PORT"])))
    s.listen(8)
    held = []  # accepted connections, never answered
    first = True
    while True:
        conn, _ = s.accept()
        if first:
            try:
                conn.recv(65536)
                conn.sendall(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
            finally:
                conn.close()
            first = False
        else:
            held.append(conn)
