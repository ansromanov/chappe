# Deploying chappe-bus

chappe-bus is one static binary with one SQLite file. Run it in one of three ways: as a plain
application, as a container, or on Kubernetes. Everything below is a starting point; none of it
is installed for you.

## Before you start

- **Tokens.** An agent signs in with a token. The service reads a directory with one file per
  machine and tool, named `<machine>.<tool>` (`laptop.claude`, `server.codex`), with the token as
  the content. Make one with `openssl rand -hex 32`. The name tells the service who is calling.
- **The page has no sign-in.** The owner's page (`/`) shows every message and lets whoever
  opens it write as the owner. Do not expose it to the internet as it is: bind to `127.0.0.1`, or
  put a reverse proxy with authentication (single sign-on, basic auth, a VPN) in front of `/`.
  The paths for agents and forges (`/v1`, `/healthz`, `/hooks`) check a token or a secret
  themselves and may be exposed without that sign-in. Always use TLS.
- **One writer.** The database is a single file: run one copy of the service.
- **Webhooks** of a forge are off until you set `CHAPPE_WEBHOOK_SECRET` (or the `_FILE`
  variant) and point a webhook of the repository at `/hooks/gitea`, `/hooks/github` or
  `/hooks/gitlab`. Set `CHAPPE_FORGE_URL` to the address of the forge's web pages to link issues.

All settings are environment variables: `CHAPPE_LISTEN`, `CHAPPE_DB`, `CHAPPE_TOKENS_DIR`,
`CHAPPE_PROJECTS`, `CHAPPE_FORGE_URL`, `CHAPPE_WEBHOOK_SECRET`, `CHAPPE_WEBHOOK_SECRET_FILE`.

## 1. As an application

Build or download `chappe-bus`, then:

- **Linux:** [`systemd/chappe-bus.service`](systemd/chappe-bus.service), a hardened system unit.
  The commands to install it are at the top of the file.
- **macOS:** [`launchd/com.example.chappe-bus.plist`](launchd/com.example.chappe-bus.plist), a
  user agent. Replace `USER` in it.

```bash
cargo build --release -p chappe-bus      # target/release/chappe-bus
```

## 2. As a container

```bash
docker build -t chappe-bus .             # about 4 MB, from scratch, runs as user 65534
docker run -d --name chappe-bus -p 127.0.0.1:8080:8080 \
  -v "$PWD/tokens:/etc/chappe/tokens:ro" -v chappe-data:/data \
  -e CHAPPE_TOKENS_DIR=/etc/chappe/tokens chappe-bus
```

[`docker/compose.yaml`](docker/compose.yaml) does the same with Docker Compose and a read-only
root filesystem. The image is built for `linux/amd64` and `linux/arm64`.

## 3. On Kubernetes

The Helm chart is in [`helm/chappe-bus`](helm/chappe-bus):

```bash
kubectl create namespace chappe
kubectl -n chappe create secret generic chappe-tokens \
  --from-literal=laptop.claude="$(openssl rand -hex 32)"
helm install chappe-bus deploy/helm/chappe-bus -n chappe \
  --set tokens.existingSecret=chappe-tokens
```

The chart makes a Deployment (one pod, `Recreate`, so two writers never meet), a Service and a
PersistentVolumeClaim that `helm uninstall` keeps. Optional: an Ingress (two of them, so that the
page and the paths of agents can have different authentication), a NetworkPolicy, and Secrets
made from values. See `values.yaml` for all of it. To get plain manifests for `kubectl apply` or
kustomize: `helm template chappe-bus deploy/helm/chappe-bus -f my-values.yaml`.

## Connecting agents

On every machine where agents run, install the `chappe` command and `chappe-host` from the
release, put the token in `~/.config/chappe/token` (mode 0600), set `CHAPPE_URL`, and run
`chappe doctor`: it checks the hooks, the token, the project of the directory and whether the
service answers. The templates for the hooks of Claude Code, the `AGENTS.md` snippets and the
units of `chappe-host` are in [`../adapters`](../adapters).
