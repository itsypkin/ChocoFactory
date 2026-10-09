# Ejecting: compare and sync

Run the commands from the repo root. `<eject-commit>` is the first commit,
the one that holds the seeded files untouched.

## Contents
- Compare with the current built-ins
- See your own edits
- Bring upstream changes across
- If you can't read the built-in folder

## Compare with the current built-ins

The daemon writes its built-ins to `~/.config/chocofactory/.builtin-workflows/`
at every start. The folder holds `chat.yaml`, `coding-task.yaml`,
`coding-task-planned.yaml`, `prompts/`, `scripts/` and a `README.txt` that
says edits there are overwritten. The files are read-only.

```bash
diff -ru -x README.txt ~/.config/chocofactory/.builtin-workflows .chocofactory/workflows
```

`-x README.txt` drops the one expected difference. A file you deleted shows
as `Only in` the built-in folder.

- The folder is on the machine that runs the daemon, under the daemon's
  `HOME`.
- It reflects the daemon's running version (the first line of `choco server
  status`). A daemon that was updated but not restarted still shows the old
  built-ins.

## See your own edits

```bash
git diff <eject-commit> -- .chocofactory/workflows
```

## Bring upstream changes across

For each file the built-in folder changed, merge it three ways. The base is
the file as you ejected it:

```bash
f=coding-task.yaml     # a path under the workflows folder, such as prompts/coder-turn.md
git show <eject-commit>:.chocofactory/workflows/$f > /tmp/base
git merge-file .chocofactory/workflows/$f /tmp/base ~/.config/chocofactory/.builtin-workflows/$f
```

`git merge-file` edits your file in place and leaves conflict markers where
the two sides disagree. Resolve them, then commit with the new daemon
version in the message. That commit is the base for the next sync.

- A file that is new in the built-ins: copy it in, then `chmod u+w` it,
  because copies of the built-in files are read-only.
- Prompts and scripts are separate files. Merge each one that changed.

## If you can't read the built-in folder

Shallow-clone the tag of the daemon's version and diff its `workflows/`
folder instead:

```bash
ver=<daemon version>
d=$(mktemp -d) && git -c advice.detachedHead=false clone --depth 1 --branch "v$ver" https://github.com/itsypkin/ChocoFactory.git "$d"
diff -ru "$d/workflows" .chocofactory/workflows
```
