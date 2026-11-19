# HTTP API

The authenticated API is available under `/api` on the bridge's HTTP listener. Send `http.token` as a bearer token. Inventory is available with the standard bridge configuration. Use the optional `api` section for cached previews, video profiles, recording storage, and enrollment.

```json5
api: {
  // For provisioning and camera password rotation, mount persistent storage at this path.
  storageDirectory: "./local-only/vault",
  targets: {
    staging: { host: "192.0.2.2", port: 9000, cameraId: "new-camera" },
  },
  media: {
    entrance: { port: 554, streams: ["main", "sub"] },
  },
  recordings: {
    temporaryDirectory: "/tmp", // null disables recorded-video playback
    cache: { maxBytes: 2147483648, ttlSeconds: 600 },
  },
  previews: {
    cameras: { entrance: { intervalMs: 5000 } },
  },
}
```

Replace the documentation address with a private IPv4 target from the installation network. Configure restricted egress; enrollment accepts only configured RFC1918, link-local or loopback addresses. Use private TLS and a credential directory protected with mode 0700. Store the vault on a persistent local filesystem with atomic rename and durable sync. The bridge stages and verifies credentials in this directory. API callers never receive camera credentials. Files are mode 0600. Back up the vault as sensitive material separately from the bridge configuration.

| Method and path under `/api` | Credential | Behavior |
| --- | --- | --- |
| GET `/contract` | `http.token` | Contract version and configured inventory, settings, media, callback, and enrollment capabilities. |
| GET `/inventory` | `http.token` | Registered public IDs, stable identities and activation status. Devices paused for credential recovery remain visible. |
| POST `/cameras/{id}/command/exec` | `http.token` | Typed command. [Camera controls](./controls.md) |
| GET/POST `/callbacks` | `http.token` | List or add callback delivery rules. |
| PUT/DELETE `/callbacks/{id}` | `http.token` | Update or delete a rule. Use the same ID in the URL and the update body. |
| GET `/cameras/{id}/settings/{group}` | `http.token` | Fresh allowlisted setting values for `leds`, `spotlight`, `motion`, or `ai` and `image`, without raw vendor XML. Writes use the existing typed command interface. |
| GET/HEAD `/cameras/{id}/preview` | `http.token` | Latest cached JPEG or its headers. Supports `If-None-Match` and returns `304` when the image is unchanged. |
| GET `/cameras/{id}/storage` | `http.token` | Card capacity, used/free bytes, readiness, and the camera's local clock. |
| GET `/cameras/{id}/recordings?date=YYYY-MM-DD&stream=sub` | `http.token` | Search a camera-local day. The stream can be `main` or `sub`. Returns opaque recording IDs. |
| GET `/cameras/{id}/recordings/{recordingId}/video` | `http.token` | Browser-playable MP4 with HTTP byte ranges for seeking. Search first to obtain the recording ID. |
| GET `/cameras/{id}/recordings/{recordingId}/metadata` | `http.token` | Timestamped object coordinates from the prepared recording, when embedded by the camera. Shares the video download/cache. |
| GET `/cameras/{id}/recordings/{recordingId}/preview` | `http.token` | JPEG keyframe preview. Optional offsetMs selects a position within the clip (0–86400000, one-second buckets). Search first to obtain the ID. Does not require a writable recording workspace. |
| GET `/cameras/{id}/media/{stream}` | `http.token` | Bounded multipart MJPEG stream from configured H.264 RTSP/TCP. FFmpeg is required in the private bridge environment. The RTSP client owns credentials; FFmpeg receives elementary video bytes. |
| GET `/enrollment-targets` | `http.token` | Allowlisted target names. |
| POST `/enrollments` | `http.token` | Reserve and begin a UUID operation with target, separate `uid` and/or `serial`, `method` (`initialize` or `import`), and `confirmation: true`. First-password flow requires a UID. |
| GET `/enrollments/{id}` and POST `/enrollments/{id}/resume` | `http.token` | Read or continue the persisted operation. Use `{"method":"import"}` after the local setup fallback. |
| GET/POST `/cameras/{id}/rotation` and POST `/cameras/{id}/rotation/reconcile` | `http.token` | Check status, stage and submit one verified-device change, or verify the staged and previous credentials. POST requires UUID `id` and `confirmation: true`. |

## Background camera previews

Set `api.previews.cameras.<cameraId>.intervalMs` for each camera you want polled. An empty camera entry uses 5000 ms. The range is 500 to 2147483647 ms. The bridge starts the interval after each capture completes. It obtains JPEGs with the same read-only Baichuan snapshot operation used for on-demand MQTT captures. Polling runs independently of viewers.

