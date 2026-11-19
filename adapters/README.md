# Adapters

Templates for connecting a tool to chappe-bus. Nothing here is installed by the repository:
copying a template into a project or onto a machine is a decision of its owner.

| Path | For |
| --- | --- |
| `claude/settings.json` | `.claude/settings.json` of a project: the hooks of Claude Code (each command sets its own `PATH`, because a session not started from a terminal has no `~/.local/bin`) and the project and roles of its sessions |
| `codex/AGENTS.snippet.md` | the `AGENTS.md` of a project whose agents work in Codex (no hooks there) |
| `projects/AGENTS.section.md` | the end of the `AGENTS.md` of the projects root of each machine (not in git: copy it there) |
| `host/chappe-host.service`, `host/com.example.chappe-host.plist` | the user service of `chappe-host` on Linux and on macOS |
