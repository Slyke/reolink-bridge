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
    entrance: { port: 554, streams: ["main", "sub", "third"] },
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
| GET `/cameras/{id}/encoding` | `http.token` | Read the camera's stream profiles, current encoder values and supported resolutions, frame rates and bitrates. The contract lists eligible cameras under `encodingCameras`. |
| PUT `/cameras/{id}/encoding` | `http.token` | Send `{"stream":"sub","settings":{"fps":10,"bitrate":256}}`. Supply width and height together for a resolution change. The bridge checks the selected profile's advertised options, sends the changed fields and verifies the camera's new values. Bitrates use kbps. |
| GET `/cameras/{id}/clock` | `http.token` | Read local `cameraTime` and saved `settings` (`utcOffsetMinutes` east of UTC, `daylightSavingEnabled`, `ntpEnabled`). Unreported values are null. `editableOptions` lists the clock switches this bridge can change independently. |
| PUT `/cameras/{id}/clock` | `http.token` | Set local `cameraTime` to the second. For a fixed zone, include e.g. `"timeZone":{"utcOffsetMinutes":-420,"daylightSaving":null}`. Seasonal zones use `daylightSaving:{hours,start,end}`. Each transition has `month,week,weekday,hour,minute` in local time before the change (week 5 means last, Sunday is 0). To switch DST or NTP independently, omit `cameraTime` and send `{"daylightSavingEnabled":true}`, `{"ntpEnabled":false}`, or both fields. The bridge changes only the supplied switches and checks their saved values on a new connection. The camera keeps its seasonal dates, standard offset and NTP server. Optional `ntpEnabled` is also accepted with a time change. A time-only correction with active NTP requires switching it off. For a time change, confirmation also requires an advancing clock on a new connection. |
| GET / PUT `/cameras/{id}/display` | `http.token` | Read or change supported on-screen display fields for a camera listed in `displayCameras`. Send `{"fields":{"watermark":false}}`, for example. The bridge verifies each changed field. |
| GET `/cameras/{id}/network` | `http.token` | Read the active IP configuration, saved Wi-Fi network and camera-reported signal. The `interfaces.wifi` and `interfaces.ethernet` entries include known addresses, active state, enabled state and DHCP status. An empty address list means the firmware supplied no address for that interface. |
| PUT `/cameras/{id}/network/wifi` | `http.token` | Send `{"enabled":false,"confirmation":true}` to disconnect Wi-Fi, or set `enabled` to true to reconnect. The bridge checks the wired path and the camera’s separate ONVIF Wi-Fi interface immediately before the change. The network read advertises this action with `wifiSwitchAvailable`. The Wi-Fi profile stays saved. |
| PUT `/cameras/{id}/network` | `http.token` | Configure Wi-Fi on a registered camera using `ssid`, `password`, `open` and `confirmation: true`. For an open network send an empty password. The response reports whether readback verified the settings or only the SSID (`ssidVerified`). Check wireless association separately. |
| GET `/cameras/{id}/media/capabilities` | `http.token` | Read the physical camera/channel binding, camera-local clock, and support for individual video and time-lapse file deletion. |
| GET `/cameras/{id}/media/days` | `http.token` | List camera-local dates. Use `kind=video&month=YYYY-MM-01` for one month of SD videos or `kind=timeLapse` for the time-lapse task calendars. |
| GET `/cameras/{id}/media/files` | `http.token` | Supply `kind=video\|timeLapse` and `date=YYYY-MM-DD`. Returns the camera binding, exact file IDs with completion times, and a skipped count. |
| POST `/cameras/{id}/media/delete` | `http.token` | Delete the selected original SD files. Body fields are `source`, `kind`, `date` and `files` (0–1024 opaque IDs from the corresponding inventory). |
| GET `/cameras/{id}/time-lapse/photos` | `http.token` | List saved time-lapse photos for a camera-local `date`. Optional inclusive `from` and `to` use HH:mm:ss. Returns an opaque ID and capture time when available. |
| GET `/cameras/{id}/time-lapse/photos/{photo}` | `http.token` | Download the original JPEG converted to a full-resolution PNG. Add `?preview=true` for a JPEG thumbnail up to 480 × 270 pixels. Repeat the search when its ten-minute reference expires. |
| GET `/cameras/{id}/storage` | `http.token` | Card capacity, used/free bytes, readiness, and the camera's local clock. |
| GET `/cameras/{id}/recordings?date=YYYY-MM-DD&stream=sub` | `http.token` | Search a camera-local day. The stream can be `main` or `sub`. Returns opaque recording IDs. |
| GET `/cameras/{id}/recordings/{recordingId}/video` | `http.token` | Browser-playable MP4 with HTTP byte ranges for seeking. Search first to obtain the recording ID. |
| GET `/cameras/{id}/recordings/{recordingId}/metadata` | `http.token` | Timestamped object coordinates from the prepared recording, when embedded by the camera. Shares the video download/cache. |
| GET `/cameras/{id}/recordings/{recordingId}/preview` | `http.token` | JPEG keyframe preview. Optional offsetMs selects a position within the clip (0–86400000, one-second buckets). Search first to obtain the ID. Does not require a writable recording workspace. |
| GET `/cameras/{id}/network/rtsp` | `http.token` | Read the camera's RTSP switch and port over Baichuan. Returns `{"supported":true,"enabled":false,"port":554}` on a camera with RTSP off. A camera without a readable switch returns `{"supported":false}`. |
| PUT `/cameras/{id}/network/rtsp` | `http.token` | Send `{"enabled":true}` or `{"enabled":false}`. The bridge changes only RTSP, preserves the port, and reads the result back. The contract advertises these routes with `rtspSettings`. |
| GET `/cameras/{id}/media/{stream}` | `http.token` | Bounded multipart MJPEG stream from H.264 or H.265 video. Each media profile accepts `transport` of `auto` (default), `rtsp` or `baichuan`. For `main` and `sub`, auto tries RTSP/TCP first and uses Baichuan if RTSP fails to deliver a frame. Add `third` on cameras that support a third live profile. It uses the native Baichuan `externStream` and requires `auto` or `baichuan`. Each camera advertises only its configured streams. FFmpeg receives encoded video bytes. Camera credentials stay at the bridge. |
| GET `/enrollment-targets` | `http.token` | Named targets and `addressLookup`, which advertises lookup of cameras declared in `cameras[]`. |
| GET `/enrollment-targets/identify?address=<IPv4>&method=initialize` | `http.token` | Match the configured camera, sign in and read its UID, reported serial and model. An optional `port` selects its Baichuan port. Use `method=import` for an initialized camera. |
| POST `/enrollments` | `http.token` | Reserve and begin a UUID operation with target, separate `uid` and/or `serial`, `method` (`initialize` or `import`), and `confirmation: true`. First-password setup requires a UID. For a configured camera, also send its returned `address` and `port`. When `/enrollment-targets` advertises `enableRtsp`, send optional `enableRtsp: true` to enable streaming after verified login. The `rtsp` result is `enabled`, `unsupported`, `failed` or `unverified`. Enrollment status is reported separately. Replaying the operation preserves subsequent manual RTSP edits. |
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