`GET /api/cameras/{id}/preview` reads the latest successful frame from the cache. Use `HEAD` for just the headers or send the previous quoted `ETag` as `If-None-Match` to get a body only when the JPEG changes. `X-Preview-Captured-At` records when the bridge received the image. Use `GET .../preview?refresh=true` to request a fresh capture. The bridge joins overlapping captures for the same camera and returns `503` while the cache is cold or a requested refresh fails.

The cache defaults to memory. To reload the last JPEG after restart, add the following under `api.previews`. Use a private Valkey endpoint and a distinct key prefix for each bridge deployment.

```json5
cache: { backend: "valkey", url: "${BRIDGE_VALKEY_URL}", keyPrefix: "reolink-bridge" },
```

The bridge serves its in-memory frame if the Valkey connection drops.

## HTTP event stream

`GET /api/events` uses `http.token`. Optional comma-separated `cameras` and `events` query parameters use the same camera and event filters as WebSocket subscriptions.

The response is `text/event-stream`. Each SSE data message contains one JSON object. The initial `{"op":"ready","snapshots":[...]}` message supplies cached state, followed by the existing `snapshot` and `event` update envelopes. A `{"op":"gap","snapshots":[...]}` message reports a subscriber backlog and refreshes current state. Comment heartbeats are sent every eight seconds. Reconnect to obtain a new state snapshot after a disconnect.

The stream reads the bridge's event hub. Connections share its existing camera subscriptions. It adds no camera requests. `GET /api/contract` advertises `eventStream` when the caller can subscribe.

## Authorization and enrollment

The bridge authenticates HTTP and WebSocket clients with one PSK, `http.token`. A client holding that PSK can call the enabled operations for every configured camera. Applications manage user accounts and per-camera permissions. MQTT uses the broker's credentials and ACLs. When upgrading, point clients at the existing `http.token` and remove the retired `api.serviceToken`, `api.adminToken`, `api.cameraIds`, `api.commands`, and `api.events` fields.

`api.storageDirectory` holds verified camera credentials and recoverable enrollment and password-rotation operations. Configure it when using `api.targets` or `api.rotationIds`. Generating a Wi-Fi QR in a client and using statically configured camera credentials needs no vault. Keep the directory after bridge-managed provisioning if it supplies the camera password. `api.recordings.temporaryDirectory` is the disposable video cache.

Recording metadata is paginated within the prepared recording cache. Read the initial `/metadata` response and follow `?page=<nextPage>` until `nextPage` is null. Each page contains timestamped detections from the same clip. Video and metadata files share the configured byte and age limits.

Recording preview responses include `X-Preview-Offset-Ms`, the start of the window used for the returned frame. When a camera ends a request before supplying a decodable frame, the bridge retries up to eight seconds earlier within that clip and caches the chosen offset with the JPEG.

### Camera IDs and ConfigMap handoff

The public `cameras[].id` names HTTP paths, event filters, MQTT topics and preview/media profiles. Discovery reads the camera’s serial number through ONVIF or Baichuan. For equipment without a usable serial it can use a reported hardware MAC. To rename `lumus` to `garage`, change its public ID and carry its keys in `api.previews.cameras`, `api.media`, `api.rotationIds`, callback filters and broker subscriptions to the new name.

After provisioning, a static entry with the same host, Baichuan port, channel and username can reuse its verified vault credential. Remove the provisioning target when handing the camera over to the ConfigMap. The optional legacy `stableId` remains readable as a vault and migration hint. Old MQTT topics can be cleared through the broker.

Inventory returns `id`, `stableId`, `identitySource` (`serial` or `mac`), and the legacy migration hint `previousId`. Pending or conflicting hardware identities have a null stable ID. State and event payloads carry `camera_id`, `stable_id` and `identity_source`. Clients can bind an HTTP request with `X-Camera-Stable-Id` or an MQTT command with `stable_id`. A mismatched binding is rejected.

