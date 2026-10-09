# Ejecting: compare and sync

Run the commands from the repo root. `<eject-commit>` is the first commit,
the one that holds the seeded files untouched. The merge base for a sync is
always the untouched built-in of the version you last synced to (the
eject commit for the first sync), never a commit that holds your edits.

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

Before your first sync this shows only your edits. After a sync it also shows
the upstream changes you merged.

## Bring upstream changes across

For each file the built-in folder changed, merge it three ways. The base is
the untouched built-in of the version you last synced to. For the first sync
that is the eject commit. After a sync, a commit holds your edits merged with
upstream, so it is no longer a base: take the base from a clone of the tag of
the version you last synced to (see the last section) instead, or merge with a
base that already holds your edits and `git merge-file` would take the new
built-in wholesale and silently drop them.

```bash
f=coding-task.yaml     # a path under the workflows folder, such as prompts/coder-turn.md
base=$(mktemp)
git show <eject-commit>:.chocofactory/workflows/$f > "$base"   # first sync
# later syncs: cp "$d/workflows/$f" "$base"   ($d: clone of the last-synced tag)
git merge-file .chocofactory/workflows/$f "$base" ~/.config/chocofactory/.builtin-workflows/$f
```

`git merge-file` edits your file in place and leaves conflict markers where
the two sides disagree. Resolve them, then commit with the new daemon
version in the message, so the next sync knows which tag to take as its base.

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
