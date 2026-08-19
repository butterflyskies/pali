# Deployment

For the deployment trust boundary and hardening controls, see
[Security](security.md).

## Quick start (any Kubernetes cluster)

The manifests in `deploy/k8s/` are generic and work on any cluster.

### 1. Build and push the image

```bash
docker build -t YOUR_REGISTRY/memory-mcp:latest .
docker push YOUR_REGISTRY/memory-mcp:latest
```

Or let the [GitHub Actions workflow](../.github/workflows/build.yml) do it on
push to `main`. The workflow pushes to `ghcr.io/butterflyskies/memory-mcp`
using the built-in `GITHUB_TOKEN` — no extra secrets needed.

### 2. Apply the manifests

```bash
# Create namespace, RBAC, PVC, and Service
kubectl apply -f deploy/k8s/namespace.yml
kubectl apply -f deploy/k8s/rbac.yml
kubectl apply -f deploy/k8s/pvc.yml
kubectl apply -f deploy/k8s/service.yml
```

### 3. Create the GitHub token Secret

Option A — use the built-in auth subcommand. Run from a host with kubeconfig
access, or as a one-off Kubernetes Job using the `memory-mcp-bootstrap`
ServiceAccount (see `deploy/k8s/rbac.yml`):

```bash
pali auth login \
  --store k8s-secret \
  --k8s-namespace memory-mcp \
  --k8s-secret-name memory-mcp-github-token
```

Option B — apply the template manually after filling in the token:

```bash
# Replace <TOKEN> with your GitHub personal access token (repo scope)
kubectl create secret generic memory-mcp-github-token \
  --namespace memory-mcp \
  --from-literal=token=<TOKEN>
```

### 4. Deploy

Edit `deploy/k8s/deployment.yml`:
- Set `image:` to your registry/image:tag
- Uncomment and set `MEMORY_MCP_REMOTE_URL` to your private GitHub repository URL
- Uncomment the `MEMORY_MCP_GITHUB_TOKEN` secretKeyRef block (required when using a remote)

Then apply:

```bash
kubectl apply -f deploy/k8s/deployment.yml
```

### 5. Expose externally

The Service is ClusterIP only. Add an Ingress or Gateway API HTTPRoute to
expose the server outside the cluster. TLS should terminate at the gateway;
the binary serves plain HTTP on port 8080.

Example with a basic Ingress:

```yaml
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: memory-mcp
  namespace: memory-mcp
spec:
  rules:
    - host: memories.example.com
      http:
        paths:
          - path: /
            pathType: Prefix
            backend:
              service:
                name: memory-mcp
                port:
                  number: 8080
```

### 6. Connect Claude Code

Add to `~/.claude.json` (or your project-local `.mcp.json`):

```json
{
  "mcpServers": {
    "memory": {
      "type": "http",
      "url": "https://memories.example.com/mcp"
    }
  }
}
```

---

## Health probes

The server exposes `/healthz` (liveness) and `/readyz` (readiness) on the same
port as the MCP endpoint. Configure Kubernetes probes:

```yaml
livenessProbe:
  httpGet:
    path: /healthz
    port: 8080
  initialDelaySeconds: 5
  periodSeconds: 10
readinessProbe:
  httpGet:
    path: /readyz
    port: 8080
  initialDelaySeconds: 5
  periodSeconds: 10
  failureThreshold: 3
```

`/readyz` returns 200 when all subsystems (git repo, embedding engine, vector
index) are operational, and 503 with per-subsystem detail when any is degraded.
Subsystems report health passively during normal operations — the probe does no
active checking.

### Sync readiness

If `--require-remote-sync` is set, the server performs an initial pull at startup
and includes sync health in readiness checks. Push/pull failures will cause
`/readyz` to return 503. Requires `--remote-url` to be configured.

### Staleness detection

Set `--health-stale-secs` (e.g. `300`) to flag subsystems that haven't performed
a successful operation within the threshold. Disabled by default (`0`) — enable
only for deployments with consistent traffic where silence indicates a problem.

---

## Operational notes

### Resource sizing

- **Memory**: ~300MB baseline (model weights + index). Grows with memory
  count — plan for 512MB request, 1Gi limit as a starting point.
- **CPU**: Embedding computation is CPU-bound. Single-query latency is
  ~50ms on modern hardware. 250m request, 1 core limit.
- **Disk**: The git repo + HF model cache. The BGE-small-en-v1.5 model
  weighs ~130MB; each memory is a small markdown file (~1–5KB). 1Gi is
  comfortable for thousands of memories. Scale the PVC if your repo
  grows significantly or if you switch to a larger embedding model.

### Model weights mmap safety

The embedding model is memory-mapped from the HuggingFace cache. If the
cache is mutated while the server is running (e.g., by `huggingface-cli`),
the mmap'd region becomes undefined. The container image pre-warms the
model so this is only a concern if the cache volume is shared. Keep
`HF_HOME` on a volume dedicated to the pod.

### Backup

The git repo IS the backup — it syncs to a GitHub remote. If the PVC is
lost, re-clone from the remote. The vector index is rebuilt from the repo
on startup.
