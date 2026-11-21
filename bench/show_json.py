import base64
import json
import os
import re
import shutil
import subprocess
import tempfile

# Found relative to this file, and overridable, the way every other bench here does it. A hardcoded
# path works in one container and fails on any other machine: this one said `/work/gesta`, which is
# where I build, and CI checks out to `/home/runner/work/gesta/gesta`, so the step failed there with
# `No such file or directory` while passing locally every single time.
ZF = os.environ.get("ZF_BIN", os.path.join(
    os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "target", "release", "gesta"))
BIN = b"\x00\x01\x02\xff\xfe\x80binary"


def run(args, cwd):
    return subprocess.run([ZF] + args, cwd=cwd, capture_output=True, text=False)


d = tempfile.mkdtemp(prefix="showjson-")
checks = []
try:
    run(["init"], d)
    with open(os.path.join(d, "b.dat"), "wb") as fh:
        fh.write(BIN)
    with open(os.path.join(d, "t.txt"), "w") as fh:
        fh.write("hello\n")
    c = run(["changeset", "create", "x", "--author", "t", "--json"], d)
    m = re.search(r"(cs[0-9a-f]+)", c.stdout.decode())
    run(["changeset", "commit", m.group(1)], d)

    # A text file: the JSON carries the content as a string.
    r = run(["show", "--json", "t.txt"], d)
    try:
        doc = json.loads(r.stdout)
    except Exception:
        doc = None
    checks.append(("a text file comes back as JSON", doc is not None, r.stdout[:120]))
    checks.append(("it is not marked binary", doc is not None and doc.get("binary") is False, str(doc)[:120] if doc else ""))
    checks.append(("its content is the file", doc is not None and doc.get("content") == "hello\n", repr(doc.get("content")) if doc else ""))
    checks.append(("it names the path", doc is not None and doc.get("path") == "t.txt", str(doc.get("path")) if doc else ""))

    # A binary file: content cannot be a JSON string, so it is base64 and said to be.
    r = run(["show", "--json", "b.dat"], d)
    try:
        doc = json.loads(r.stdout)
    except Exception:
        doc = None
    checks.append(("a binary file comes back as JSON", doc is not None, r.stdout[:120]))
    checks.append(("it is marked binary", doc is not None and doc.get("binary") is True, str(doc)[:120] if doc else ""))
    ok_b64 = False
    if doc and isinstance(doc.get("content_base64"), str):
        try:
            ok_b64 = base64.b64decode(doc["content_base64"]) == BIN
        except Exception:
            ok_b64 = False
    checks.append(("the base64 decodes to the bytes", ok_b64,
                   (doc.get("content_base64") if doc else "")[:40]))
    checks.append(("the JSON has no raw content field for binary", doc is not None and "content" not in doc, str(sorted(doc.keys())) if doc else ""))

    # Without --json the bytes still come out untouched, which is what a pipe into a file expects.
    r = run(["show", "b.dat"], d)
    checks.append(("without --json the bytes are unchanged", r.stdout == BIN, repr(r.stdout[:24])))

    for name, ok, detail in checks:
        print(("  [PASS] " if ok else "  [FAIL] ") + name + ("" if ok else f"  ({detail})"))
    print()
    n = sum(1 for _, ok, _ in checks if ok)
    print(f"{n}/{len(checks)} show --json checks pass")
finally:
    shutil.rmtree(d, ignore_errors=True)
