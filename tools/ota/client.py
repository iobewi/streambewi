#!/usr/bin/env python3
"""Qualification client of the system A OTA (#3). Standard library only, bounded memory.

    client.py maintenance http://DEVICE                  restart the device into maintenance (normal start only)
    client.py health https://DEVICE --ca CERT.pem         is the OTA server up
    client.py upload https://DEVICE IMAGE ID --ca CERT.pem [--activate] [--sha256 sha256:HEX]
    client.py activate https://DEVICE ID --ca CERT.pem

The token is IOBEWI_OTA_TOKEN (the one compiled into the image). The image is the application alone
(`streambewi-esp32s3-ota.bin`), never the first-install image. `--sha256` lets a test lie about the digest.
The protocol is the one of `rust-mcu-libs/examples/firmware/ota-system`; SHA-256 here proves integrity,
not who sent the image."""
import argparse
import hashlib
import http.client
import os
from pathlib import Path
import ssl
from urllib.parse import urlsplit

ID_CHARS = set("abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_.")


def origin(parser, text, scheme):
    url = urlsplit(text)
    if url.scheme != scheme or not url.hostname or url.path not in ("", "/") or url.username or url.query or url.fragment:
        parser.error(f"provide a {scheme.upper()} origin")
    return url


def token(parser):
    value = os.environ.get("IOBEWI_OTA_TOKEN", "")
    if not value or any(c in value for c in "\r\n"):
        parser.error("set IOBEWI_OTA_TOKEN")
    return value


def tls(parser, args):
    if not args.ca:
        parser.error("--ca CERT.pem is required (the server certificate is checked)")
    return ssl.create_default_context(cafile=args.ca)


def check_id(parser, name):
    if not 1 <= len(name) <= 64 or any(c not in ID_CHARS for c in name):
        parser.error("invalid deployment id")


def activate(url, context, secret, name):
    connection = http.client.HTTPSConnection(url.hostname, url.port or 443, context=context, timeout=60)
    connection.request("POST", "/ota/activate", body=b"", headers={
        "Authorization": "Bearer " + secret, "X-Deployment-ID": name})
    response = connection.getresponse()
    result = response.read(2048).decode()
    connection.close()
    print(f"activate {name}: {response.status} {result}")
    return response.status == 200 and result == "activating"


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    m = sub.add_parser("maintenance")
    m.add_argument("url")
    h = sub.add_parser("health")
    h.add_argument("url")
    h.add_argument("--ca")
    u = sub.add_parser("upload")
    u.add_argument("url")
    u.add_argument("image", type=Path)
    u.add_argument("deployment_id")
    u.add_argument("--ca")
    u.add_argument("--activate", action="store_true")
    u.add_argument("--sha256", help="send this digest instead of the image's (to test a refusal)")
    a = sub.add_parser("activate")
    a.add_argument("url")
    a.add_argument("deployment_id")
    a.add_argument("--ca")
    args = parser.parse_args()

    if args.command == "maintenance":
        url = origin(parser, args.url, "http")
        connection = http.client.HTTPConnection(url.hostname, url.port or 80, timeout=10)
        connection.request("POST", "/api/maintenance", body=b"confirm=maintenance",
                           headers={"Content-Type": "application/x-www-form-urlencoded"})
        response = connection.getresponse()
        print("maintenance:", response.status, response.read(512).decode())
        raise SystemExit(0 if response.status == 202 else 1)

    url = origin(parser, args.url, "https")
    context = tls(parser, args)
    if args.command == "health":
        connection = http.client.HTTPSConnection(url.hostname, url.port or 443, context=context, timeout=10)
        connection.request("GET", "/health")
        response = connection.getresponse()
        print("health:", response.status, response.read(64).decode())
        raise SystemExit(0 if response.status == 200 else 1)
    secret = token(parser)
    if args.command == "activate":
        check_id(parser, args.deployment_id)
        raise SystemExit(0 if activate(url, context, secret, args.deployment_id) else 1)

    check_id(parser, args.deployment_id)
    with args.image.open("rb") as image:
        digest = args.sha256 or "sha256:" + hashlib.file_digest(image, "sha256").hexdigest()
        length = os.fstat(image.fileno()).st_size
        if not length:
            parser.error("empty image")
        image.seek(0)
        connection = http.client.HTTPSConnection(url.hostname, url.port or 443, context=context, timeout=60)
        connection.putrequest("POST", "/ota/upload")
        for name, value in {"Authorization": "Bearer " + secret, "X-Deployment-ID": args.deployment_id,
                            "X-Image-SHA256": digest, "Content-Length": str(length)}.items():
            connection.putheader(name, value)
        connection.endheaders()
        remaining = length
        while remaining:
            chunk = image.read(min(2048, remaining))
            if not chunk:
                raise RuntimeError("image changed during upload")
            connection.send(chunk)
            remaining -= len(chunk)
        response = connection.getresponse()
        result = response.read(2048).decode()
        connection.close()
        print(f"upload {args.deployment_id}: {response.status} {result}")
        if response.status != 200 or result != "staged":
            raise SystemExit(1)
    if args.activate and not activate(url, context, secret, args.deployment_id):
        raise SystemExit(1)


if __name__ == "__main__":
    main()
