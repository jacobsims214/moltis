# moltis Helm chart

Runs one isolated Moltis agent server per person. A single release manages a
whole household or team: every entry under `instances` gets its own
Deployment, Service, PVC, seeded `moltis.toml`, password and (optionally)
IngressRoute and NetworkPolicy, on top of shared `defaults`.

```yaml
defaults:
  timezone: America/New_York
  llm:                       # any OpenAI-compatible endpoint (llama.cpp, vLLM, …)
    name: muse               # → provider "custom-muse"
    baseUrl: http://192.168.1.204:8000/v1
    models: [Muse-Glimmer-30B]
    contextWindows:
      Muse-Glimmer-30B: 131072
  persistence:
    storageClass: nfs-storage

instances:
  jacob:
    displayName: Jacob
    terminal: true           # web UI shell (inside this instance's pod)
    password:
      existingSecret: moltis-jacob-password
  sam:
    displayName: Sam
    password:
      existingSecret: moltis-sam-password
    resources:
      limits:
        memory: 2Gi
```

```bash
kubectl -n moltis create secret generic moltis-jacob-password \
  --from-literal=password="$(openssl rand -base64 18)"
helm upgrade --install moltis charts/moltis -n moltis -f my-values.yaml
kubectl -n moltis port-forward svc/moltis-jacob 13131:13131   # http://localhost:13131
```

## How it maps onto Moltis

| Concern | Chart behaviour |
|---|---|
| Auth | `MOLTIS_PASSWORD` from the instance Secret becomes the initial password (≥ 12 chars) the first time the instance starts; change it or add a passkey in Settings afterwards. |
| TLS | Moltis runs with `--no-tls` and `MOLTIS_BEHIND_PROXY=true`; terminate TLS at your ingress. |
| Config | `moltis.toml` is rendered per instance but only **seeds** an empty volume, because Moltis edits it from the UI. `configOverwrite: true` replaces it on every start. |
| LLM | `defaults.llm` renders a `custom-<name>` OpenAI-compatible provider; `exclusive: true` hides every other provider. |
| Sandbox | Kubernetes has no Docker socket to share, so `sandboxBackend: restricted-host` runs agent commands inside the instance's own pod (env cleared, rlimits). The pod is the isolation boundary: it runs as uid 1000 with no privilege escalation (the image's passwordless sudo is inert), all capabilities dropped and RuntimeDefault seccomp. Shell commands still need approval in the UI. |
| Storage | One PVC per instance with `config/`, `data/` and `npm/` subPaths. Moltis uses SQLite, so the Deployment uses `Recreate` and never runs two pods on a volume. PVCs are kept on uninstall (`persistence.retain`). |
| Removing someone | `enabled: false` scales the instance to zero and keeps its data. |

## Web search and browsing (`webTools`)

```yaml
defaults:
  webTools:
    searxng: { enabled: true }      # meta search + MCP (mcp__searxng__*)
    playwright: { enabled: true }   # headless Chromium MCP (mcp__playwright__*)
```

Both run as **native sidecars** in each instance's pod, bound to the pod's
loopback (`127.0.0.1:8080` SearXNG, `:3010` its MCP, `:8931` Playwright), so
nothing new is exposed. Kubernetes starts them before Moltis and waits for
their startup probes, so the MCP connections never race the servers.

- SearXNG uses `files/searxng-settings.yml`: JSON API on, limiter off, a broad
  engine mix (Google, Bing, DuckDuckGo, Brave, Startpage, Mojeek, Qwant,
  Wikipedia, GitHub, Stack Overflow, Reddit) plus image and news engines.
  Engines fail independently, so the mix is what keeps results flowing.
  Override with `webTools.searxng.settings`.
- Enabling either adds its MCP server to the instance (merged into an existing
  list, never removing UI-added servers), denies Moltis's built-in
  `web_search` / `browser` tools, and appends search → read → browse guidance
  to `TOOLS.md` once.
- Image results are given as links: Moltis's CSP doesn't load external images
  inline (which also blocks image-URL data exfiltration).
- Resources: SearXNG ~200 MiB, its MCP ~64 MiB, Playwright 0.4–2 GiB while
  browsing; per instance.

## Run limits (why a turn can't run for 10 minutes)

```yaml
defaults:
  tools:
    agentMaxIterations: 10    # effective 30 iterations (lazy mode multiplies by 3)
    agentTimeoutSecs: 300     # wall clock; 0 = no timeout
```

With no chart value, instances inherited the engine defaults: 25 iterations
(**75 in practice**, see below) and 600 s. A war-game round hit both — one turn
spent ~590 s on a degenerate `memory_search` tail after the answers were
already gathered, and another spent 580 s on a *single* LLM stream.

- **`agentMaxIterations` is a base, not the effective limit.** Moltis
  multiplies it by 3 when `tools.registryMode` is `lazy`
  (`crates/agents/src/runner/streaming.rs:112`, `non_streaming.rs:124`), which
  this chart defaults to. The startup log prints the base
  (`resolved agent runtime limits max_iterations = 10`), so a run that dies at
  iteration 30 looks like a bug. It isn't.
- **`agentTimeoutSecs` is the only bound on a slow stream.** The iteration cap
  cannot catch a run that is slow because one completion took minutes.
- **Both are exported as `MOLTIS_TOOLS__AGENT_*` env vars as well as rendered
  into `[tools]`.** Moltis applies `MOLTIS_*` overrides *after* loading the
  config file, so the env var also binds an instance whose `moltis.toml` was
  seeded by an earlier chart version (with `configOverwrite: false`, which is
  the default, a rendered-only value never reaches an existing volume).
- **Hitting either limit costs the answer.** The iteration limit is
  recoverable — the UI offers "continue" — while the timeout returns
  `Timed out` and no text, so raise `agentTimeoutSecs` (or set `0`) if real
  turns start hitting it. 300 s is a deliberate ceiling: an answer that lands
  at 590 s has already lost its reader.
- Either value can be set per instance under `instances.<name>.tools`.
- **These two are enforced, not suggested:** because the env var is applied
  after the config file loads, changing them in the Moltis UI does not stick —
  the chart value wins at every start. That is deliberate for a cap whose whole
  purpose is to be absent from the model's decision loop, but it means an
  operator who needs a different limit changes it here and redeploys.

## Locking it down

```yaml
defaults:
  llm:
    apiKeySecret: { name: moltis-llm-api-key, key: api-key }
  networkPolicy:
    enabled: true
    restrictIngress: false        # true when only reached through Traefik
    egress:
      allow:
        - cidr: 192.168.1.204/32  # the LLM host
          ports: [8000]
```

- **Egress** (`networkPolicy.egress`, on whenever `networkPolicy.enabled`):
  DNS and the public internet only. Your LAN, the cluster and link-local /
  metadata addresses are blocked, so a prompt-injected web page can't point
  the agent's browser or shell at internal services. Open what an instance
  needs with `allow`.
- **LLM key**: create the Secret without a trailing newline
  (`openssl rand -hex 32 | tr -d '\n' > key`); a newline breaks the TOML
  string the key is substituted into and Moltis falls back to no providers.
- **Update check**: `updateCheck: false` (default) stops Moltis polling
  moltis.org for releases.
- Sidecar images are pinned by digest; bump them deliberately.

See `values.yaml` for every option.
