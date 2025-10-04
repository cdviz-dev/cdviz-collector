# ArgoCD Events Transformer

Transform ArgoCD notification webhooks into CDEvents.

## Overview

This transformer converts ArgoCD application lifecycle events (deployments, sync operations, health status) into standardized CDEvents following the [ArgoCD CDEvents Integration Proposal](https://raw.githubusercontent.com/argoproj/argo-cd/dd8d34c73869c8585fb57c56c6a5f3bec78ef081/docs/proposals/argocd-cdevents-integration.md).

The transformer uses a **simplified configuration approach**: one ArgoCD notification template for all event types, with event detection logic implemented in VRL. This keeps ArgoCD configuration minimal while providing full transformation flexibility.

### Architecture

Two chained transformers process ArgoCD webhooks:

1. **argocd_metadata**: Injects `environment_id` from `app.spec.destination.server`
2. **argocd_notifications**: Detects event type and transforms to CDEvents

### Event Mapping

Event type detection is performed in VRL based on payload fields:

| ArgoCD Condition            | CDEvent Type      | Detection Logic                                              |
| --------------------------- | ----------------- | ------------------------------------------------------------ |
| Application deleted         | service.removed   | `metadata.deletionTimestamp` exists                          |
| Sync succeeded + Healthy    | service.deployed  | `operationState.phase=Succeeded` AND `health.status=Healthy` |
| Sync failed/error           | incident.detected | `operationState.phase` in [Failed, Error]                    |
| Health degraded             | incident.detected | `health.status` in [Degraded, Missing, Unknown]              |
| Health healthy (standalone) | service.deployed  | `health.status=Healthy` (no operation state)                 |
| Sync running                | (skipped)         | `operationState.phase=Running`                               |

### ArtifactId Generation

For `service.deployed` events, artifactId is generated from the ArgoCD application's source:

- **Helm Chart**: `pkg:helm/<chart>@<version>?repository_url=<encoded_url>`
- **Git Repository**: `pkg:git/<app_name>@<revision>?repository_url=<encoded_url>`

The transformer uses `operationState.syncResult.sources` (completed sync) or falls back to `spec.source` (other states).

## Capturing Real ArgoCD Webhook Payloads

To capture authentic ArgoCD webhook payloads for testing:

### 1. Set Up webhook.site

1. Visit [webhook.site](https://webhook.site)
2. Copy your unique URL (e.g., `https://webhook.site/abc123-def456-...`)
3. Keep the browser tab open to monitor incoming webhooks

### 2. Configure ArgoCD Notifications

#### Step 1: Create Webhook Service

Edit the `argocd-notifications-cm` ConfigMap:

```bash
kubectl edit configmap argocd-notifications-cm -n argocd
```

Add webhook service configuration:

```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: argocd-notifications-cm
  namespace: argocd
data:
  # For testing: register webhook.site as a webhook service
  service.webhook.webhook-site: |
    url: https://webhook.site/YOUR-UNIQUE-ID
    headers:
    - name: Content-Type
      value: application/json

  # For production: register your cdviz-collector endpoint
  service.webhook.cdviz: |
    url: https://your-cdviz-collector.example.com/webhook/000-argocd
    headers:
    - name: Content-Type
      value: application/json
    # Optional: Add authentication
    # - name: Authorization
    #   value: Bearer YOUR-TOKEN
```

#### Step 2: Create Unified Notification Template

Add **one template** for all event types (event detection happens in VRL):

```yaml
# Unified template that sends full application state for all events
template.webhook-cdviz: |
  webhook:
    cdviz:  # or webhook-site for testing
      method: POST
      body: |
        {
          "timestamp": "{{(call .time.Now).Format \"2006-01-02T15:04:05.000000-07:00\"}}",
          "context": {{toJson .context}},
          "app": {{toJson .app}}
        }
```

**Key points**:

- Single template for all events keeps ArgoCD configuration simple
- Event type detection is handled by VRL transformer logic
- Full `.app` object provides all necessary state for transformation

#### Step 3: Configure Triggers

Add triggers to send webhooks on status changes:

```yaml
# Trigger on any sync operation state change
trigger.on-sync-status-unknown: |
  - when: app.status.operationState.phase in ['Running', 'Failed', 'Error', 'Succeeded']
    send: [webhook-cdviz]

# Trigger on health status changes
trigger.on-health-status: |
  - when: app.status.health.status != nil
    send: [webhook-cdviz]

# Trigger on application deletion
trigger.on-app-deleted: |
  - when: app.metadata.deletionTimestamp != nil
    send: [webhook-cdviz]
```

**Alternative**: Use `defaultTriggers` for automatic notifications:

```yaml
# Apply to all applications by default
defaultTriggers: |
  - on-sync-status-unknown
  - on-health-status
  - on-app-deleted
```

#### Step 4: Subscribe Application to Notifications

**Option A**: Use default triggers (recommended)

If you configured `defaultTriggers` above, all applications automatically send notifications. No per-app configuration needed.

**Option B**: Per-application subscription

Add annotations to specific applications:

```bash
kubectl annotate app <app-name> -n argocd \
  notifications.argoproj.io/subscribe.on-sync-status-unknown.cdviz="" \
  notifications.argoproj.io/subscribe.on-health-status.cdviz="" \
  notifications.argoproj.io/subscribe.on-app-deleted.cdviz=""
```

Or in Application manifest:

```yaml
apiVersion: argoproj.io/v1alpha1
kind: Application
metadata:
  name: my-app
  namespace: argocd
  annotations:
    notifications.argoproj.io/subscribe.on-sync-status-unknown.cdviz: ""
    notifications.argoproj.io/subscribe.on-health-status.cdviz: ""
    notifications.argoproj.io/subscribe.on-app-deleted.cdviz: ""
spec:
# ... your application spec
```

#### Test template on cli

see [Troubleshooting commands - Notifications](https://argo-cd.readthedocs.io/en/stable/operator-manual/notifications/troubleshooting-commands/)

```bash
# after login
# argocd login ARGOCD_HOST --grpc-web-root-path / --username admin --grpc-web
# argocd admin notifications template notify NAME RESOURCE_NAME [flags]
argocd admin notifications template notify webhook-cdviz podinfo -n argocd
```

```txt
webhook:
  cdviz:
    body: |
      {
        "timestamp": "2025-10-02T17:45:00.442161+02:00",
        "context": {"argocdUrl":"https://demo.cdviz.dev","notificationType":"console"},
        "app": {"apiVersion":"argoproj.io/v1alpha1","kind":"Application","metadata":{"annotations":{"argocd.argoproj.io/tracking-id":"bootstrap:argoproj.io/Application:argocd/podinfo"},"creationTimestamp":"2025-10-02T15:32:45Z","generation":2,"labels":{"app.kubernetes.io/managed-by":"argocd","cdviz.dev/cluster":"demo-cluster"},"managedFields":[{"apiVersion":"argoproj.io/v1alpha1","fieldsType":"FieldsV1","fieldsV1":{"f:metadata":{"f:annotations":{"f:argocd.argoproj.io/tracking-id":{}},"f:labels":{"f:app.kubernetes.io/managed-by":{},"f:cdviz.dev/cluster":{}}},"f:spec":{"f:destination":{"f:namespace":{},"f:server":{}},"f:project":{},"f:revisionHistoryLimit":{},"f:sources":{},"f:syncPolicy":{"f:automated":{"f:prune":{},"f:selfHeal":{}},"f:retry":{"f:backoff":{"f:duration":{},"f:factor":{},"f:maxDuration":{}},"f:limit":{}},"f:syncOptions":{}}}},"manager":"argocd-controller","operation":"Apply","time":"2025-10-02T15:32:45Z"},{"apiVersion":"argoproj.io/v1alpha1","fieldsType":"FieldsV1","fieldsV1":{"f:status":{".":{},"f:conditions":{},"f:health":{".":{},"f:lastTransitionTime":{},"f:status":{}},"f:sync":{".":{},"f:status":{}}}},"manager":"argocd-application-controller","operation":"Update","time":"2025-10-02T15:32:45Z"}],"name":"podinfo","namespace":"argocd","resourceVersion":"70725964","uid":"648b3973-e90b-4496-84c0-febef8536eec"},"spec":{"destination":{"namespace":"podinfo","server":"https://kubernetes.default.svc"},"project":"demo-cluster","revisionHistoryLimit":3,"sources":[{"chart":"podinfo","repoURL":"oci://ghcr.io/stefanprodan/charts/","targetRevision":"6.9.0"}],"syncPolicy":{"automated":{"prune":true,"selfHeal":true},"retry":{"backoff":{"duration":"5s","factor":2,"maxDuration":"3m"},"limit":5},"syncOptions":["CreateNamespace=true","ServerSideApply=true","ApplyOutOfSyncOnly=true"]}},"status":{"conditions":[{"lastTransitionTime":"2025-10-02T15:32:45Z","message":"application repo oci://ghcr.io/stefanprodan/charts/ is not permitted in project 'demo-cluster'","type":"InvalidSpecError"}],"health":{"lastTransitionTime":"2025-10-02T15:32:45Z","status":"Unknown"},"sync":{"status":"Unknown"}}}
      }
    method: POST
    path: ""
```

Check trigger run

```bash
# argocd admin notifications trigger run NAME RESOURCE_NAME [flags]
argocd admin notifications trigger run on-sync-status-unknown podinfo
```

If you have the error `failed to get api: secrets "argocd-notifications-secret" not found` it's caused by searching the secrets (& the app) in the default namespace, use `-n ...` to specify the namespace.

### 3. Trigger Events

Perform actions to generate webhook events:

```bash
# Trigger a sync
argocd app sync <app-name>

# Force a sync (might cause failures for testing)
argocd app sync <app-name> --force

# Delete an application (for testing removal)
argocd app delete <app-name>
```

### 4. Capture Webhook Payloads

1. Go to your webhook.site browser tab
2. Click on each webhook request to view the full payload
3. Copy the JSON body
4. Save to `inputs/captured/` directory with descriptive names

Example file naming:

- `inputs/captured/000_template_test.json` - CLI template test output
- `inputs/captured/001_sync_running.json` - Sync operation started
- `inputs/captured/012_sync_succeeded.json` - Successful deployment
- `inputs/captured/015_sync_failed.json` - Failed deployment
- `inputs/captured/018_health_degraded.json` - Health issues detected
- `inputs/captured/021_app_deleted.json` - Application deletion

### 5. Test Transformer with Real Data

Once you have real webhook payloads:

```bash
# Generate expected outputs (overwrites existing outputs)
cd transformers/argocd_notifications
../../target/debug/cdviz-collector transform \
  --mode overwrite \
  --directory . \
  --config ./cdviz-collector.toml \
  -t argocd_metadata,argocd_notifications \
  --input ./inputs/captured \
  --output ./outputs/captured

# Or use mise task for check mode
mise run test:transform:argocd_notifications
```

**Note**: Some inputs may not generate outputs (e.g., sync running events are skipped). This is expected behavior.

## Testing

### Run Transformer Tests

```bash
# Check transformation against expected outputs (validates CDEvents)
mise run test:transform:argocd_notifications

# Generate/overwrite expected outputs (after adding new inputs)
cd transformers/argocd_notifications
../../target/debug/cdviz-collector transform \
  --mode overwrite \
  --directory . \
  --config ./cdviz-collector.toml \
  -t argocd_metadata,argocd_notifications \
  --input ./inputs/captured \
  --output ./outputs/captured
```

### Test with Local Server

Configure collector to accept ArgoCD webhooks (see example in `cdviz-collector.toml`), then start:

```bash
mise run run
```

Send test events:

```bash
# Using curl with a captured webhook
curl -X POST http://localhost:8080/webhook/000-argocd \
  -H "Content-Type: application/json" \
  -d @transformers/argocd_notifications/inputs/captured/012_sync_podinfo_2_2.json
```

## Transformer Configuration

The transformer is configured via `cdviz-collector.toml` with **two chained transformers**:

```toml
# Metadata transformer injects environment_id from ArgoCD destination
[transformers.argocd_metadata]
type = "vrl"
template = """
.metadata = object(.metadata) ?? {}

[{
  "metadata": merge(.metadata, {
    "environment_id": .body.app.spec.destination.server || "unknown",
  }),
  "headers": .headers,
  "body": .body,
}]
"""

# Main transformer detects event type and transforms to CDEvents
[transformers.argocd_notifications]
type = "vrl"
template_file = "./transformer.vrl"
```

**Webhook source configuration** (in main `cdviz-collector.toml`):

```toml
[sources.argocd_webhook]
enabled = true
transformer_refs = ["argocd_metadata", "argocd_notifications"]

[sources.argocd_webhook.extractor]
type = "webhook"
id = "000-argocd"
headers_to_keep = []

[sources.argocd_webhook.extractor.headers]
# Optional: Verify webhook authenticity with signature
# "authorization" = { type = "signature", signature_encoding = "base64", signature_on = "body", signature_prefix = "Bearer ", token = "changeme" }
```

## VRL Transformation Logic

Key transformation rules (aligned with kubewatch transformer):

- **Transformer chaining**: `argocd_metadata` → `argocd_notifications`
- **Event detection**: VRL logic determines event type from payload fields
- **context.id**: Auto-generated by collector (set to "0")
- **context.source**: `/argocd` (following kubewatch pattern)
- **subject.id**: `namespace/app_name` format
- **subject.content.artifactId**: PURL for Helm chart or Git source
- **environment.id**: Injected by argocd_metadata from `app.spec.destination.server`
- **customData.argocd**: Preserves ArgoCD-specific operation, health, and sync details

## Troubleshooting

### Webhooks Not Received

1. Verify ArgoCD notifications controller is running:

   ```bash
   kubectl get pods -n argocd | grep notifications
   ```

2. Check notifications controller logs:

   ```bash
   kubectl logs -n argocd -l app.kubernetes.io/name=argocd-notifications-controller
   ```

3. Verify webhook service configuration:

   ```bash
   kubectl get configmap argocd-notifications-cm -n argocd -o yaml
   ```

4. Check application annotations:
   ```bash
   kubectl get app <app-name> -n argocd -o yaml | grep notifications
   ```

### Invalid Webhook Payloads

If webhook.site shows errors:

1. Verify JSON syntax in templates using `{{toJson .app}}`
2. Check for proper escaping of special characters
3. Test templates with `argocd admin notifications template notify` command

## References

- [ArgoCD Notifications Documentation](https://argo-cd.readthedocs.io/en/stable/operator-manual/notifications/)
- [ArgoCD Webhook Service](https://argo-cd.readthedocs.io/en/latest/operator-manual/notifications/services/webhook/)
- [ArgoCD CDEvents Integration Proposal](https://raw.githubusercontent.com/argoproj/argo-cd/dd8d34c73869c8585fb57c56c6a5f3bec78ef081/docs/proposals/argocd-cdevents-integration.md)
- [CDEvents Specification](https://cdevents.dev/)
- [webhook.site](https://webhook.site) - Free webhook testing tool

## Sample Webhook Payloads

The `inputs/captured/` directory contains real ArgoCD webhook payloads captured during testing:

- `000_cli_template.json` - Result of `argocd admin notifications template notify webhook-cdviz podinfo -n argocd`
- `001-006.json` - Initial events after ArgoCD notifications configuration
- `007-010.json` - Events during failed deployment (PodSecurity violations)
- `011-013.json` - Events during failed deployment (value type errors)
- `014-017.json` - Events from successful deployment (Podinfo 3.3)
- `018-020.json` - Events from successful upgrade (Podinfo 4.2)
- `021.json` - Events from application deletion via Git
- `022.json` - Events from manual prune operation

## Design Decisions

### Why One Template Instead of Multiple?

**Simplified ArgoCD Configuration**: One template for all event types keeps ArgoCD configuration minimal and maintainable. Event type detection is centralized in VRL logic where it can be versioned, tested, and evolved independently.

**Benefits**:

- Easier to maintain: single source of truth for webhook payload format
- Simpler troubleshooting: all events have identical structure
- Flexible event detection: VRL can implement complex logic based on multiple fields
- Easier testing: one payload format to validate

### Why Application-Level Events Instead of Per-Container?

**ArgoCD Limitation**: Unlike Kubewatch (which monitors Kubernetes resources directly), ArgoCD webhooks provide application-level state without detailed container/image information. The webhook payload includes:

- ✅ Application source (Helm chart, Git repository, revision)
- ✅ Sync operation state and results
- ✅ Overall health status
- ❌ Individual container images and digests
- ❌ Pod-level deployment details

**Result**: Transformer emits one CDEvent per ArgoCD Application (using Helm chart or Git repo as artifactId) rather than per-container events like Kubewatch.

### Why Chained Transformers?

**Separation of Concerns**:

- `argocd_metadata`: Infrastructure-level metadata injection (environment_id from destination cluster)
- `argocd_notifications`: Business logic for event type detection and CDEvent generation

This pattern follows the Kubewatch transformer design and allows each transformer to focus on a single responsibility.