To enable HTTP callback administration set `api.callbackDestinations` to a list of exact receiver URLs. Authenticate with `http.token`. The `POST` and `PUT` bodies use the same `id`, `url`, `cameras` and `events` fields as static `callbacks` entries. Updates replace the bridge configuration file atomically, which requires a writable config directory. The callback HTTP client refuses redirects. [Callback delivery and filters](./interfaces.md#manage-downstream-callbacks)

The former `wui` config key is accepted as an alias for `api` during upgrades. Clients should use the paths in the table.

The password protocol is gated to the verified Lumus E430 revision and firmware. First-password setup checks the complete UID and the factory admin user list. For other devices set the password with Reolink Client on the LAN, configure `targets.<name>.importPassword` privately, and resume as an import. Existing non-enrolled cameras require a configured `api.rotationIds.<cameraId>` object with a printed UID and/or serial. The bridge checks that binding again with fresh authenticated reads before a change.

`restart-required` means the bridge verified a stored credential. Restart the private bridge to start the new workers or switch existing ones to the verified credential. Do not add a duplicate static camera entry for a vault-managed camera. Read inventory after the restart for the active camera IDs. MQTT clients also need broker ACLs for the new camera topics.

After interruption use the existing enrollment status and resume, or rotation reconciliation, to test credentials without resending a password write. If the bridge reports `recovery-required`, use the printed ID and the bridge vault on the host to check current and staged credentials in a private maintenance session. Stop the bridge before restoring or repairing its vault, verify successful authentication to the same printed identifier, and restart it only after the credential reference is consistent. Avoid resetting or repeating a password change until you know which credential works.

## SD-card recordings

Configure `api.recordings.temporaryDirectory` to select the existing directory used for recording downloads and conversion. The default is `/tmp`. Set it to `null` to disable video playback. The bridge probes the directory for write access before advertising or serving recorded video. Missing, invalid, and unwritable directories disable playback without stopping the bridge.

The contract reports native devices in `storageCameras`. It includes them in `recordingsCameras` when the temporary directory is usable. The storage response includes `playbackAvailable`, so clients can disable playback independently of card statistics and file searches. A video request made while the directory is unavailable returns HTTP 422 before any camera download.

Storage and file searches use read-only Baichuan operations. The API reports storage quantities in bytes and recording times in the camera's local wall clock. Each search returns a maximum of 4096 entries and reports truncation. A failed card read returns `API_STORAGE_READ_FAILED`. A failed file search returns `API_RECORDING_SEARCH_FAILED`. Both include a correlation ID for the device error in the bridge log. Each endpoint works independently. If only the camera clock fails, storage still returns the cards with `cameraTime: null`.

Video reads use Baichuan recording downloads. FFmpeg converts the camera's video and available AAC audio into a seekable H.264/AAC MP4. Configure `api.recordings.cache.maxBytes` for the combined converted-video cache budget and `api.recordings.cache.ttlSeconds` for its idle lifetime. Defaults are 2147483648 bytes (2 GiB) and 600 seconds (10 minutes). Both values are integers. The byte budget must be at least 32 and the lifetime must be positive and supported by the system clock.

Each playback request, including a byte-range request, renews that clip's idle lifetime. Recording searches do not extend video-cache lifetime. Expired clips are removed on the next cache operation or periodic cleanup, which runs every 60 seconds or the configured lifetime, whichever is shorter. Size pressure evicts the least recently played cached clips. A converted clip larger than the configured budget fails playback and is discarded. In-progress responses retain their files until they finish. Recording-search IDs remain available for ten minutes after search or playback and while their video is cached.

The byte budget covers cached converted videos across all cameras. Downloads, files being converted, and evicted files still used by active responses also occupy the temporary workspace. Allow additional disk space for these files or enforce a filesystem/volume limit. Two downloads can prepare concurrently, with one active download per camera. Individual transfers accept up to 512 MiB of native camera data and 768 MiB of converted MP4, with a 120-second preparation deadline. Camera credentials and native recording file paths stay inside the bridge.

Recording thumbnails use read-only Baichuan CoverPreview (command 298) to fetch a keyframe near the clip's start. FFmpeg decodes it to a 480-pixel-wide JPEG. The contract advertises `recordingPreviewCameras` independently of playback temporary storage. Thumbnail reads do not download the whole clip. Previews are cached in memory, bounded to 128 images and 32 MiB, with idle expiry at the shorter of `cache.ttlSeconds` and ten minutes. One native recording operation per camera runs at a time. When CoverPreview fails or times out, the bridge reads only the first keyframe from ordinary command 143, then stops. It logs the cause once per five-minute retry window using API_RECORDING_PREVIEW_FALLBACK. Prepared videos can supply thumbnails without a camera read. Recording frame JPEGs use api.previews.cache when Valkey is configured, with a separate namespace and a lifetime at the shorter of the recording cache TTL and ten minutes. The in-process cache remains bounded to 128 images/32 MiB.

Native recording downloads accept camera completion statuses 300 and 331. A playback preparation failure returns `API_RECORDING_PREPARE_FAILED`, a correlation ID and a safe `stage`: `camera-download`, `conversion`, `cache-limit`, `busy`, `timeout`, `temporary-storage` or `preparation`. The server logs the underlying cause with the same correlation ID. Oversized cached clips return HTTP 413, busy preparations return 503 and preparation timeouts return 504. Other preparation failures return 502.

## Recording schedule and time lapse

Authenticated GET requests to `/api/cameras/{id}/settings/recording_schedule` and `/api/cameras/{id}/settings/time_lapse` return native camera settings when the camera advertises those controls. The regular control API accepts these changes and reads the values back from the camera.

```json
{"setting":"recording_schedule","enabled":true,"schedule":{"kind":"continuous","enable_hours":[32],"disable_hours":[]}}
```

Recording hours use the camera’s local week, starting with Sunday at 00:00 (slot 0). Monday at 08:00 is slot 32. Supply the hours to change for one advertised recording type. The bridge preserves other schedules and camera-specific rules.

```json
{"setting":"time_lapse","enabled":true,"interval_seconds":10,"format":"jpeg","stream":"main"}
```

Time lapse supports JPEG or MP4, main or sub stream, and an optional overwrite switch when the camera reports it. Newly initialized tasks use the camera clock and run daily until stopped. Changes to an existing task preserve its dates and daily windows.

## Camera accounts

`GET /api/cameras/{id}/users` returns local camera usernames, administrator flags, the account used by the bridge, and whether password changes are available. The contract lists native endpoints in `userCameras`. Password changes require a verified device password protocol and a credential vault.

`POST /api/cameras/{id}/users/password` accepts a version-4 UUID in `id`, `username`, `password` and `confirmation: true`. If the list returns `requiresUid: true`, include the printed `uid`. The bridge verifies identity before writing and verifies the new login afterward. A change to its own account uses the rotation journal. Restart the bridge when the response reports `restart-required`. Keep the credential vault when restarting.

## Image orientation

The `image_orientation` typed setting accepts `action: "hflip"`, `"vflip"`, `"clockwise"`, or `"counterclockwise"`. Each update reads current image settings, changes the supported orientation field, preserves other image settings, and verifies readback.

The `image` capability reports horizontal/vertical flipping independently. Clockwise and counterclockwise controls require the camera to report quarter-turn support. Availability depends on model and firmware. These operations change the camera's saved image settings.

Protocol references are maintained by [reolink-aio](https://github.com/starkillerOG/reolink_aio/blob/main/reolink_aio/baichuan/baichuan.py) and [nodelink-js](https://github.com/apocaliss92/nodelink-js/blob/main/documentation/baichuan-api/recordings.md).


Each successful background preview refresh publishes a generic state/preview snapshot through the bridge's configured WS, MQTT and HTTP event transports. Its preview field contains the quoted etag and captured_at timestamp. The contract advertises previewNotifications: true. Clients can fetch only changed JPEGs and keep API polling as a fallback. This snapshot is cache metadata, not a detection event.

The recording metadata endpoint returns source: "recording", a frames array of offsetMs/objects entries, and truncated. Offsets follow native video timestamps relative to the first encoded frame; they align with the prepared MP4. Objects use the same bounding_box representation as live detections. Metadata is sampled at no more than five frames per second and bounded to 4096 samples and 4 MiB per clip. Absent metadata returns an empty array; coordinates are never synthesized from boolean alarms. Metadata shares the converted video's size/idle eviction lifecycle, with its bounded memory overhead separate from the video byte budget. Live coordinates require cameras[].coordinates.allowVideoStream: true; reading metadata embedded in a recorded file does not require the live stream option.

HTTP 4xx/5xx responses always generate keyed diagnostics, including framework rejections (HTTP_REQUEST_REJECTED) and failed probes (HTTP_READINESS_FAILED). The middleware preserves already reported causes without duplicate logs. Sink and per-key logging gates decide which outputs receive those records. Preview capture errors use API_PREVIEW_CAPTURE_FAILED even when the last good frame remains available. Recording preview failures use API_RECORDING_PREVIEW_FAILED, and recording metadata failures use API_RECORDING_METADATA_FAILED.

Valkey preview-cache command failures generate API_PREVIEW_CACHE_FAILED with a safe error category. This key defaults to warning level and can be routed or suppressed by the logging gates. A Valkey outage does not disable the memory preview cache.
