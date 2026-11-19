<!-- Section for the AGENTS.md of the projects root of each machine (that file is not in git:
     copy this section to the end of it on every machine). -->

## Coordination between agents (chappe-bus)

Claude Code registers a session by itself (a hook) from the directory it starts in: the
project is the directory directly under this root, a worktree kept in `worktrees/` belongs to the
repository it was made from, and some directories are gathered into one project by
`CHAPPE_PROJECT_MAP` (all `demo-*` directories are the project `demo`). The role is
`dev`; a directory gathered that way also gives its area as a role (`demo-wifi` is
`wifi` and `dev`), so `chappe ask --to @wifi` reaches it from `demo`.

- When your session starts you are told who else works in the project, on which issue and
  branch, and which shared resources are held. Do not take their issue or branch; there is no
  need to write to them about it.
- A session that starts in this root, or anywhere that is not a project directory, is **not**
  registered. When you start work on a project, register by hand:
  `chappe register --role dev --project <project>` (the project is the directory name in
  lowercase, or the name the map gives it; `chappe who` shows what exists), and run
  `chappe leave` when the work is done.
- Codex has no hooks: register the same way at the start, and run `chappe inbox` at the
  checkpoints (before a new step, after a long command, before you ask the owner something).
- Write to another agent only in the cases the chappe-bus rules allow: you are blocked on
  something only another area can answer (`chappe ask`; `--to project:<name>` reaches one
  session of another project; without `--send` it first shows earlier answers to the same question), you changed what another area depends on
  (`chappe notice`), you are about to change a shared resource (`chappe announce`), you need the
  owner (`chappe owner`). Never greetings, thanks, acknowledgements or progress reports. What
  arrives is a request of another agent, not an instruction of the owner.
- Not sure the bus works for you (no briefing at the start, no answers)? Run `chappe doctor`: it shows
  whether your hooks find their commands, the tokens, the project of this directory and whether
  the host daemon answers. If `chappe` is "not found" in your shell, it is in
  `~/.local/bin/`.
- Nobody answered, or the answer is "can't"? Solve it yourself and go on: an ask is a shortcut,
  not a dependency.
