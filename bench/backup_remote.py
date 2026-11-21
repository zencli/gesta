#!/usr/bin/env python3
"""Does an encrypted backup survive a round trip through an S3-shaped server?

The unit tests check seal/unseal. This runs the whole path against a real HTTP server that answers
S3's four calls (PUT, GET, DELETE, list-type=2), because the failure this feature exists for is a
backup that cannot be restored, and every part in between is where that goes wrong: the seal, the key
name, the listing, and the restore into a live repository.

The server is deliberately dumb. It is a dictionary and a request parser; if the client signs
correctly and the format is right, the round trip works, and if either is wrong this fails loudly
rather than at three in the morning.
"""
import hashlib
import http.server
import os
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import urllib.parse

ZF = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "target", "release", "gesta")

BUCKET = "testbucket"
STORE = {}          # key -> bytes
SEEN_PUTS = []
SEEN_DELETES = []


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def _key(self):
        # /bucket/key... — the client signs this path exactly.
        path = urllib.parse.unquote(self.path.split("?", 1)[0])
        parts = path.lstrip("/").split("/", 1)
        return parts[1] if len(parts) > 1 else ""

    def do_PUT(self):
        key = self._key()
        n = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(n) if n else b""
        STORE[key] = body
        SEEN_PUTS.append(key)
        self.send_response(200)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def do_DELETE(self):
        key = self._key()
        existed = STORE.pop(key, None) is not None
        SEEN_DELETES.append(key)
        self.send_response(204 if existed else 404)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def do_GET(self):
        q = urllib.parse.urlparse(self.path).query
        params = urllib.parse.parse_qs(q)

        if params.get("list-type") == ["2"]:
            prefix = params.get("prefix", [""])[0]
            keys = sorted(k for k in STORE if k.startswith(prefix))
            body = (
                "<?xml version=\"1.0\"?><ListBucketResult>"
                + "".join(f"<Contents><Key>{k}</Key></Contents>" for k in keys)
                + "</ListBucketResult>"
            ).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/xml")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return

        key = self._key()
        if key in STORE:
            body = STORE[key]
            self.send_response(200)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        else:
            self.send_response(404)
            self.send_header("Content-Length", "0")
            self.end_headers()


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def run(args, cwd, env):
    return subprocess.run([ZF] + args, cwd=cwd, capture_output=True, text=True, env=env)


