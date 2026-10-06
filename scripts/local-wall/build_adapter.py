#!/usr/bin/env python3
"""Build one immutable adapter with host-owned endpoint and identity constants."""
import argparse
import os
from pathlib import Path
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--endpoint", required=True)
    parser.add_argument("--peer-token", type=int, nargs=8, required=True)
    parser.add_argument("--direct-ports", type=int, nargs=4, required=True)
    parser.add_argument("--control-port", type=int, default=0)
    parser.add_argument("--control-fd", type=int, default=-1)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    endpoint = os.fsencode(args.endpoint)
    if not args.endpoint.startswith("/") or not 0 < len(endpoint) < 104 or b"\0" in endpoint:
        parser.error("endpoint must be an absolute Unix socket path shorter than 104 bytes")
    if any(not 0 <= word <= 0xFFFFFFFF for word in args.peer_token):
        parser.error("peer token requires eight unsigned 32-bit words")
    if len(set(args.direct_ports)) != 4 or any(not 1 <= port <= 65535 for port in args.direct_ports):
        parser.error("direct ports must be four distinct nonzero 16-bit ports")
    if not 0 <= args.control_port <= 65535 or args.control_fd < -1:
        parser.error("invalid control capability")
    if args.control_port in args.direct_ports or bool(args.control_port) != (args.control_fd >= 0):
        parser.error("control capability requires both a distinct port and inherited descriptor")
    output = args.output.absolute()
    if output.is_symlink():
        parser.error("output must not be a symlink")
    parent = output.parent.resolve(strict=True)
    output = parent / output.name
    source = Path(__file__).resolve().parent / "native"
    # Only numeric byte initializers enter generated C: no input can inject
    # source or options. The host stages the output in write-denied own state.
    with tempfile.TemporaryDirectory(prefix=".adapter-", dir=parent) as directory:
        scratch = Path(directory)
        configuration = scratch / "configuration.c"
        configuration.write_text(
            '#include "wire_client.h"\n'
            + "static const char endpoint[] = {" + ",".join(map(str, endpoint + b"\0")) + "};\n"
            + "const struct wall_configuration wall_configuration = { endpoint, {"
            + ",".join(map(str, args.peer_token)) + "} };\n"
            + "const uint16_t wall_direct_ports[4] = {" + ",".join(map(str, args.direct_ports)) + "};\n"
            + f"const uint16_t wall_control_port = {args.control_port};\n"
            + f"const int wall_control_fd = {args.control_fd};\n"
        )
        image = scratch / "adapter.dylib"
        subprocess.run([
            "clang", "-std=c11", "-Wall", "-Wextra", "-Werror", "-O2", "-dynamiclib", "-pthread",
            "-I", str(source), str(source / "wire_client.c"), str(source / "adapter.c"),
            str(configuration), "-Wl,-install_name," + str(output), "-o", str(image),
        ], check=True)
        subprocess.run(["/usr/bin/codesign", "--force", "--sign", "-", str(image)], check=True)
        os.chmod(image, 0o500)
        os.replace(image, output)
    print(output)


if __name__ == "__main__":
    main()
