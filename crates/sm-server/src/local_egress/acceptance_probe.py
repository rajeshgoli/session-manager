"""Executed inside production walls by the Rust two-agent acceptance test."""
import errno
import http.client
import json
import os
import socket
import sys
from urllib.parse import urlsplit

mode, data = sys.argv[1], json.loads(sys.argv[2])
if mode == "submit":
    endpoint = urlsplit(data["gateway"])
    client = http.client.HTTPConnection(endpoint.hostname, endpoint.port, timeout=5)
    client.request("POST", data.get("route", "/queue-jobs"), json.dumps(data["body"]), {
        "Content-Type": "application/json", "X-SM-Local-Agent": "forged",
        "X-SM-Gateway-Signature": "forged", "X-SM-Session-ID": "forged",
        "X-SM-Session-Credential": "forged", "Authorization": "Bearer forged",
    })
    response = client.getresponse()
    result = {"status": response.status, "body": json.loads(response.read())}
    with open(data["result"], "w") as output:
        json.dump(result, output)
else:
    assert os.environ["CLAUDE_SESSION_MANAGER_ID"] == data["agent"]
    assert os.environ["SM_API_URL"] == data["gateway"]
    assert os.environ["HTTP_PROXY"] == data["proxy"]
    for port in data["forbidden_ports"]:
        try:
            connection = socket.create_connection(("127.0.0.1", port), timeout=2)
        except OSError as error:
            assert error.errno in (errno.EACCES, errno.EPERM), error
        else:
            connection.close()
            raise AssertionError(f"direct connection escaped: {port}")
    try:
        connection = socket.create_connection(("93.184.216.34", 443), timeout=2)
    except OSError as error:
        assert error.errno in (errno.EACCES, errno.EPERM), error
    else:
        connection.close()
        raise AssertionError("direct public egress escaped")
    try:
        with open(data["secret"], "rb") as secret:
            secret.read()
    except OSError as error:
        assert error.errno in (errno.EACCES, errno.EPERM), error
    else:
        raise AssertionError("gateway secret readable")
    proxy = urlsplit(os.environ["HTTPS_PROXY"])
    connection = socket.create_connection((proxy.hostname, proxy.port), timeout=5)
    connection.sendall(b"CONNECT public.example:443 HTTP/1.1\r\nX-SM-Local-Agent: forged\r\n\r\n")
    reply = b""
    while not reply.endswith(b"\r\n\r\n"):
        reply += connection.recv(1)
    assert reply.startswith(b"HTTP/1.1 200"), reply
    connection.sendall(b"opaque-probe")
    connection.shutdown(socket.SHUT_WR)
    echoed = b""
    while True:
        chunk = connection.recv(1024)
        if not chunk:
            break
        echoed += chunk
    assert echoed == b"opaque-probe", echoed
    connection.close()
    print("combined-wall-ok", flush=True)
