# Squashing (baselines)

Squashing needs Docker.

`squash [--through <id>]` replaces every migration up to and including `<id>`
(all of them by default) with one `<timestamp>_baseline.star` that creates
their final state, and deletes the replaced files. For each environment it
folds the history (deletes drop what they remove, recreated resources keep
their last config), then proves in Docker that the history and the baseline
produce identical streams, topics and groups before writing anything.

The baseline records each replaced migration's checksum. On `up`, a server
that has:

- **applied all of them** marks the baseline applied without running it;
- **applied none of them** runs the baseline like any migration;
- **applied only some** is refused (`PARTIAL` in `status`). Apply the rest
  from the commit before the squash, e.g. by pointing `--dir` at a checkout
  of it, then run `up` again.

So before squashing, run `up` on every long-lived server if you want them all
to take the fast path. Baselines can themselves be squashed later: the new
baseline carries the old one's replaced migrations along, so servers that are
several squashes behind are still recognised. Don't edit generated baselines.
