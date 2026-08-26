# filesnap

Git-free file snapshots and rewind: capture what a directory holds at a moment
you name, and put it back later — without a repository, and without touching
your version control.

```console
npm install -g filesnap
```

This package is a thin launcher. The binary for your platform arrives as an
optional dependency, so the install downloads one build rather than six.

```console
$ filesnap capture --session s1 --turn t1
{"v":1,"type":"capture.done","manifest":"a5a2b149…","reused":0,"hashed":1,"dropped":0}

$ echo "something regrettable" > a.txt

$ filesnap restore --session s1 --turn t1
{"v":1,"type":"restore.done","written":1,"deleted":0,"failed":0,"safety":"e41358f0…"}
```

That `safety` id is the point the rewind itself can be rewound to. Every
restore captures one before it writes anything.

**Output is JSON Lines on stdout, prose on stderr**, and the exit code is part
of the contract: `0` everything happened, `1` it ran and reported but not
everything happened, `2` it did not run, `3` the arguments were wrong.

Nine commands — `capture`, `declare`, `log`, `restore`, `undo`, `delete`,
`gc`, `status`, `doctor`. The full surface, the event contract, and what it
deliberately refuses to do are in the
[documentation](https://github.com/extracurricular-ai/filesnap#readme).

## Platforms

Linux, macOS and Windows, on x64 and arm64. The Linux builds are statically
linked against musl, so they do not carry a glibc floor from the machine that
built them.

Not on this list? `cargo install filesnap-cli` builds the same command from
source.

## Licence

Apache-2.0.