def main():
    checks = []
    d = tempfile.mkdtemp(prefix="backup-e2e-")
    port = free_port()
    server = http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler)
    t = threading.Thread(target=server.serve_forever, daemon=True)
    t.start()

    env = dict(os.environ)
    env.update({
        "GESTA_BACKUP_URL": f"s3://{BUCKET}/backups",
        "GESTA_S3_ENDPOINT": f"http://127.0.0.1:{port}",
        "GESTA_S3_REGION": "us-east-1",
        "GESTA_S3_ACCESS_KEY": "testkey",
        "GESTA_S3_SECRET_KEY": "testsecret",
        "GESTA_BACKUP_PASSPHRASE": "correct horse battery staple",
    })

    try:
        run(["init"], d, env)
        for i in range(3):
            with open(os.path.join(d, f"f{i}.txt"), "w") as fh:
                fh.write(f"content {i}\n" * 40)
        made = run(["changeset", "create", "first", "--author", "t", "--json"], d, env)
        m = re.search(r"(cs[0-9a-f]+)", made.stdout)
        if m:
            run(["changeset", "commit", m.group(1)], d, env)

        # 1. Push.
        r = run(["backup-remote", "push", "--prefix", "backups", "--keep", "3"], d, env)
        checks.append(("a backup pushes", r.returncode == 0, (r.stdout + r.stderr)[-300:]))
        checks.append(("exactly one object was uploaded", len(SEEN_PUTS) == 1, f"{len(SEEN_PUTS)}"))

        key = SEEN_PUTS[0] if SEEN_PUTS else ""
        checks.append(("the key ends in .bak", key.endswith(".bak"), key))
        checks.append(("the key sorts by time", bool(re.search(r"gesta-\d{8}T\d{6}\d{3}Z\.bak$", key)), key))

        # 2. It must not be readable. This is the whole point: the store holds every object and every
        #    branch, so an upload that is the database in the clear is a disclosure.
        blob = STORE.get(key, b"")
        checks.append(("the upload is smaller than the database", 0 < len(blob) < 10_000_000, f"{len(blob)}"))
        checks.append(("the upload is sealed text", blob.startswith(b"gesta-backup-v1"), blob[:20]))
        checks.append(("the sqlite header is not present", b"SQLite format 3" not in blob, "sealed" if blob else "empty"))

        # 3. A wrong passphrase must be refused, not silently produce garbage.
        bad = dict(env)
        bad["GESTA_BACKUP_PASSPHRASE"] = "wrong passphrase"
        rbad = run(["backup-remote", "pull", "latest", "--yes"], d, bad)
        checks.append(
            ("a wrong passphrase is refused",
             rbad.returncode != 0 and "wrong passphrase" in (rbad.stdout + rbad.stderr).lower(),
             (rbad.stdout + rbad.stderr)[-200:]),
        )

        # 4. Damage the seal and check the tag catches it rather than the restore.
        if key:
            tampered = bytearray(blob)
            # Flip a byte in the ciphertext, past the three header lines.
            body_at = tampered.index(b"\n", tampered.index(b"\n", tampered.index(b"\n") + 1) + 1) + 3
            tampered[body_at] = ord("0") if tampered[body_at] != ord("0") else ord("1")
            STORE[key] = bytes(tampered)
            rt = run(["backup-remote", "pull", "latest", "--yes"], d, env)
            checks.append(
                ("an edited backup is refused by the tag",
                 rt.returncode != 0,
                 (rt.stdout + rt.stderr)[-200:]),
            )
            STORE[key] = blob

        # 5. List.
        rl = run(["backup-remote", "list", "--prefix", "backups"], d, env)
        checks.append(("list shows the backup", rl.returncode == 0 and ".bak" in rl.stdout, rl.stdout[-160:]))

        # 6. Destroy the store, restore, and read the file back.
        before = run(["log", "--json"], d, env).stdout
        gesta_dir = os.path.join(d, ".gesta")
        shutil.rmtree(gesta_dir, ignore_errors=True)
        run(["init"], d, env)
        rr = run(["backup-remote", "pull", "latest", "--yes"], d, env)
        checks.append(("the restore succeeds", rr.returncode == 0, (rr.stdout + rr.stderr)[-300:]))

        after = run(["log", "--json"], d, env).stdout
        checks.append(("the history survived the restore", before.strip() == after.strip() and before.strip() != "",
                       f"before={len(before)} after={len(after)}"))
        content = run(["show", "f0.txt"], d, env)
        checks.append(("a file reads back", content.returncode == 0 and "content 0" in content.stdout,
                       (content.stdout + content.stderr)[-160:]))

        # 7. Pruning keeps the newest N.
        for i in range(4):
            run(["backup-remote", "push", "--prefix", "backups", "--keep", "3"], d, env)
        rp = run(["backup-remote", "prune", "--prefix", "backups", "--keep", "3"], d, env)
        left = [k for k in STORE if k.startswith("backups/")]
        checks.append(("prune keeps the newest three", len(left) == 3, f"{len(left)} left: {sorted(left)}"))
        checks.append(("the oldest was deleted", len(SEEN_DELETES) >= 2, f"{len(SEEN_DELETES)} deleted"))

    finally:
        server.shutdown()
        shutil.rmtree(d, ignore_errors=True)

    for name, ok, detail in checks:
        print(("  [PASS] " if ok else "  [FAIL] ") + name + ("" if ok else f"  ({detail})"))
    print()
    n = sum(1 for _, ok, _ in checks if ok)
    print(f"{n}/{len(checks)} off-site backup checks pass")
    return 0 if n == len(checks) else 1


if __name__ == "__main__":
    sys.exit(main())
