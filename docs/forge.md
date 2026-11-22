# Pushing to a forge

GitHub, GitLab and Gitea speak git's protocol. Until this release, reaching any of them meant having
the git binary installed: the tool exported a bare repository to a temporary directory and ran
`git push` in it. That works, and it costs a process plus a full copy of the tree on every push.

`gesta forge push` makes the same three requests git makes, and needs no git anywhere on the machine.

## The command

```
GESTA_FORGE_TOKEN=... gesta forge push https://github.com/owner/repo main
```

```
pushed main to https://github.com/owner/repo
  refs/heads/main now at the commit the remote accepted
```

Flags:

- `--remote-ref refs/heads/main` sets the ref name on the remote, when it should differ from the local
  branch name.
- `--token` takes the token directly. Prefer `GESTA_FORGE_TOKEN`: a token in an argument is a token in
  the shell history and in `ps`.
- `--user` sets the username paired with the token. GitHub and Gitea ignore it. GitLab wants `oauth2`.
- `--json` prints the ref and whether the remote took it.

## What it sends, in order

1. `GET <base>/info/refs?service=git-receive-pack`: the refs the remote holds and the capabilities it
   offers. This is also where a bad token fails, before anything is sent.
2. `POST <base>/git-receive-pack`: a pkt-line command list (`<old-id> <new-id> <ref>`, with the
   capabilities after a NUL on the first line), a flush, then a pack.
3. Read the `report-status` that comes back. `ok <ref>` is a push; `ng <ref> <reason>` is a refusal and
   is reported with the remote's own words.

The pack writer is the one the server half already used to answer a clone, so there is one
implementation of what an object looks like on the wire rather than two.

## What is verified, and what is not

Verified against this tool's own server, end to end:

```
$ gesta forge push http://127.0.0.1:8791/git/fremote main
pushed main to http://127.0.0.1:8791/git/fremote

$ git clone http://127.0.0.1:8792/git/fremote gc
$ ls gc
a.txt
$ cat gc/a.txt
alpha
$ git log --oneline
f01d692 seed
```

A real git client cloned what the forge client pushed, and got the file contents back. Eight unit tests
cover the parts that decide right from wrong: an advertisement is parsed into refs and capabilities, a
ref the remote does not have answers `None` so the command carries zeroes, an HTML login page is
reported rather than read as a repository with no branches, the capabilities land on the first command
only, a rejected ref keeps its reason, and a silent remote is a failure rather than a success.

Not verified: a push to github.com. That needs a token and a network, and this was developed without
either. The request shape is the one git uses and the pieces that differ per forge (the URL path, the
header) are parameters rather than assumptions, but a claim that it works against GitHub would be a
claim this repository has not earned.

## Limits

- **One ref per push.** A push that updates several branches is several invitations. The protocol
  supports doing them in one request; this does not yet.
- **No force push.** The command carries the old id the remote advertised, so a non-fast-forward
  update is refused by the remote. Sending `--force` is not implemented, and adding it means sending
  the old id the client *wants*, which is a different decision from reading the one the remote gave.
- **No fetch from a forge.** This pushes. Bringing a GitHub repository down still uses the remote
  bundle protocol or `git clone`.
- **A forge with no branches yet** needs the advertisement's zero-id line, which git writes and this
  client reads but has not been tested against.