`api.storageDirectory` holds verified camera credentials and recoverable enrollment and password-rotation operations. Configure it for camera enrollment or credential rotation. Generating a Wi-Fi QR in a client and using statically configured camera credentials needs no vault. Keep the directory after bridge-managed provisioning if it supplies the camera password. `api.recordings.temporaryDirectory` is the disposable video cache.

Recording metadata is paginated within the prepared recording cache. Read the initial `/metadata` response and follow `?page=<nextPage>` until `nextPage` is null. Each page contains timestamped detections from the same clip. Video and metadata files share the configured byte and age limits.

Recording preview responses include `X-Preview-Offset-Ms`, the start of the window used for the returned frame. When a camera ends a request before supplying a decodable frame, the bridge retries up to eight seconds earlier within that clip and caches the chosen offset with the JPEG.

### Camera IDs and ConfigMap handoff

The public `cameras[].id` names HTTP paths, event filters, MQTT topics and preview/media profiles. Discovery reads the camera’s serial number through ONVIF or Baichuan. For equipment without a usable serial it can use a reported hardware MAC. To rename `lumus` to `garage`, change its public ID and carry its keys in `api.previews.cameras`, `api.media`, `api.rotationIds`, callback filters and broker subscriptions to the new name.

After provisioning, a static entry with the same host, Baichuan port, channel and username can reuse its verified vault credential. Remove the provisioning target when handing the camera over to the ConfigMap. The optional legacy `stableId` remains readable as a vault and migration hint. Old MQTT topics can be cleared through the broker.

