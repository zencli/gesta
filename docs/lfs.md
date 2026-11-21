# Large files: the storage half and the protocol half

A large file costs its full size in every version that changes it, and it makes every operation that
touches its row pay for it. Two separate mechanisms deal with that, and they are useful independently.

## The storage half

An object at or above 4 MB is written to `.gesta/objects/<first two hex>/<rest>` and its row keeps
only the hash and the size. A `.gitattributes` rule decides in both directions:

```gitattributes
*.psd  large      # always beside the database, whatever its size
*.json -large     # never, even when it is 20 MB
```

The reason a rule exists beside the threshold: the writer usually knows what a file is, and a
generated 20 MB JSON belongs outside the database for the same reason a binary does - a `VACUUM`, a
backup and a single-row read all copy an inline blob in full.

This half is invisible. The hash is still the object's name, and every reader asks `get_object_bytes`,
which checks the sidecar first. Nothing above the store knows which shape an object has.

## The protocol half

`gesta serve` answers two routes that make a tree of pointers resolvable:

| Route | Method | Answers |
|---|---|---|
| `/<repo>/info/lfs/objects/batch` | POST | which of the requested oids the store holds, and where to fetch each |
| `/<repo>/info/lfs/objects/<oid>` | GET | the object's bytes |

The request and response follow the published batch shape:

```json
{"operation":"download","transfers":["basic"],"objects":[{"oid":"<hex>","size":123}]}

{"transfer":"basic","objects":[{"oid":"<hex>","size":123,"authenticated":true,
  "actions":{"download":{"href":"/<repo>/info/lfs/objects/<hex>",
                         "expires_at":"2099-01-01T00:00:00Z"}}}]}
```

Three decisions are worth recording because each one is a way the same feature goes wrong:

- **The `href` is a path, not an absolute URL.** The request carries no host and no scheme, and
  building one from the path produces a link that is wrong behind any proxy or on any non-default
  port. A client resolves a relative href against the endpoint it called, which is correct by
  construction.
- **A missing object is answered inside a `200`, not as an HTTP error.** One object the store does not
  hold must not fail the other ninety-nine in the same batch. The client reads `error.code` per object.
- **An `upload` for an object the store already holds gets no actions block at all.** That answer is
  distinct from `Missing`: a client reading `Missing` for an upload retries forever, while an empty
  entry tells it the transfer is already done.

## What the tree holds

A pointer is exactly three lines, and the version line has to be first:

```text
version https://git-lfs.github.com/spec/v1
oid sha256:<64 hex>
size <decimal bytes>
```

`gesta git export` writes this instead of the object's bytes when the store kept the object in the
sidecar, so a `git clone` of the exported repository carries 130 bytes where the payload was.
`gesta git import` reads it back: the pointer names its oid, the object is resolved against the
sidecar, and the real bytes are written under their own hash. A round trip through git therefore keeps
a binary intact rather than replacing it with the pointer text.

When the object is absent on import the pointer is stored as the text it is. That keeps the round trip
lossless and leaves the missing object visible; inventing bytes for it would be worse.

## What is not here

- **No `lfs` subcommand.** A client that speaks git-lfs uses the two routes above. There is no
  command that stages a file as a pointer on the client side, because the export does that.
- **No locking API.** `POST locks/verify` and the lock routes are absent. A team that relies on
  git-lfs locking to keep two people off one binary does not have that here.
- **`expires_at` is not enforced on read.** The value is emitted because a strict client rejects a
  response without it, and the object route does not check it afterwards. A leaked href works until
  the process stops, which is the same lifetime as a plain clone URL.
- **No transfer adapters beyond `basic`.** A client offering only `ssh` or `custom` is not served.

## Measured

`bench/lfs.py`, ten checks, a gate step. It writes a 5 MB payload, exports the repository to a real
git repository, reads the blob back through `git cat-file` to prove it is three lines of text, then
starts a real server and drives the batch API over HTTP - including a missing oid beside the held one,
and a byte-for-byte comparison of what the download link serves against what was committed.

```
10/10 lfs checks pass
```
