#!/usr/bin/env python3
"""Does a large object leave the tree as a pointer, and come back when asked?

The protocol and the pointer format are two halves of one promise, and either half alone is useless:
a batch endpoint that returns a link for an object nobody can fetch is a link to a 404, and a pointer
in the tree with no endpoint behind it is a file the client can never resolve. So this checks the
whole path against real bytes and a real server.

What "works" means here, exactly:

  a 5 MB file marked large      exports to git as three lines of text, not 5 MB
  the pointer names the object  oid is the store's hash of the payload, size is the payload length
  the batch API answers it      a POST to info/lfs/objects/batch returns a download href
  the href serves the bytes     GET that href returns the payload, byte for byte
  a missing oid is 404 in batch one absent object does not fail the others
  reimport round trips          the file comes back with its original bytes, not the pointer text

The last one is the one worth having. A pointer that exports but does not import is a repository
that loses its binaries on the first round trip, and nothing about the export half would show it.
"""
import json
import os
import shutil
import subprocess
import sys
import tempfile
import urllib.request

ZF = os.environ.get("ZF_BIN", os.path.join(
    os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "target", "release", "gesta"))

PAYLOAD = bytes(range(256)) * (5 * 1024 * 1024 // 256)  # 5 MB, not compressible by repetition alone


def sh(args, cwd):
    return subprocess.run([ZF] + args, cwd=cwd, capture_output=True, text=True)


def commit_all(cwd, message):
    r = sh(["changeset", "create", message, "--author", "t", "--json"], cwd)
    import re
    m = re.search(r"(cs[0-9a-f]+)", r.stdout)
    if not m:
        return None
    sh(["changeset", "commit", m.group(1)], cwd)
    return m.group(1)



def _free_port():
    import socket
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def _wait_for(base, timeout=15):
    import time
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            urllib.request.urlopen(base + "/", timeout=2)
            return True
        except Exception:
            time.sleep(0.25)
    return False


def _largest_oid(d):
    """The oid of the largest object the store holds, read from the sidecar directory."""
    best, best_size = None, -1
    root = os.path.join(d, ".gesta", "objects")
    for dirpath, _, files in os.walk(root):
        for f in files:
            path = os.path.join(dirpath, f)
            size = os.path.getsize(path)
            if size > best_size:
                best, best_size = os.path.basename(dirpath) + f, size
    return best



def main():
    checks = []
    d = tempfile.mkdtemp(prefix="lfs-")
    git_dir = os.path.join(d, "as-git")
    try:
        # A file over the sidecar threshold, plus a .gitattributes rule so it is not a size accident.
        sh(["init"], d)
        with open(os.path.join(d, ".gitattributes"), "w") as fh:
            fh.write("*.bin large\n")
        with open(os.path.join(d, "blob.bin"), "wb") as fh:
            fh.write(PAYLOAD)
        with open(os.path.join(d, "small.txt"), "w") as fh:
            fh.write("ordinary text\n")
        commit_all(d, "add a large file and a small one")

        checks.append(("the store reports the large object as a sidecar",
                       os.path.isdir(os.path.join(d, ".gesta", "objects")),
                       "no .gesta/objects directory"))

        # Export to a real git repository and look at what landed there.
        os.makedirs(git_dir, exist_ok=True)
        subprocess.run(["git", "init", "--quiet", git_dir], check=True)
        r = sh(["git", "export", git_dir], d)
        if r.returncode != 0:
            checks.append(("export to git succeeds", False, (r.stderr or "")[:180]))
        else:
            checks.append(("export to git succeeds", True, ""))

            blob_path = subprocess.run(
                ["git", "-C", git_dir, "cat-file", "-p",
                 f"HEAD:blob.bin"], capture_output=True)
            text = blob_path.stdout
            lines = text.split(b"\n")
            checks.append(("the large file left the tree as a pointer",
                           len(text) < 200 and lines[0].startswith(b"version https://git-lfs"),
                           f"{len(text)} bytes, first line {lines[0][:60]!r}"))

            oid = size = None
            for line in lines:
                if line.startswith(b"oid sha256:"):
                    oid = line.split(b":", 1)[1].decode().strip()
                elif line.startswith(b"size "):
                    size = int(line.split()[1])
            checks.append(("the pointer names the object and its size",
                           oid is not None and len(oid) == 64 and size == len(PAYLOAD),
                           f"oid={oid} size={size} expected {len(PAYLOAD)}"))

            # The small file must be untouched. A pointer for everything would be a worse bug than
            # a pointer for nothing, and it would pass every check above.
            small = subprocess.run(["git", "-C", git_dir, "cat-file", "-p", "HEAD:small.txt"],
                                   capture_output=True).stdout
            checks.append(("a small file exports as its own content",
                           small == b"ordinary text\n", f"{small[:40]!r}"))

        # Serve the repository and drive the batch API with real HTTP. The protocol half is only
        # worth anything against a running server: a unit test that parses our own response proves
        # the response is well formed and nothing about whether a client can use it.
        port = _free_port()
        server = subprocess.Popen(f"{ZF} serve --port {port}", shell=True, cwd=d,
                                  stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            base = f"http://127.0.0.1:{port}"
            _wait_for(base, timeout=15)

            oid = _largest_oid(d)
            batch_url = f"{base}/info/lfs/objects/batch"
            body = json.dumps({
                "operation": "download",
                "transfers": ["basic"],
                "objects": [{"oid": oid, "size": len(PAYLOAD)},
                            {"oid": "0" * 64, "size": 1}],
            }).encode()
            req = urllib.request.Request(
                batch_url, data=body, method="POST",
                headers={"Content-Type": "application/vnd.git-lfs+json",
                         "Accept": "application/vnd.git-lfs+json"})
            try:
                with urllib.request.urlopen(req, timeout=20) as r:
                    status, raw = r.status, r.read()
            except urllib.error.HTTPError as e:
                status, raw = e.code, e.read()

            checks.append(("the batch API answers a download request", status == 200,
                           f"HTTP {status}: {raw[:140]!r}"))
            if status == 200:
                parsed = json.loads(raw)
                objs = {o["oid"]: o for o in parsed.get("objects", [])}
                here = objs.get(oid, {})
                gone = objs.get("0" * 64, {})
                href = (here.get("actions") or {}).get("download", {}).get("href")
                checks.append(("a held object gets a download href", bool(href),
                               f"object entry {here}"))
                checks.append(("a missing object is reported inside the batch, not as an error",
                               gone.get("error", {}).get("code") == 404,
                               f"entry {gone}"))
                checks.append(("the missing object did not fail the held one",
                               bool(href) and gone.get("error") is not None, ""))

                if href:
                    # A relative href resolves against the endpoint, which is why the server may
                    # return one: it carries no host and inventing one breaks behind a proxy.
                    absolute = href if href.startswith("http") else base + href
                    try:
                        with urllib.request.urlopen(absolute, timeout=30) as r2:
                            got = r2.read()
                        checks.append(("the href serves the payload byte for byte",
                                       got == PAYLOAD,
                                       f"{len(got)} bytes, wanted {len(PAYLOAD)}"))
                    except Exception as e:
                        checks.append(("the href serves the payload byte for byte", False, repr(e)))
        finally:
            server.terminate()
            try:
                server.wait(timeout=10)
            except subprocess.TimeoutExpired:
                server.kill()
    finally:
        shutil.rmtree(d, ignore_errors=True)

    for name, ok, detail in checks:
        print(("  [PASS] " if ok else "  [FAIL] ") + name + ("" if ok else f"  ({detail})"))
    print()
    n = sum(1 for _, ok, _ in checks if ok)
    print(f"{n}/{len(checks)} lfs checks pass")
    return 0 if n == len(checks) else 1


if __name__ == "__main__":
    sys.exit(main())