Inventory returns `id`, `stableId`, `identitySource` (`serial` or `mac`), and the legacy migration hint `previousId`. Pending or conflicting hardware identities have a null stable ID. State and event payloads carry `camera_id`, `stable_id` and `identity_source`. Clients can bind an HTTP request with `X-Camera-Stable-Id` or an MQTT command with `stable_id`. A mismatched binding is rejected.

To enable HTTP callback administration set `api.callbackDestinations` to a list of exact receiver URLs. Authenticate with `http.token`. The `POST` and `PUT` bodies use the same `id`, `url`, `cameras` and `events` fields as static `callbacks` entries. Updates replace the bridge configuration file atomically, which requires a writable config directory. The callback HTTP client refuses redirects. [Callback delivery and filters](./interfaces.md#manage-downstream-callbacks)

The former `wui` config key is accepted as an alias for `api` during upgrades. Clients should use the paths in the table.

### Set the first admin password over the LAN

Add the camera to `cameras[]` with its wired or wireless LAN IP, administrator username, and desired first password. Keep the private vault at `api.storageDirectory`. A minimal example follows.

```json
{
  "api": { "storageDirectory": "/bridge-vault" },
  "cameras": [{
    "id": "e1-pro",
    "host": "192.168.1.12",
    "username": "admin",
    "password": "${REO_CAM_PASSWORD}",
    "baichuanPort": 9000,
    "channel": 0
  }]
}
```

Apply the camera configuration once. In **Settings → Enrollment**, choose the bridge and **Set first admin password**, enter the IP and select **Identify camera**. Leave the port blank to use the unique configured endpoint. Lookup is read-only and returns the camera’s full UID. Check it against the label and confirm **Start enrollment**. The bridge rechecks the identity, sets the configured password (8–31 printable ASCII characters), verifies a fresh login and starts the camera workers immediately. The client receives only identity and enrollment status. For a camera that already uses its configured password, select **Use an existing password** to verify it.

Address lookup supports private IPv4, link-local or loopback entries using `admin`, channel zero and an enabled Baichuan port. Separately declared `api.targets` remain available by name. An existing named target matching the camera ID (or configured stable ID), address and port can stay as an alias and uses the camera entry’s password. A named target can supply `initialPassword`, omit it to generate a password, or specify `importPassword` for an existing login.

Initialization supports Lumus E430 (`IPC_NT1NO24MP`, `v3.1.0.6198_2604102075`) and E1 Pro E330 (`IPC_NT1NA45MP`, `v3.1.0.4417_2412122130`). It runs only on a confirmed enrollment request. The bridge signs in to the factory admin account, matches the full printed UID, checks the firmware and sole administrator, stages the credential in its private vault, and verifies a fresh login and device identity after setting the password.

Camera account management and credential rotation support both hardware and firmware revisions listed above. For other revisions, use the device's supported initialization method, set the existing password in the camera configuration, and enroll using `import`. Rotation on a statically configured camera without a vault binding requires `api.rotationIds.<cameraId>` with a printed UID or serial.

A configured camera returns `active` once its running credential is verified. A named target awaiting its workers, or an existing worker using a different credential, returns `restart-required`. Restart the bridge in that case. MQTT clients need broker ACLs for the camera topics.

After interruption use the existing enrollment status and resume, or rotation reconciliation, to test credentials without resending a password write. If the bridge reports `recovery-required`, use the printed ID and the bridge vault on the host to check current and staged credentials in a private maintenance session. Stop the bridge before restoring or repairing its vault, verify successful authentication to the same printed identifier, and restart it only after the credential reference is consistent. Avoid resetting or repeating a password change until you know which credential works.

## SD-card recordings

Configure `api.recordings.temporaryDirectory` to select the existing directory used for recording downloads and conversion. The default is `/tmp`. Set it to `null` to disable video playback. The bridge probes the directory for write access before advertising or serving recorded video. Missing, invalid, and unwritable directories disable playback without stopping the bridge.

The contract reports native devices in `storageCameras`. It includes them in `recordingsCameras` when the temporary directory is usable. The storage response includes `playbackAvailable`, so clients can disable playback independently of card statistics and file searches. A video request made while the directory is unavailable returns HTTP 422 before any camera download.

Display changes use separate requests for general settings (`deviceName`, `dateFormat`) and channel overlays (`name`, `nameVisible`, `timeVisible`, `watermark`, `background`, `nameX`, `nameY`, `timeX`, `timeY`). Fields appear when the firmware reports them. Overlay coordinates exposed for editing range from 0 to 65536. The bridge reads both channel overlay blocks before a save, preserves their firmware-specific layout and reads back the edited fields. If the camera refuses the write, `API_DISPLAY_REJECTED` includes its numeric `cameraStatus`. The correlated log entry includes the camera ID, native command number and requested field names. A camera rename keeps its existing physical identity and grants.

Storage and file searches use read-only Baichuan operations. The API reports storage quantities in bytes and recording times in the camera's local wall clock. Each search returns a maximum of 4096 entries and reports truncation. A failed card read returns `API_STORAGE_READ_FAILED`. A failed file search returns `API_RECORDING_SEARCH_FAILED`. Both include a correlation ID for the device error in the bridge log. Each endpoint works independently. If only the camera clock fails, storage still returns the cards with `cameraTime: null`.

Video reads use Baichuan recording downloads. FFmpeg converts the camera's video and available AAC audio into a seekable H.264/AAC MP4. Configure `api.recordings.cache.maxBytes` for the combined converted-video cache budget and `api.recordings.cache.ttlSeconds` for its idle lifetime. Defaults are 2147483648 bytes (2 GiB) and 600 seconds (10 minutes). Both values are integers. The byte budget must be at least 32 and the lifetime must be positive and supported by the system clock.

Each playback request, including a byte-range request, renews that clip's idle lifetime. Recording searches do not extend video-cache lifetime. Expired clips are removed on the next cache operation or periodic cleanup, which runs every 60 seconds or the configured lifetime, whichever is shorter. Size pressure evicts the least recently played cached clips. A converted clip larger than the configured budget fails playback and is discarded. In-progress responses retain their files until they finish. Recording-search IDs remain available for ten minutes after search or playback and while their video is cached.

The byte budget covers cached converted videos across all cameras. Downloads, files being converted, and evicted files still used by active responses also occupy the temporary workspace. Allow additional disk space for these files or enforce a filesystem/volume limit. Two downloads can prepare concurrently, with one active download per camera. Individual transfers accept up to 512 MiB of native camera data and 768 MiB of converted MP4, with a 120-second preparation deadline. Camera credentials and native recording file paths stay inside the bridge.

Recording thumbnails use read-only Baichuan CoverPreview (command 298) to fetch a keyframe near the clip's start. FFmpeg decodes it to a 480-pixel-wide JPEG. The contract advertises `recordingPreviewCameras` independently of playback temporary storage. Thumbnail reads do not download the whole clip. Previews are cached in memory, bounded to 128 images and 32 MiB, with idle expiry at the shorter of `cache.ttlSeconds` and ten minutes. One native recording operation per camera runs at a time. When CoverPreview fails or times out, the bridge reads only the first keyframe from ordinary command 143, then stops. It logs the cause once per five-minute retry window using API_RECORDING_PREVIEW_FALLBACK. Prepared videos can supply thumbnails without a camera read. Recording frame JPEGs use api.previews.cache when Valkey is configured, with a separate namespace and a lifetime at the shorter of the recording cache TTL and ten minutes. The in-process cache remains bounded to 128 images/32 MiB.

Native recording downloads accept camera completion statuses 300 and 331. A playback preparation failure returns `API_RECORDING_PREPARE_FAILED`, a correlation ID and a safe `stage`: `camera-download`, `conversion`, `cache-limit`, `busy`, `timeout`, `temporary-storage` or `preparation`. The server logs the underlying cause with the same correlation ID. Oversized cached clips return HTTP 413, busy preparations return 503 and preparation timeouts return 504. Other preparation failures return 502.

### Exact SD file deletion

The contract advertises `mediaFileCameras`. Query each camera’s capabilities for `video` and `timeLapse` support. The caller owns any schedule or age policy. The bridge rechecks the physical source, camera capability and selected day’s inventory before issuing native exact-file commands. It verifies removal against a fresh inventory and returns separate `deleted` and `absent` ID arrays. An empty array succeeds immediately. Repeating a request for a previously removed file reports it as absent.

JPEG files require a capture time. Time-lapse MP4 files require an idle task and a reported end time. An ambiguous identity stays protected. Video deletion requires the camera’s individual recording deletion capability and a native file identity. If a batch is interrupted, list the files again or retry the same IDs.

## Recording schedule and time lapse

Authenticated GET requests to `/api/cameras/{id}/settings/recording_schedule` and `/api/cameras/{id}/settings/time_lapse` return native camera settings when the camera advertises those controls. The regular control API accepts these changes and reads the values back from the camera.

```json
{"setting":"recording_schedule","enabled":true,"schedule":{"kind":"continuous","enable_hours":[32],"disable_hours":[]}}
```

Recording hours use the camera’s local week, starting with Sunday at 00:00 (slot 0). Monday at 08:00 is slot 32. Supply the hours to change for one advertised recording type. The bridge preserves other schedules and camera-specific rules.

```json
{"setting":"time_lapse","enabled":true,"interval_seconds":10,"format":"jpeg","stream":"main"}
```

Time lapse supports JPEG or MP4, main or sub stream, and an optional overwrite switch when the camera reports it. New tasks default to JPEG, use the camera clock and run daily until stopped. Specify `"format": "mp4"` to save video. On cameras that support overwrite, new tasks enable it unless the request specifies `"overwrite": false`. Existing tasks retain their overwrite setting, dates and daily windows. Fluent photos use the original JPEG for gallery previews. An explicit camera rejection of the time-lapse write returns `CONTROL_TIME_LAPSE_REJECTED` and the numeric `cameraStatus`.

## Camera accounts

`GET /api/cameras/{id}/users` returns local camera usernames, administrator flags, the account used by the bridge, and whether password changes are available. The contract lists native endpoints in `userCameras`. Password changes require a verified device password protocol and a credential vault.

`POST /api/cameras/{id}/users/password` accepts a version-4 UUID in `id`, `username`, `password` and `confirmation: true`. If the list returns `requiresUid: true`, include the printed `uid`. The bridge verifies identity before writing and verifies the new login afterward. A change to its own account uses the rotation journal. Restart the bridge when the response reports `restart-required`. Keep the credential vault when restarting.

When the list returns `manageAccounts: true`, `POST /api/cameras/{id}/users` also accepts the following operations. Both take the same operation ID, confirmation and optional identity binding as a password change.

```json
{"action":"add","id":"d9ff5da9-aa38-4275-bdbe-197b8cb3d227","username":"viewer","role":"user","password":"<new camera password>","confirmation":true}
```

```json
{"action":"remove","id":"aa058e55-247d-4893-9a92-78fb0f11ab77","username":"viewer","confirmation":true}
```

The available roles are `user` and `administrator`. Only the built-in administrator can manage other administrators. The bridge protects its own login and the built-in admin from deletion. It reads back membership and checks a new login before reporting success. Reusing an operation ID cannot submit the device write twice.

## Image orientation

The `image_orientation` typed setting accepts `action: "hflip"`, `"vflip"`, `"clockwise"`, or `"counterclockwise"`. Each update reads current image settings, changes the supported orientation field, preserves other image settings, and verifies readback.

The `image` capability reports horizontal/vertical flipping independently. Clockwise and counterclockwise controls require the camera to report quarter-turn support. Availability depends on model and firmware. These operations change the camera's saved image settings.

Protocol references are maintained by [reolink-aio](https://github.com/starkillerOG/reolink_aio/blob/main/reolink_aio/baichuan/baichuan.py) and [nodelink-js](https://github.com/apocaliss92/nodelink-js/blob/main/documentation/baichuan-api/recordings.md).


Each successful background preview refresh publishes a generic state/preview snapshot through the bridge's configured WS, MQTT and HTTP event transports. Its preview field contains the quoted etag and captured_at timestamp. The contract advertises previewNotifications: true. Clients can fetch only changed JPEGs and keep API polling as a fallback. This snapshot is cache metadata, not a detection event.

The recording metadata endpoint returns source: "recording", a frames array of offsetMs/objects entries, and truncated. Offsets follow native video timestamps relative to the first encoded frame; they align with the prepared MP4. Objects use the same bounding_box representation as live detections. Metadata is sampled at no more than five frames per second and bounded to 4096 samples and 4 MiB per clip. Absent metadata returns an empty array; coordinates are never synthesized from boolean alarms. Metadata shares the converted video's size/idle eviction lifecycle, with its bounded memory overhead separate from the video byte budget. Live coordinates require cameras[].coordinates.allowVideoStream: true; reading metadata embedded in a recorded file does not require the live stream option.

HTTP 4xx/5xx responses always generate keyed diagnostics, including framework rejections (HTTP_REQUEST_REJECTED) and failed probes (HTTP_READINESS_FAILED). The middleware preserves already reported causes without duplicate logs. Sink and per-key logging gates decide which outputs receive those records. Preview capture errors use API_PREVIEW_CAPTURE_FAILED even when the last good frame remains available. Recording preview failures use API_RECORDING_PREVIEW_FAILED, and recording metadata failures use API_RECORDING_METADATA_FAILED.

Valkey preview-cache command failures generate API_PREVIEW_CACHE_FAILED with a safe error category. This key defaults to warning level and can be routed or suppressed by the logging gates. A Valkey outage does not disable the memory preview cache.


### Camera picture, sound, detection and delivery settings

The contract flag `deviceOptions` enables `GET` and `PUT /api/cameras/{id}/options/{group}` for Baichuan cameras listed under `displayCameras`. The groups are `picture`, `threshold`, `sound`, `recordingAudio`, `privacy`, `tracking`, `siren`, `push`, `email` and `ftp`. Each GET returns the available fields, their current values and applicable bounds or choices. A privacy response can include the number of existing masks. The audio recording switches belong to their individual streams. Alert delivery switches use the schedules and destinations already stored on the device.

The separate `ftpDestination` contract flag exposes `GET` and `PUT /api/cameras/{id}/options/ftpDestination`. This reads the camera's device-wide FTP configuration. The public `fields` include the server, port and any reported `username`, `remoteDirectory`, `anonymous` and `ftpsOnly` settings. The bridge returns `passwordStored` and `passwordMaxLength` when the camera exposes a password field. Send changed fields only. Omitting `password` retains the credential at the current destination, and an empty string clears it. When changing the address or port with a stored credential, supply the password for the new destination or explicitly clear it. A masked password readback leaves a password change unverified. A camera can expose the per-channel FTP task switch while rejecting the destination command; the latter returns `API_DEVICE_OPTIONS_UNSUPPORTED`.

Send a JSON object such as `{"fields":{"contrast":140,"dayNight":"auto"}}` to the picture endpoint. The bridge validates the reported channel, updates only the requested fields in that settings group and verifies the values by reading the camera again. A firmware rejection or a missing readback is reported separately. Camera notification addresses, credentials and mask coordinates stay on the bridge.

The clock endpoint includes `settings.daylightSavingSchedule` when the camera supplies valid recurring rules. Enabling DST makes the camera switch on those dates automatically.
