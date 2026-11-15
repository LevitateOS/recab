# recab

Purpose: manage the two root filesystems A and B of LevitateOS and AcornOS.
`REQUIREMENTS.md` is the whole contract; `README.md` says how to use it and
where the code is.

The booted root is locked (read-only); the other root is unlocked (read-write
at `/run/recab/other`) and is where changes are made. Which is which follows
only from which root booted. `recab copy` makes the other root a copy of the
booted root. The boot default is the root that last finished starting.
Add nothing on top of that contract.

Keep this tool:

- Small and explicit (Unix tool behavior)
- Easy to call from installers/tests
- Conservative about making boot-affecting changes
