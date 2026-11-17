# HTTP API

The authenticated API is available under `/api` on the bridge's HTTP listener. Send `http.token` as a bearer token. Inventory is available with the standard bridge configuration. Use the optional `api` section for cached previews, video profiles, restricted credentials, and enrollment.

```json5
api: {
  // Optional scoped alternative to http.token.
  serviceToken: "${BRIDGE_API_SERVICE_TOKEN}",
  // Omit adminToken/storageDirectory for viewing and controls only.
  adminToken: "${BRIDGE_API_ADMIN_TOKEN}",
  storageDirectory: "./local-only/vault",
  // Omit cameraIds on a dedicated bridge, or list allowed camera IDs.
  cameraIds: ["entrance", "new-camera"],
  commands: true,
  // events: true, // Allow this scoped token to subscribe via WebSocket.
  targets: {
    staging: { host: "192.0.2.2", port: 9000, cameraId: "new-camera" },
  },
  media: {
    entrance: { port: 554, streams: ["main", "sub"] },
  },
  previews: {
    cameras: { entrance: { intervalMs: 5000 } },
  },
}
```

Replace the documentation address with a private IPv4 target from the installation network. Configure restricted egress; enrollment accepts only configured RFC1918, link-local or loopback addresses. Use private TLS, a dedicated admin token, and a directory protected with mode 0700. Store the vault on a persistent local filesystem with atomic rename and durable sync. The bridge stages and verifies credentials in this directory. API callers never receive camera credentials. Files are mode 0600. Back up the vault as sensitive material separately from the bridge configuration.

| Method and path under `/api` | Credential | Behavior |
| --- | --- | --- |
| GET `/contract` | HTTP or scoped service token | Contract version and configured inventory, settings, media, callback, and enrollment capabilities. |
| GET `/inventory` | HTTP or scoped service token | Registered camera IDs within scope, including devices paused for credential recovery. |
| POST `/cameras/{id}/command/exec` | HTTP or scoped service token with `commands` | Typed command. [Camera controls](./controls.md) |
| GET/POST `/callbacks` | Callback access | List or add callback delivery rules. |
| PUT/DELETE `/callbacks/{id}` | Callback access | Update or delete a rule. Use the same ID in the URL and the update body. |
| GET `/cameras/{id}/settings/{group}` | HTTP or scoped service token | Fresh allowlisted setting values for `leds`, `spotlight`, `motion`, or `ai`, without raw vendor XML. Writes use the existing typed command interface. |
| GET/HEAD `/cameras/{id}/preview` | HTTP or scoped service token | Latest cached JPEG or its headers. Supports `If-None-Match` and returns `304` when the image is unchanged. |
| GET `/cameras/{id}/media/{stream}` | HTTP or scoped service token | Bounded multipart MJPEG stream from configured H.264 RTSP/TCP. FFmpeg is required in the private bridge environment. The RTSP client owns credentials; FFmpeg receives elementary video bytes. |
| GET `/enrollment-targets` | admin | Allowlisted target names. |
| POST `/enrollments` | admin | Reserve and begin a UUID operation with target, separate `uid` and/or `serial`, `method` (`initialize` or `import`), and `confirmation: true`. First-password flow requires a UID. |
| GET `/enrollments/{id}` and POST `/enrollments/{id}/resume` | admin | Read or continue the persisted operation. Use `{"method":"import"}` after the local setup fallback. |
| GET/POST `/cameras/{id}/rotation` and POST `/cameras/{id}/rotation/reconcile` | admin | Check status, stage and submit one verified-device change, or verify the staged and previous credentials. POST requires UUID `id` and `confirmation: true`. |

## Background camera previews

Set `api.previews.cameras.<cameraId>.intervalMs` for each camera you want polled. An empty camera entry uses 5000 ms. The range is 500 to 2147483647 ms. The bridge starts the interval after each capture completes. It obtains JPEGs with the same read-only Baichuan snapshot operation used for on-demand MQTT captures. Polling runs independently of viewers.

`GET /api/cameras/{id}/preview` reads the latest successful frame from the cache. Use `HEAD` for just the headers or send the previous quoted `ETag` as `If-None-Match` to get a body only when the JPEG changes. `X-Preview-Captured-At` records when the bridge received the image. Use `GET .../preview?refresh=true` to request a fresh capture. The bridge joins overlapping captures for the same camera and returns `503` while the cache is cold or a requested refresh fails.

The cache defaults to memory. To reload the last JPEG after restart, add the following under `api.previews`. Use a private Valkey endpoint and a distinct key prefix for each bridge deployment.

```json5
cache: { backend: "valkey", url: "${BRIDGE_VALKEY_URL}", keyPrefix: "reolink-bridge" },
```

The bridge serves its in-memory frame if the Valkey connection drops.

## Authorization and enrollment

The optional `api.serviceToken` accepts `api.cameraIds` as an allowlist. Enable `api.commands` to permit typed commands and `api.events` to permit subscriptions with that token on the bridge's WebSocket interface. `api.adminToken` is required for enrollment and credential rotation. Each extra token must be distinct from `http.token` and at least 24 characters long. MQTT access is administered with broker ACLs.

To enable HTTP callback administration set `api.callbackDestinations` to a list of exact receiver URLs. Use `http.token` or a service token with no camera allowlist. The `POST` and `PUT` bodies use the same `id`, `url`, `cameras` and `events` fields as static `callbacks` entries. Updates replace the bridge configuration file atomically, which requires a writable config directory. The callback HTTP client refuses redirects. [Callback delivery and filters](./interfaces.md#manage-downstream-callbacks)

The former `wui` config key is accepted as an alias for `api` during upgrades. Clients should use the paths in the table.

The password protocol is gated to the verified Lumus E430 revision and firmware. First-password setup checks the complete UID and the factory admin user list. For other devices set the password with Reolink Client on the LAN, configure `targets.<name>.importPassword` privately, and resume as an import. Existing non-enrolled cameras require a configured `api.rotationIds.<cameraId>` object with a printed UID and/or serial. The bridge checks that binding again with fresh authenticated reads before a change.

`restart-required` means the bridge verified a stored credential. Restart the private bridge to start the new workers or switch existing ones to the verified credential. Do not add a duplicate static camera entry for a vault-managed camera. Read inventory after the restart for the active camera IDs. MQTT clients also need broker ACLs for the new camera topics.

After interruption use the existing enrollment status and resume, or rotation reconciliation, to test credentials without resending a password write. If the bridge reports `recovery-required`, use the printed ID and the bridge vault on the host to check current and staged credentials in a private maintenance session. Stop the bridge before restoring or repairing its vault, verify successful authentication to the same printed identifier, and restart it only after the credential reference is consistent. Avoid resetting or repeating a password change until you know which credential works.
