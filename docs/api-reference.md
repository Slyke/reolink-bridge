# Reolink bridge API reference

Use the bridge HTTP API to discover and control cameras. Subscribe to push messages at WebSocket `/ws` or SSE `/api/events`. The examples pair each request with its response and use fictional credentials, camera IDs and file references. Use the values returned by your bridge. Binary examples show the media type, bytes or signature. A `HEAD` request on a `GET` resource returns headers without a body.

The WUI uses its own camera IDs and session credentials. [WUI API reference](https://github.com/Slyke/reolink-wui/blob/master/docs/api-reference.md) · [MQTT topics and payloads](./events.md)

- [Bridge discovery and health](#bridge-discovery-and-health)
- [Bridge WebSocket and events](#bridge-websocket-and-events)
- [Bridge callbacks and camera notifications](#bridge-callbacks-and-camera-notifications)
- [Bridge controls and settings](#bridge-controls-and-settings)
- [Bridge media and talk](#bridge-media-and-talk)
- [Bridge enrollment and camera users](#bridge-enrollment-and-camera-users)
- [Implementation sources](#implementation-sources)

## Bridge discovery and health

Send `Authorization: Bearer <http.token>` to `/api/*` and on the WebSocket handshake. Send `Content-Type: application/json` with JSON bodies. `/livez` and `/readyz` are public. The two camera notification routes use a per-route key in the URL. Every HTTP response carries `X-Correlation-Id`. A valid UUID in that request header is echoed.

Use the `id` from `/api/inventory` in a camera URL, for example `front-door`. To bind a request to the hardware you just discovered, send `X-Camera-Stable-Id` with its `stableId`. A changed or unknown binding yields `409` with `API_CAMERA_IDENTITY_CHANGED`. Feature arrays in `/api/contract` tell you which camera IDs can use each resource.

#### `GET /livez`

```http
GET /livez HTTP/1.1

HTTP/1.1 200 OK
Content-Type: application/json

{
  "ok": true,
  "probe": "liveness",
  "service": "reolink-bridge",
  "version": "0.1.0",
  "buildHash": "unknown",
  "correlation_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
}
```

#### `GET /readyz`

An unavailable dependency yields `500` with `ok: false` and the failed check.

```http
GET /readyz HTTP/1.1

HTTP/1.1 200 OK
Content-Type: application/json

{
  "ok": true,
  "probe": "readiness",
  "service": "reolink-bridge",
  "version": "0.1.0",
  "buildHash": "unknown",
  "correlation_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
  "checks": {"cameras": true, "mqtt": true}
}
```

#### `GET /api/contract`

Example for a bridge with one camera, a preview worker, media storage and enrollment enabled.

```http
GET /api/contract HTTP/1.1

HTTP/1.1 200 OK
Content-Type: application/json

{
  "version": 1,
  "terminate": true,
  "terminationOptions": true,
  "eventStream": true,
  "media": true,
  "mediaCameras": {"front-door": ["main", "sub"]},
  "talk": true,
  "detectionZones": true,
  "privacyMaskEditing": true,
  "ftpDestination": true,
  "deviceOptions": true,
  "displayCameras": ["front-door"],
  "encodingCameras": ["front-door"],
  "clockCameras": ["front-door"],
  "networkCameras": ["front-door"],
  "userCameras": ["front-door"],
  "storageCameras": ["front-door"],
  "mediaFileCameras": ["front-door"],
  "timeLapsePhotoCameras": ["front-door"],
  "recordingPreviewCameras": ["front-door"],
  "recordingMetadataCameras": ["front-door"],
  "recordingsCameras": ["front-door"],
  "previewCameras": ["front-door"],
  "rtspSettings": true,
  "previewNotifications": true,
  "enrollment": true,
  "inventory": true,
  "callbacks": true,
  "settings": ["leds", "spotlight", "motion", "ai", "image", "recording_schedule", "time_lapse"],
  "alerts": ["motion"],
  "correlation_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
}
```

#### `GET /api/inventory`

`stableId` and `identitySource` can be null during discovery. A hardware serial, when available, appears in `hardwareSerial`. Inactive registered cameras can appear here.

```http
GET /api/inventory HTTP/1.1
Host: bridge.example
Authorization: Bearer <http.token>

HTTP/1.1 200 OK
Content-Type: application/json

{
  "cameras": [
    {
      "id": "front-door",
      "stableId": "952700EXAMPLE123",
      "identitySource": "uid",
      "previousId": "front-door",
      "active": true
    }
  ],
  "correlation_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
}
```

#### Camera read retries

The authenticated contract reports the current limit in `cameraReadRetries`. Send
`PUT /api/settings/camera-read-retries` with `{"retries":2}` to change it for subsequent reads.
The value is an integer from 0 through 10 and counts attempts after the first one.
Standalone deployments can set `api.cameraReadRetries` in the bridge configuration for startup.
The default is 2. An operation keeps its original deadline and its limit at dispatch.
Temporary socket failures on supported reads reconnect with backoff. Camera rejections and
invalid responses end the operation. Writes, relative PTZ movement, preset recall,
formatting, file deletion and credential changes are dispatched once. A timed PTZ movement
still sends its safety Stop if the acknowledgement is lost.

#### `POST /api/terminate`

The first accepted request selects the process exit code. Both fields are optional. An empty POST defaults to 0. The supervisor determines whether the bridge restarts.

```http
POST /api/terminate HTTP/1.1
Content-Type: application/json

{"exitCode": 75, "reason": "Scheduled maintenance"}

HTTP/1.1 202 Accepted
Content-Type: application/json

{"ok": true, "status": "terminating", "exitCode": 75, "correlation_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"}
```

An unauthenticated contract request returns `401`. Some bridge errors also include a human-readable `error` and a stable `errorCode`. Match `errorKey` in client code.

```json
{"ok": false, "errorKey": "API_SCOPE_DENIED", "correlation_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"}
```

## Bridge WebSocket and events

Open the configured `http.websocket.path` (default `/ws`) with the bridge token. After the `101` upgrade, send one JSON text object per frame, up to 32 KiB. Request IDs accept 1–64 ASCII letters, digits, `_` or `-`. Direct replies echo `id` and include `correlation_id`. Event delivery begins after `subscribe`.

#### `GET /ws`

Example upgrade for `wss://bridge.example/ws`.

```http
GET /ws HTTP/1.1
Host: bridge.example
Upgrade: websocket
Connection: Upgrade
Sec-WebSocket-Version: 13
Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==
Authorization: Bearer <http.token>

HTTP/1.1 101 Switching Protocols
Connection: Upgrade
Upgrade: websocket
Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=
```

#### `subscribe`

Replaces the subscription and returns matching cached state. Omitted or empty filter arrays match all. `events` accepts a normalized type or exact vendor topic.

Client → bridge

```json
{"id": "watch-1", "op": "subscribe", "cameras": ["front-door"], "events": ["doorbell"]}
```

Bridge → client

```json
{
  "id": "watch-1",
  "ok": true,
  "snapshots": [
    {
      "camera_id": "front-door",
      "topic": "availability",
      "payload": {
        "camera_id": "front-door",
        "stable_id": "952700EXAMPLE123",
        "identity_source": "uid",
        "online": true,
        "transport": "onvif_push",
        "reason": "subscribed",
        "updated_at": "2026-11-23T17:30:00.000Z"
      }
    }
  ],
  "correlation_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
}
```

#### `snapshot`

Reads cached state using the current subscription filter. Before the first subscription it returns the available state for all cameras. For image bytes, use the camera preview endpoint.

Client → bridge

```json
{"id": "read-1", "op": "snapshot"}
```

Bridge → client

```json
{
  "id": "read-1",
  "ok": true,
  "snapshots": [
    {
      "camera_id": "front-door",
      "topic": "availability",
      "payload": {
        "camera_id": "front-door",
        "stable_id": "952700EXAMPLE123",
        "identity_source": "uid",
        "online": true,
        "transport": "onvif_push",
        "reason": "subscribed",
        "updated_at": "2026-11-23T17:30:00.000Z"
      }
    }
  ],
  "correlation_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
}
```

#### `control`

Match the reply by `id`. Controls can complete out of order. The [command catalog](#typed-camera-commands) also applies here.

Client → bridge

```json
{
  "id": "move-1",
  "op": "control",
  "camera_id": "front-door",
  "command": {"setting": "ptz", "action": {"op": "move", "direction": "left", "duration_ms": 400}}
}
```

Bridge → client

```json
{
  "id": "move-1",
  "ok": true,
  "verified": false,
  "result": {"action": "move", "direction": "left", "duration_ms": 400, "stopped": true, "interrupted": false},
  "correlation_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
}
```

#### `callbacks_list`

Client → bridge

```json
{"id": "cb-1", "op": "callbacks_list"}
```

Bridge → client

```json
{"id": "cb-1", "ok": true, "callbacks": [], "correlation_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"}
```

#### `callbacks_add`

Client → bridge

```json
{
  "id": "cb-2",
  "op": "callbacks_add",
  "callback": {
    "id": "porch",
    "url": "https://receiver.example/camera-events",
    "cameras": ["front-door"],
    "events": ["doorbell"]
  }
}
```

Bridge → client

```json
{
  "id": "cb-2",
  "ok": true,
  "callbacks": [
    {
      "id": "porch",
      "url": "https://receiver.example/camera-events",
      "cameras": ["front-door"],
      "events": ["doorbell"]
    }
  ],
  "correlation_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
}
```

#### `callbacks_update`

Client → bridge

```json
{
  "id": "cb-3",
  "op": "callbacks_update",
  "callback": {
    "id": "porch",
    "url": "https://receiver.example/camera-events",
    "cameras": ["front-door"],
    "events": ["motion"]
  }
}
```

Bridge → client

```json
{
  "id": "cb-3",
  "ok": true,
  "callbacks": [
    {
      "id": "porch",
      "url": "https://receiver.example/camera-events",
      "cameras": ["front-door"],
      "events": ["motion"]
    }
  ],
  "correlation_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
}
```

#### `callbacks_remove`

Client → bridge

```json
{"id": "cb-4", "op": "callbacks_remove", "callback_id": "porch"}
```

Bridge → client

```json
{"id": "cb-4", "ok": true, "callbacks": [], "correlation_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"}
```

#### Unsolicited WebSocket messages

The bridge sends these as individual text frames, without a request ID. A `gap` replaces the client’s cached state after a backlog. `snapshot` with a singular `snapshot` field is a push. Replies to a client snapshot request use the plural `snapshots`.

```json
{
  "op": "event",
  "event": {
    "camera_id": "front-door",
    "stable_id": "952700EXAMPLE123",
    "identity_source": "uid",
    "type": "doorbell",
    "source": "reolink_webhook",
    "vendor_topic": "HaCfg/wake",
    "received_at": "2026-11-23T17:30:00.000Z",
    "channel": 0,
    "vendor": {"event": "wake", "reason": "doorbell"}
  }
}
```

```json
{
  "op": "snapshot",
  "snapshot": {
    "camera_id": "front-door",
    "topic": "availability",
    "payload": {
      "camera_id": "front-door",
      "stable_id": "952700EXAMPLE123",
      "identity_source": "uid",
      "online": true,
      "transport": "onvif_push",
      "reason": "subscribed",
      "updated_at": "2026-11-23T17:30:00.000Z"
    }
  }
}
```

```json
{
  "op": "gap",
  "snapshots": [
    {
      "camera_id": "front-door",
      "topic": "availability",
      "payload": {
        "camera_id": "front-door",
        "stable_id": "952700EXAMPLE123",
        "identity_source": "uid",
        "online": true,
        "transport": "onvif_push",
        "reason": "subscribed",
        "updated_at": "2026-11-23T17:30:00.000Z"
      }
    }
  ]
}
```

Snapshot topics include `availability`, `info`, `capabilities`, `rules`, `state/<event type>`, `state/settings`, `state/ptz` and `state/preview`. Detection events can include `active`, `camera_time`, `operation`, `rule_id`, `objects` and vendor details when available. An invalid JSON request or invalid request ID gets an error with `id: null`. For example, sending `{"id":"read-2","op":"missing"}` yields

```json
{
  "ok": false,
  "error": "The WebSocket request is invalid or uses an unsupported operation.",
  "errorKey": "WEBSOCKET_REQUEST_INVALID",
  "errorCode": "404C2AAC8B6D7328",
  "correlation_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
  "id": null
}
```

#### `GET /api/events`

Use comma-separated `cameras` and `events` (up to 128 entries per list). The stream opens with `ready`, then uses the same `event`, `snapshot` and `gap` payloads shown above. Comment heartbeats run every eight seconds. Reconnect for a fresh initial snapshot.

```http
GET /api/events?cameras=front-door&events=doorbell,motion HTTP/1.1
Accept: text/event-stream

HTTP/1.1 200 OK
Content-Type: text/event-stream

data: {"op":"ready","snapshots":[{"camera_id":"front-door","topic":"availability","payload":{"camera_id":"front-door","stable_id":"952700EXAMPLE123","identity_source":"uid","online":true,"transport":"onvif_push","reason":"subscribed","updated_at":"2026-11-23T17:30:00.000Z"}}]}

data: {"op":"event","event":{"camera_id":"front-door","stable_id":"952700EXAMPLE123","identity_source":"uid","type":"doorbell","source":"reolink_webhook","vendor_topic":"HaCfg/wake","received_at":"2026-11-23T17:30:00.000Z","channel":0,"vendor":{"event":"wake","reason":"doorbell"}}}

:
```

## Bridge callbacks and camera notifications

### Manage callback delivery rules

HTTP callback administration uses the bridge token and configured `api.callbackDestinations`. Add and update take the exact receiver URL and a complete rule. Edits persist to the bridge configuration. An empty `cameras` or `events` array matches all. Listings may preserve an environment reference or redact a URL query with `query_redacted: true`.

#### `GET /api/callbacks`

```http
GET /api/callbacks HTTP/1.1

HTTP/1.1 200 OK
Content-Type: application/json

{"ok": true, "callbacks": [], "correlation_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"}
```

#### `POST /api/callbacks`

```http
POST /api/callbacks HTTP/1.1
Content-Type: application/json

{
  "id": "porch",
  "url": "https://receiver.example/camera-events",
  "cameras": ["front-door"],
  "events": ["doorbell"]
}

HTTP/1.1 200 OK
Content-Type: application/json

{
  "ok": true,
  "callbacks": [
    {
      "id": "porch",
      "url": "https://receiver.example/camera-events",
      "cameras": ["front-door"],
      "events": ["doorbell"]
    }
  ],
  "correlation_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
}
```

#### `PUT /api/callbacks/{callback}`

The body `id` must match the path.

```http
PUT /api/callbacks/porch HTTP/1.1
Content-Type: application/json

{
  "id": "porch",
  "url": "https://receiver.example/camera-events",
  "cameras": ["front-door"],
  "events": ["motion"]
}

HTTP/1.1 200 OK
Content-Type: application/json

{
  "ok": true,
  "callbacks": [
    {
      "id": "porch",
      "url": "https://receiver.example/camera-events",
      "cameras": ["front-door"],
      "events": ["motion"]
    }
  ],
  "correlation_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
}
```

#### `DELETE /api/callbacks/{callback}`

```http
DELETE /api/callbacks/porch HTTP/1.1

HTTP/1.1 200 OK
Content-Type: application/json

{"ok": true, "callbacks": [], "correlation_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"}
```

### Bridge → registered receiver

For every matching live event the bridge sends the event object to the rule’s URL. The receiver acknowledges with any 2xx status. The bridge uses a five-second delivery timeout.

#### `POST /camera-events`

Example destination `https://receiver.example/camera-events`.

```http
POST /camera-events HTTP/1.1
Host: receiver.example
Content-Type: application/json

{
  "camera_id": "front-door",
  "stable_id": "952700EXAMPLE123",
  "identity_source": "uid",
  "type": "doorbell",
  "source": "reolink_webhook",
  "vendor_topic": "HaCfg/wake",
  "received_at": "2026-11-23T17:30:00.000Z",
  "channel": 0,
  "vendor": {"event": "wake", "reason": "doorbell"}
}

HTTP/1.1 204 No Content
```

### Camera → bridge

Use the key assigned to that camera’s notification route. A successful delivery returns `200` with an empty body.

#### `POST /ingest/onvif/{camera}/{key}`

The ONVIF push receiver accepts `application/soap+xml`, `text/xml` or `application/xml`.

```http
POST /ingest/onvif/front-door/example-route-key HTTP/1.1
Content-Type: application/soap+xml

<?xml version="1.0"?>
<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"
    xmlns:wsnt="http://docs.oasis-open.org/wsn/b-2"
    xmlns:tt="http://www.onvif.org/ver10/schema"
    xmlns:tns1="http://www.onvif.org/ver10/topics">
  <s:Body><wsnt:Notify>
    <wsnt:NotificationMessage>
      <wsnt:Topic Dialect="http://www.onvif.org/ver10/tev/topicExpression/ConcreteSet">tns1:RuleEngine/MyRuleDetector/PeopleDetect</wsnt:Topic>
      <wsnt:Message><tt:Message UtcTime="2026-11-11T17:33:06.413Z" PropertyOperation="Changed">
        <tt:Source><tt:SimpleItem Name="Source" Value="000"/></tt:Source><tt:Data><tt:SimpleItem Name="State" Value="true"/></tt:Data>
      </tt:Message></wsnt:Message>
    </wsnt:NotificationMessage>
  </wsnt:Notify></s:Body>
</s:Envelope>

HTTP/1.1 200 OK
```

#### `POST /ingest/reolink/{camera}/{key}`

HaCfg example. The same route also accepts HaCfg alarm reports such as `{"cmd":33,"xml":"<body><AlarmEvent><channelId>0</channelId><status>MD</status></AlarmEvent></body>"}` when that parser is selected. A camera configured for CGI sends `{"alarm":{"channel":0,"type":"AI_PEOPLE"}}` or `{"type":"test"}`.

```http
POST /ingest/reolink/front-door/example-route-key HTTP/1.1
Content-Type: application/json

{"data": {"event": "wake", "reason": "doorbell"}}

HTTP/1.1 200 OK
```

### Bridge → camera CGI

When CGI webhook setup is configured, the bridge calls the camera’s `/cgi-bin/api.cgi`. All four commands use an array request and an array response. `Login` supplies the token for the remaining calls. The bridge reads the webhook list again after saving to verify the slot.

#### `POST /cgi-bin/api.cgi` · Login

```http
POST /cgi-bin/api.cgi?cmd=Login HTTP/1.1
Content-Type: application/json

[{"cmd": "Login", "action": 0, "param": {"User": {"userName": "admin", "password": "example-camera-password"}}}]

HTTP/1.1 200 OK
Content-Type: application/json

[{"cmd": "Login", "code": 0, "value": {"Token": {"name": "example-camera-session"}}}]
```

#### `POST /cgi-bin/api.cgi` · GetWebHook

```http
POST /cgi-bin/api.cgi?cmd=GetWebHook&token=example-camera-session HTTP/1.1
Content-Type: application/json

[{"cmd": "GetWebHook", "action": 0, "param": {"channel": 0}}]

HTTP/1.1 200 OK
Content-Type: application/json

[{"cmd": "GetWebHook", "code": 0, "value": {"WebHook": [{"index": 1, "indexEnable": -1, "hookUrl": ""}]}}]
```

#### `POST /cgi-bin/api.cgi` · SetWebHook

```http
POST /cgi-bin/api.cgi?cmd=SetWebHook&token=example-camera-session HTTP/1.1
Content-Type: application/json

[
  {
    "cmd": "SetWebHook",
    "action": 0,
    "param": {
      "WebHook": {
        "channel": 0,
        "index": 1,
        "indexEnable": 1,
        "hookUrl": "https://bridge.example/ingest/reolink/front-door/example-route-key",
        "bCustom": 0,
        "hookBody": ""
      }
    }
  }
]

HTTP/1.1 200 OK
Content-Type: application/json

[{"cmd": "SetWebHook", "code": 0, "value": {"rspCode": 200}}]
```

#### `POST /cgi-bin/api.cgi` · Logout

```http
POST /cgi-bin/api.cgi?cmd=Logout&token=example-camera-session HTTP/1.1
Content-Type: application/json

[{"cmd": "Logout", "action": 0, "param": {}}]

HTTP/1.1 200 OK
Content-Type: application/json

[{"cmd": "Logout", "code": 0, "value": {"rspCode": 200}}]
```

## Bridge controls and settings

#### `POST /api/cameras/{camera}/command/exec`

Use a fresh command ID for each intended action. PTZ movement IDs are shared across HTTP, WS and MQTT. A repeated movement with the same remembered ID and input reuses its outcome. A conflicting payload can return `409 CONTROL_ID_CONFLICT`.

```http
POST /api/cameras/front-door/command/exec HTTP/1.1
Content-Type: application/json

{"id": "presets-1", "command": {"setting": "ptz", "action": {"op": "list_presets"}}}

HTTP/1.1 200 OK
Content-Type: application/json

{
  "id": "presets-1",
  "ok": true,
  "verified": true,
  "result": {"presets": [{"id": 1, "name": "Door"}]},
  "correlation_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
}
```

### Typed camera commands

For HTTP put the left column in `command`. For WS put it in `command` on the `control` frame. For WUI settings put it in `change`. The right column shows the setting-specific fields in the bridge reply’s `state`. Some camera readbacks also include a `vendor` object. The `recording_schedule` example here changes hours 8 and 9 of the weekly table.

| Request `command` | Example `state` fields |
| --- | --- |
| `{"setting":"spotlight","enabled":true,"duration_seconds":30}` | `{"enabled":true,"received_at":"2026-11-23T17:30:00.000Z"}` |
| `{"setting":"spotlight_brightness","brightness":60}` | `{"enabled":true,"brightness":60}` |
| `{"setting":"status_led","enabled":false}` | `{"status_led":false,"ir_mode":"auto"}` |
| `{"setting":"ir_led","mode":"auto"}` | `{"status_led":false,"ir_mode":"auto"}` |
| `{"setting":"motion_detection","enabled":true,"sensitivity":40}` | `{"enabled":true,"sensitivity":40}` |
| `{"setting":"ai_detection","class":"person","sensitivity":70,"delay_seconds":2}` | `{"sensitivity":70,"delay_seconds":2}` |
| `{"setting":"image_orientation","action":"hflip"}` | `{"hflip":true,"vflip":false,"rotation":0}` |
| `{"setting":"recording_schedule","schedule":{"kind":"continuous","enable_hours":[8,9],"disable_hours":[]}}` | `{"enabled":true,"schedules":{"continuous":"000000001100000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"}}` |
| `{"setting":"time_lapse","enabled":true,"interval_seconds":60,"format":"jpeg","stream":"sub","overwrite":true}` | `{"enabled":true,"configured":true,"interval_seconds":60,"format":"jpeg","stream":"sub","overwrite":true}` |

Spotlight duration is 1–180 seconds (default 60). IR mode is `auto` or `off`. Motion accepts either or both of `enabled` and `sensitivity` (1–50). AI accepts `sensitivity` (0–100), `delay_seconds` (0–8), or both, for `person`, `vehicle`, `pet`, `face` or `package`. Orientation actions are `hflip`, `vflip`, `clockwise` and `counterclockwise`. A recording schedule can change `enabled`, `schedule` or both. Kinds are `continuous`, `motion`, `person`, `vehicle`, `pet`, `face`, `package` and `doorbell`. Hour indices are 0–167 and must be unique across both lists. Time-lapse accepts a nonempty subset of its fields, with a 2–21600 second interval, `jpeg` or `mp4`, and `main` or `sub`.

### Pan, tilt and zoom

For each action below, the bridge command is `{"setting":"ptz","action":...}`. The WUI accepts the same action at its PTZ endpoint. The response columns show the bridge’s `result` and `verified` fields.

| Request `action` | Example `result` | `verified` |
| --- | --- | --- |
| `{"op":"move","direction":"left","duration_ms":400,"speed":16}` | `{"action":"move","direction":"left","duration_ms":400,"stopped":true,"interrupted":false}` | `false` |
| `{"op":"stop"}` | `{"action":"stop","stopped":true}` | `false` |
| `{"op":"status"}` | `{"pan":725,"tilt":110,"zoom":{"position":12,"range":{"min":0,"max":27}},"received_at":"2026-11-23T17:30:00.000Z"}` | `true` |
| `{"op":"list_presets"}` | `{"presets":[{"id":1,"name":"Door"}]}` | `true` |
| `{"op":"save_preset","preset_id":1,"name":"Door","replace":true}` | `{"action":"save_preset","preset_id":1,"presets":[{"id":1,"name":"Door"}]}` | `true` |
| `{"op":"goto_preset","preset_id":1}` | `{"action":"goto_preset","preset_id":1}` | `false` |
| `{"op":"delete_preset","preset_id":1}` | `{"action":"delete_preset","preset_id":1,"presets":[]}` | `true` |
| `{"op":"set_zoom","position":12}` | `{"action":"set_zoom","requested":12,"zoom":{"position":12,"range":{"min":0,"max":27}}}` | `true` |

`move` accepts `left`, `right`, `up`, `down`, `left_up`, `left_down`, `right_up`, `right_down`, `zoom_in` and `zoom_out` when advertised. Duration is 50–3000 ms. Optional speed is 1–64 on variable-speed cameras. Saving and deleting use slots 0–255. Bridge preset recall accepts a camera-reported ID from `list_presets`. The WUI accepts slots 0–255 for all preset actions. `set_zoom` uses the camera’s reported range. A movement acknowledgement can have `ok: true` and `verified: false`. For fresh position data send `status`.

#### `GET /api/cameras/{camera}/settings/{group}`

Reads settings from the camera for the requested group. This example uses `motion`. `values` is keyed by the setting name returned by the bridge.

```http
GET /api/cameras/front-door/settings/motion HTTP/1.1

HTTP/1.1 200 OK
Content-Type: application/json

{
  "values": {"motion_detection": {"enabled": true, "sensitivity": 40}},
  "correlation_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
}
```

| Path `{group}` | Typical fields in `values` | Command setting |
| --- | --- | --- |
| `leds` | status_led, ir_mode | `status_led`, `ir_led` |
| `spotlight` | enabled, brightness, brightness_min, brightness_max, mode | `spotlight`, `spotlight_brightness` |
| `motion` | enabled, sensitivity (under motion_detection) | `motion_detection` |
| `ai` | sensitivity, delay_seconds (under ai_person, ai_vehicle, ai_pet, ai_face or ai_package) | `ai_detection` |
| `image` | hflip, vflip, rotation | `image_orientation` |
| `recording_schedule` | enabled, schedules (168-character weekly bitmaps by kind) | `recording_schedule` |
| `time_lapse` | enabled, configured, interval_seconds, format, stream, overwrite | `time_lapse` |

#### `GET /api/cameras/{camera}/options/{group}`

Use the field descriptors from this read to build a write. Switches have `value`, numeric controls have `value`, `min` and `max`, and choices have `value` and `choices`.

```http
GET /api/cameras/front-door/options/sound HTTP/1.1

HTTP/1.1 200 OK
Content-Type: application/json

{"fields": {"speakerVolume": {"value": 60, "min": 0, "max": 100}}}
```

#### `PUT /api/cameras/{camera}/options/{group}`

Send the subset of supported fields to change.

```http
PUT /api/cameras/front-door/options/sound HTTP/1.1
Content-Type: application/json

{"fields": {"speakerVolume": 65}}

HTTP/1.1 200 OK
Content-Type: application/json

{
  "outcome": "confirmed",
  "fields": {"speakerVolume": {"value": 65, "min": 0, "max": 100}},
  "correlation_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
}
```

### Device option groups

| Path `{group}` | Write fields |
| --- | --- |
| `picture` | brightness, contrast, saturation, hue, sharpness, dayNight, antiFlicker, exposure, whiteBalance, redGain, blueGain, hdr, noiseReduction, backlightCompensation, dayTone, dayHighlight, dayShadow, nightTone, nightHighlight, nightShadow |
| `threshold` | switchThreshold |
| `sound` | speakerVolume, talkVolume, doorbellVolume |
| `recordingAudio` | mainAudio, subAudio, thirdAudio |
| `privacy` | maskEnabled. To replace rectangles send maskEnabled, maskAreas and expectedRevision together. |
| `motionZone`, `personZone`, `vehicleZone`, `petZone`, `faceZone` | cells, expectedRevision |
| `tracking` | autoTracking |
| `siren` | sirenOnEvent |
| `push`, `email`, `ftp` | pushEnabled, emailEnabled, ftpEnabled, respectively |
| `ftpDestination` | server, port, username, remoteDirectory, anonymous, ftpsOnly, password |

### Examples by option group

For these groups, send the middle column as `fields`. A successful bridge write echoes the field descriptors in the right column and adds `outcome` and `correlation_id`. A WUI write adds `operationId` to the request and puts the observed field descriptors under `data.fields`. Use the range or choices returned by the particular camera.

| `{group}` | Write `fields` | Example readback `fields` |
| --- | --- | --- |
| `picture` | `{"brightness":96}` | `{"brightness":{"value":96,"min":0,"max":255}}` |
| `threshold` | `{"switchThreshold":75}` | `{"switchThreshold":{"value":75,"min":0,"max":255}}` |
| `sound` | `{"speakerVolume":65}` | `{"speakerVolume":{"value":65,"min":0,"max":100}}` |
| `recordingAudio` | `{"subAudio":true}` | `{"subAudio":{"value":true}}` |
| `privacy` | `{"maskEnabled":false}` | `{"maskEnabled":{"value":false}}` |
| `tracking` | `{"autoTracking":true}` | `{"autoTracking":{"value":true}}` |
| `siren` | `{"sirenOnEvent":true}` | `{"sirenOnEvent":{"value":true}}` |
| `push` | `{"pushEnabled":true}` | `{"pushEnabled":{"value":true}}` |
| `email` | `{"emailEnabled":false}` | `{"emailEnabled":{"value":false}}` |
| `ftp` | `{"ftpEnabled":true}` | `{"ftpEnabled":{"value":true}}` |

Detection-zone reads return `fields: {}` plus a `zone` containing `width`, `height`, `cells` and `revision`. The bitmap has one `0` or `1` per cell in camera order. Copy the last revision into the write. Privacy-mask reads can return `privacyZones` and `privacyMasks` with `areas`, `maxAreas` and `revision`. Rectangles use the 1024 × 768 coordinate space. Stale revisions can return `409`.

#### `PUT /api/cameras/{camera}/options/{group}` · detection zone

Example change on a camera reporting a 2 × 2 grid.

```http
PUT /api/cameras/front-door/options/motionZone HTTP/1.1
Content-Type: application/json

{
  "fields": {"expectedRevision": "e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5", "cells": "1001"}
}

HTTP/1.1 200 OK
Content-Type: application/json

{
  "fields": {},
  "zone": {
    "width": 2,
    "height": 2,
    "cells": "1001",
    "revision": "3434343434343434343434343434343434343434343434343434343434343434"
  },
  "outcome": "confirmed",
  "correlation_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
}
```

#### `PUT /api/cameras/{camera}/options/{group}` · privacy masks

Use the revision and limit from this camera’s privacy read.

```http
PUT /api/cameras/front-door/options/privacy HTTP/1.1
Content-Type: application/json

{
  "fields": {
    "maskEnabled": true,
    "maskAreas": [{"id": 0, "x": 64, "y": 32, "width": 200, "height": 100}],
    "expectedRevision": "e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5"
  }
}

HTTP/1.1 200 OK
Content-Type: application/json

{
  "fields": {"maskEnabled": {"value": true}},
  "privacyZones": 1,
  "privacyMasks": {
    "areas": [{"id": 0, "x": 64, "y": 32, "width": 200, "height": 100}],
    "maxAreas": 4,
    "revision": "3535353535353535353535353535353535353535353535353535353535353535"
  },
  "outcome": "confirmed",
  "correlation_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
}
```

#### `GET /api/cameras/{camera}/options/{group}` · FTP destination

FTP destination reads expose the stored-password flag and length limit where available. The `ftp` group controls whether FTP uploads are enabled.

```http
GET /api/cameras/front-door/options/ftpDestination HTTP/1.1

HTTP/1.1 200 OK
Content-Type: application/json

{
  "fields": {
    "server": "files.example",
    "port": 21,
    "username": "camera",
    "remoteDirectory": "/images",
    "anonymous": false,
    "ftpsOnly": true
  },
  "passwordStored": true,
  "passwordMaxLength": 16
}
```

#### `PUT /api/cameras/{camera}/options/{group}` · FTP destination

When a password is stored, changing the server or port requires supplying `password` as well. A camera that masks its readback can yield `outcome: "unverified"`.

```http
PUT /api/cameras/front-door/options/ftpDestination HTTP/1.1
Content-Type: application/json

{"fields": {"remoteDirectory": "/images"}}

HTTP/1.1 200 OK
Content-Type: application/json

{
  "fields": {
    "server": "files.example",
    "port": 21,
    "username": "camera",
    "remoteDirectory": "/images",
    "anonymous": false,
    "ftpsOnly": true
  },
  "passwordStored": true,
  "passwordMaxLength": 16,
  "outcome": "confirmed",
  "correlation_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
}
```

#### `GET /api/cameras/{camera}/encoding`

Profiles describe current values, supported resolution/FPS/bitrate combinations and optional GOP limits. Bitrate uses kbps.

```http
GET /api/cameras/front-door/encoding HTTP/1.1

HTTP/1.1 200 OK
Content-Type: application/json

{
  "profiles": [
    {
      "stream": "sub",
      "current": {"width": 640, "height": 360, "fps": 10, "bitrate": 256},
      "options": [{"width": 640, "height": 360, "fps": [5, 10], "bitrates": [128, 256]}],
      "gopRange": null,
      "bitrateMode": "cbr",
      "encoderProfile": null
    }
  ]
}
```

#### `PUT /api/cameras/{camera}/encoding`

Choose the stream from the read response. Set width and height together when changing resolution. Optional `gop` follows the advertised range.

```http
PUT /api/cameras/front-door/encoding HTTP/1.1
Content-Type: application/json

{"stream": "sub", "settings": {"fps": 10, "bitrate": 256}}

HTTP/1.1 200 OK
Content-Type: application/json

{
  "outcome": "confirmed",
  "profiles": [
    {
      "stream": "sub",
      "current": {"width": 640, "height": 360, "fps": 10, "bitrate": 256},
      "options": [{"width": 640, "height": 360, "fps": [5, 10], "bitrates": [128, 256]}],
      "gopRange": null,
      "bitrateMode": "cbr",
      "encoderProfile": null
    }
  ],
  "correlation_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
}
```

#### `GET /api/cameras/{camera}/display`

Returned fields depend on the model. Keys include `deviceName`, `dateFormat`, `name`, `nameVisible`, `nameX`, `nameY`, `timeVisible`, `timeX`, `timeY`, `watermark` and `background`.

```http
GET /api/cameras/front-door/display HTTP/1.1

HTTP/1.1 200 OK
Content-Type: application/json

{"fields": {"watermark": false}}
```

#### `PUT /api/cameras/{camera}/display`

Send supported fields. Submit device-wide `deviceName` and `dateFormat` separately from channel overlay fields.

```http
PUT /api/cameras/front-door/display HTTP/1.1
Content-Type: application/json

{"fields": {"watermark": false}}

HTTP/1.1 200 OK
Content-Type: application/json

{
  "outcome": "confirmed",
  "fields": {"watermark": false},
  "correlation_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
}
```

#### `GET /api/cameras/{camera}/clock`

```http
GET /api/cameras/front-door/clock HTTP/1.1

HTTP/1.1 200 OK
Content-Type: application/json

{
  "cameraTime": "2026-11-23T09:30:00",
  "editableOptions": ["daylightSavingEnabled", "ntpEnabled"],
  "settings": {"utcOffsetMinutes": -480, "daylightSavingEnabled": false, "ntpEnabled": false}
}
```

#### `PUT /api/cameras/{camera}/clock`

`cameraTime` is local wall time to the second. A switch-only request can set `ntpEnabled`, `daylightSavingEnabled` or both if listed in `editableOptions`. With a wall time, optional `timeZone` takes a signed offset and `daylightSaving` (null or `{hours,start,end}`). Each transition has `month`, `week`, `weekday`, `hour`, `minute`. Week 5 is last and Sunday is 0.

```http
PUT /api/cameras/front-door/clock HTTP/1.1
Content-Type: application/json

{
  "cameraTime": "2026-11-23T09:30:00",
  "timeZone": {"utcOffsetMinutes": -480, "daylightSaving": null},
  "ntpEnabled": false
}

HTTP/1.1 200 OK
Content-Type: application/json

{
  "outcome": "confirmed",
  "cameraTime": "2026-11-23T09:30:00",
  "editableOptions": ["daylightSavingEnabled", "ntpEnabled"],
  "settings": {"utcOffsetMinutes": -480, "daylightSavingEnabled": false, "ntpEnabled": false},
  "correlation_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
}
```

#### `GET /api/cameras/{camera}/network`

`wifiStatus` may add `associated`, `ssid` and a signal description. Interface and signal values can be null when the camera supplies no reading.

```http
GET /api/cameras/front-door/network HTTP/1.1

HTTP/1.1 200 OK
Content-Type: application/json

{
  "link": {"address": "192.168.10.24", "netmask": "255.255.255.0", "gateway": "192.168.10.1", "dhcp": true},
  "wifi": {"ssid": "CameraLAN", "nearby": ["CameraLAN"], "configurable": true},
  "interfaces": {
    "ethernet": {"addresses": ["192.168.10.24"], "active": true, "enabled": true, "dhcp": true},
    "wifi": {"addresses": [], "active": false, "enabled": true, "dhcp": null}
  },
  "wifiSignal": null,
  "wifiSwitchAvailable": true
}
```

#### `PUT /api/cameras/{camera}/network`

For an open network send `open: true` and an empty password. `ssidVerified` describes SSID readback.

```http
PUT /api/cameras/front-door/network HTTP/1.1
Content-Type: application/json

{"ssid": "CameraLAN", "password": "example-only-passphrase", "open": false, "confirmation": true}

HTTP/1.1 200 OK
Content-Type: application/json

{"outcome": "confirmed", "ssidVerified": true, "correlation_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"}
```

#### `PUT /api/cameras/{camera}/network/wifi`

Available when the preceding network read has `wifiSwitchAvailable: true` and the wired path supports the change.

```http
PUT /api/cameras/front-door/network/wifi HTTP/1.1
Content-Type: application/json

{"enabled": false, "confirmation": true}

HTTP/1.1 200 OK
Content-Type: application/json

{"enabled": false, "outcome": "confirmed", "correlation_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"}
```

#### `GET /api/cameras/{camera}/network/rtsp`

An unreadable switch can return `{"supported":false}`.

```http
GET /api/cameras/front-door/network/rtsp HTTP/1.1

HTTP/1.1 200 OK
Content-Type: application/json

{"supported": true, "enabled": true, "port": 554}
```

#### `PUT /api/cameras/{camera}/network/rtsp`

```http
PUT /api/cameras/front-door/network/rtsp HTTP/1.1
Content-Type: application/json

{"enabled": true}

HTTP/1.1 200 OK
Content-Type: application/json

{"outcome": "confirmed", "enabled": true, "port": 554, "correlation_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"}
```

## Bridge media and talk

### Live images and audio

#### `GET /api/cameras/{camera}/preview`

The default read returns the latest cached JPEG. Add `refresh=true` to request a fresh capture. Reuse the quoted `ETag` as `If-None-Match` for a bodyless `304`. A cold cache or failed refresh can return `503`.

```http
GET /api/cameras/front-door/preview?refresh=true HTTP/1.1

HTTP/1.1 200 OK
ETag: "0101010101010101010101010101010101010101010101010101010101010101"
X-Preview-Captured-At: 2026-11-23T17:30:00.000Z
Content-Type: image/jpeg

[JPEG bytes, beginning FF D8 FF]
```

#### `HEAD /api/cameras/{camera}/preview`

The same cache headers are available without transferring a JPEG. This conditional example uses the previous response’s tag.

```http
HEAD /api/cameras/front-door/preview HTTP/1.1
If-None-Match: "0101010101010101010101010101010101010101010101010101010101010101"

HTTP/1.1 304 Not Modified
ETag: "0101010101010101010101010101010101010101010101010101010101010101"
X-Preview-Captured-At: 2026-11-23T17:30:00.000Z
```

#### `GET /api/cameras/{camera}/media/{stream}`

Choose an advertised `main`, `sub` or `third` profile. The bridge returns a multipart MJPEG response. The camera transport (`auto`, `rtsp` or `baichuan`) is configured on the bridge media profile.

```http
GET /api/cameras/front-door/media/sub HTTP/1.1

HTTP/1.1 200 OK
Content-Type: multipart/x-mixed-replace; boundary=reolink-frame

--reolink-frame
Content-Type: image/jpeg
Content-length: 12980

[JPEG frame bytes]
[more parts follow]
```

#### `GET /api/cameras/{camera}/talk`

Read the sample rate and block size before streaming. A camera without usable talk capability returns `{"available":false}`.

```http
GET /api/cameras/front-door/talk HTTP/1.1

HTTP/1.1 200 OK
Content-Type: application/json

{"available": true, "sampleRate": 16000, "blockSamples": 65}
```

#### `POST /api/cameras/{camera}/talk/{token}`

Choose a UUID for this speaker session. Reopening the active UUID returns its format. A different active holder can produce `409`. The bridge releases an idle speaker after four seconds.

```http
POST /api/cameras/front-door/talk/44444444-4444-4444-8444-444444444444 HTTP/1.1

HTTP/1.1 200 OK
Content-Type: application/json

{"available": true, "sampleRate": 16000, "blockSamples": 65}
```

#### `PUT /api/cameras/{camera}/talk/{token}`

Send Base64-encoded little-endian signed 16-bit mono PCM. This payload is one silent block of 65 samples (130 bytes). Size each real chunk to 1–4 complete negotiated blocks. Start at sequence 0 and increase by one per chunk. Already accepted sequence numbers receive `204`. A gap or invalid chunk receives `400`. Buffer pressure can return `429`.

```http
PUT /api/cameras/front-door/talk/44444444-4444-4444-8444-444444444444 HTTP/1.1
Content-Type: application/json

{
  "sequence": 0,
  "pcm": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=="
}

HTTP/1.1 204 No Content
```

#### `DELETE /api/cameras/{camera}/talk/{token}`

Release the speaker.

```http
DELETE /api/cameras/front-door/talk/44444444-4444-4444-8444-444444444444 HTTP/1.1

HTTP/1.1 204 No Content
```

### Storage, recordings and time-lapse photos

#### `GET /api/cameras/{camera}/storage`

Supply `format=true` to request a card-specific `formatSource` when the slot supports formatting. Capacities are in bytes. `cameraTime` is local wall time.

```http
GET /api/cameras/front-door/storage?format=true HTTP/1.1

HTTP/1.1 200 OK
Content-Type: application/json

{
  "cards": [
    {
      "number": 0,
      "kind": "sd",
      "mounted": true,
      "formatted": true,
      "totalBytes": 64000000000,
      "usedBytes": 1000000000,
      "freeBytes": 63000000000,
      "formatSource": "d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4"
    }
  ],
  "playbackAvailable": true,
  "cameraTime": "2026-11-23T09:30:00",
  "refreshedAt": "2026-11-23T17:30:00.000Z",
  "streams": ["main", "sub"],
  "correlation_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
}
```

#### `POST /api/cameras/{camera}/storage/format`

Formatting erases the selected card. Use its `number` and freshly returned `formatSource`. `202` acknowledges the camera command. Read storage again to check when formatting finishes.

```http
POST /api/cameras/front-door/storage/format HTTP/1.1
Content-Type: application/json

{"slot": 0, "source": "d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4", "confirm": true}

HTTP/1.1 202 Accepted
Content-Type: application/json

{"outcome": "acknowledged", "slot": 0, "correlation_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"}
```

#### `GET /api/cameras/{camera}/recordings`

Search a camera-local date. `stream` defaults to `sub` and also accepts `main`. Optional `from` and `to` are local `HH:MM:SS`. Use returned IDs for preview, metadata and video.

```http
GET /api/cameras/front-door/recordings?date=2026-11-23&stream=sub&from=09:00:00&to=10:00:00 HTTP/1.1

HTTP/1.1 200 OK
Content-Type: application/json

{
  "recordings": [
    {
      "id": "a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1",
      "start": "2026-11-23T09:30:00",
      "end": "2026-11-23T09:31:00",
      "stream": "sub",
      "sizeBytes": 1920000,
      "kind": "MD"
    }
  ],
  "truncated": false,
  "date": "2026-11-23",
  "stream": "sub",
  "correlation_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
}
```

#### `GET /api/cameras/{camera}/recordings/{recording}/video`

A full request returns `200 video/mp4`. A single byte range returns `206` or `416` for an unsatisfiable range.

```http
GET /api/cameras/front-door/recordings/a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1/video HTTP/1.1
Range: bytes=0-15

HTTP/1.1 206 Partial Content
Accept-Ranges: bytes
Content-Range: bytes 0-15/1920000
Content-Length: 16
Content-Type: video/mp4

[16 MP4 bytes, hex: 00 00 00 20 66 74 79 70 69 73 6F 6D 00 00 02 00]
```

#### `GET /api/cameras/{camera}/recordings/{recording}/preview`

Optional `offsetMs` selects 0–86400000 ms. The header gives the offset of the returned keyframe window, which can precede the requested position.

```http
GET /api/cameras/front-door/recordings/a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1/preview?offsetMs=3000 HTTP/1.1

HTTP/1.1 200 OK
X-Preview-Offset-Ms: 3000
Content-Type: image/jpeg

[JPEG bytes, beginning FF D8 FF]
```

#### `GET /api/cameras/{camera}/recordings/{recording}/metadata`

Start with `page=0` (the default). Follow `nextPage` until it is null. Per-frame `objects` can include a bounding box and its coordinate space.

```http
GET /api/cameras/front-door/recordings/a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1/metadata?page=0 HTTP/1.1

HTTP/1.1 200 OK
Content-Type: application/json

{
  "source": "recording",
  "frames": [{"offsetMs": 1000, "objects": [{"class": "person", "tracking_id": "1", "confidence": 0.9}]}],
  "truncated": false,
  "nextPage": null
}
```

#### `GET /api/cameras/{camera}/time-lapse/photos`

List a local day with optional inclusive `from` and `to`. A photo can have `capturedAt: null`.

```http
GET /api/cameras/front-door/time-lapse/photos?date=2026-11-23&from=09:00:00&to=10:00:00 HTTP/1.1

HTTP/1.1 200 OK
Content-Type: application/json

{
  "photos": [
    {
      "id": "b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2",
      "capturedAt": "2026-11-23T09:30:00"
    }
  ]
}
```

#### `GET /api/cameras/{camera}/time-lapse/photos/{photo}`

The default returns the original image converted to PNG. With `?preview=true` the response is `200 image/jpeg` (up to 480 × 270). Search again if the ten-minute reference expires.

```http
GET /api/cameras/front-door/time-lapse/photos/b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2 HTTP/1.1

HTTP/1.1 200 OK
Content-Type: image/png

[PNG bytes, beginning 89 50 4E 47 0D 0A 1A 0A]
```

### Inventory for individual file deletion

#### `GET /api/cameras/{camera}/media/capabilities`

The opaque `source` binds operations to the physical camera and channel. Each Boolean describes support for deleting that kind of file.

```http
GET /api/cameras/front-door/media/capabilities HTTP/1.1

HTTP/1.1 200 OK
Content-Type: application/json

{
  "source": "c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3",
  "video": true,
  "timeLapse": true,
  "cameraTime": "2026-11-23T09:30:00"
}
```

#### `GET /api/cameras/{camera}/media/days`

For `video`, send `month=YYYY-MM-01`. For `timeLapse`, use `?kind=timeLapse` alone.

```http
GET /api/cameras/front-door/media/days?kind=video&month=2026-11-01 HTTP/1.1

HTTP/1.1 200 OK
Content-Type: application/json

{"days": ["2026-11-23"]}
```

#### `GET /api/cameras/{camera}/media/files`

Use the selected `kind` (`video` or `timeLapse`) and date. The IDs here identify files for deletion.

```http
GET /api/cameras/front-door/media/files?kind=video&date=2026-11-23 HTTP/1.1

HTTP/1.1 200 OK
Content-Type: application/json

{
  "source": "c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3",
  "files": [
    {
      "id": "f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6",
      "completedAt": "2026-11-23T09:30:00"
    }
  ],
  "skipped": 0
}
```

#### `POST /api/cameras/{camera}/media/delete`

Deletes original files from the camera. Use the matching `source`, kind, date and up to 1024 IDs from the file inventory. `absent` identifies selections already missing.

```http
POST /api/cameras/front-door/media/delete HTTP/1.1
Content-Type: application/json

{
  "source": "c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3",
  "kind": "video",
  "date": "2026-11-23",
  "files": ["f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6"]
}

HTTP/1.1 200 OK
Content-Type: application/json

{"deleted": ["f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6"], "absent": []}
```

## Bridge enrollment and camera users

Enrollment and credential rotation use `api.storageDirectory`. An enrollment operation uses a client-generated UUIDv4 and `confirmation: true`. For `initialize`, supply the full UID. For `import`, supply the UID, hardware serial, or both. A named target can be selected by name. For a configured camera identified by address, echo the returned address and port.

#### `GET /api/enrollment-targets`

Lists named targets and whether configured cameras can be identified by address.

```http
GET /api/enrollment-targets HTTP/1.1

HTTP/1.1 200 OK
Content-Type: application/json

{
  "addressLookup": true,
  "enableRtsp": true,
  "targets": ["staging"],
  "correlation_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
}
```

#### `GET /api/enrollment-targets/identify`

Use `method=initialize` or `method=import`. Add `port` when necessary to select a unique configured endpoint.

```http
GET /api/enrollment-targets/identify?address=192.168.10.24&port=9000&method=import HTTP/1.1

HTTP/1.1 200 OK
Content-Type: application/json

{
  "target": "front-door",
  "address": "192.168.10.24",
  "port": 9000,
  "uid": "952700EXAMPLE123",
  "serial": null,
  "model": "Lumus E430",
  "correlation_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
}
```

#### `POST /api/enrollments`

`enableRtsp` is optional when advertised. Use `status` for enrollment progress. `rtsp` independently reports `enabled`, `unsupported`, `failed`, `unverified` or null.

```http
POST /api/enrollments HTTP/1.1
Content-Type: application/json

{
  "id": "33333333-3333-4333-8333-333333333333",
  "target": "front-door",
  "uid": "952700EXAMPLE123",
  "address": "192.168.10.24",
  "port": 9000,
  "method": "import",
  "enableRtsp": true,
  "confirmation": true
}

HTTP/1.1 200 OK
Content-Type: application/json

{
  "ok": true,
  "status": "active",
  "camera_id": "front-door",
  "rtsp": "enabled",
  "correlation_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
}
```

#### `GET /api/enrollments/{enrollment}`

```http
GET /api/enrollments/33333333-3333-4333-8333-333333333333 HTTP/1.1

HTTP/1.1 200 OK
Content-Type: application/json

{
  "ok": true,
  "status": "active",
  "camera_id": "front-door",
  "rtsp": "enabled",
  "correlation_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
}
```

#### `POST /api/enrollments/{enrollment}/resume`

Send `{}` to resume normally or select `import` after completing camera setup locally.

```http
POST /api/enrollments/33333333-3333-4333-8333-333333333333/resume HTTP/1.1
Content-Type: application/json

{"method": "import"}

HTTP/1.1 200 OK
Content-Type: application/json

{
  "ok": true,
  "status": "active",
  "camera_id": "front-door",
  "rtsp": "enabled",
  "correlation_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
}
```

#### `GET /api/cameras/{camera}/rotation`

```http
GET /api/cameras/front-door/rotation HTTP/1.1

HTTP/1.1 200 OK
Content-Type: application/json

{
  "ok": true,
  "status": "not-started",
  "camera_id": null,
  "correlation_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
}
```

#### `POST /api/cameras/{camera}/rotation`

Stages a new bridge-managed credential and reports progress. `restart-required` means the running worker needs to pick up the verified credential.

```http
POST /api/cameras/front-door/rotation HTTP/1.1
Content-Type: application/json

{"id": "77777777-7777-4777-8777-000000000001", "confirmation": true}

HTTP/1.1 200 OK
Content-Type: application/json

{
  "ok": true,
  "status": "restart-required",
  "camera_id": null,
  "correlation_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
}
```

#### `POST /api/cameras/{camera}/rotation/reconcile`

Checks the staged and previous credentials after interruption.

```http
POST /api/cameras/front-door/rotation/reconcile HTTP/1.1
Content-Type: application/json

{"id": "77777777-7777-4777-8777-000000000002", "confirmation": true}

HTTP/1.1 200 OK
Content-Type: application/json

{"ok": true, "status": "verified", "camera_id": null, "correlation_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"}
```

#### `GET /api/cameras/{camera}/users`

These are camera-local accounts. A blocked write can report `vault`, `rotation`, `administrator` or `firmware`. Provide the printed UID when `requiresUid` is true.

```http
GET /api/cameras/front-door/users HTTP/1.1

HTTP/1.1 200 OK
Content-Type: application/json

{
  "users": [{"username": "admin", "administrator": true, "bridge_login": true}],
  "writable": true,
  "manageAccounts": true,
  "writeBlockReason": null,
  "requiresUid": false,
  "correlation_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
}
```

#### `POST /api/cameras/{camera}/users`

For `add`, supply `role` (`user` or `administrator`) and a password of 8–31 printable ASCII characters. For `remove`, send the confirmed username and omit role and password. Both accept optional `uid`.

```http
POST /api/cameras/front-door/users HTTP/1.1
Content-Type: application/json

{
  "id": "77777777-7777-4777-8777-000000000003",
  "action": "add",
  "username": "patio-user",
  "confirmation": true,
  "role": "user",
  "password": "Example-only-456"
}

HTTP/1.1 200 OK
Content-Type: application/json

{"ok": true, "status": "verified", "camera_id": null, "correlation_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"}
```

#### `POST /api/cameras/{camera}/users` · remove

Remove a named camera user.

```http
POST /api/cameras/front-door/users HTTP/1.1
Content-Type: application/json

{
  "id": "77777777-7777-4777-8777-000000000004",
  "action": "remove",
  "username": "patio-user",
  "confirmation": true
}

HTTP/1.1 200 OK
Content-Type: application/json

{"ok": true, "status": "verified", "camera_id": null, "correlation_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"}
```

#### `POST /api/cameras/{camera}/users/password`

A change to the bridge’s own login uses the credential rotation flow.

```http
POST /api/cameras/front-door/users/password HTTP/1.1
Content-Type: application/json

{
  "id": "77777777-7777-4777-8777-000000000005",
  "username": "patio-user",
  "confirmation": true,
  "password": "Example-only-456"
}

HTTP/1.1 200 OK
Content-Type: application/json

{"ok": true, "status": "verified", "camera_id": null, "correlation_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"}
```

Possible progress states include `waiting`, `identity-mismatch`, `local-setup-required`, `recovery-required`, `in-progress`, `verified`, `restart-required`, `active`, `previous-credential-valid` and `not-started`. The camera ID can be null before activation or on a credential-only operation.

## Camera firmware

`GET /api/cameras/{id}/firmware` reads the model, hardware revision, installed firmware, update permission and any progress supplied by the camera. Add `?check=true` to query the camera's online offer. A null `updateAvailable` or `progress` means the camera did not supply that information. The contract lists cameras under `firmwareCameras`.

To start an offered update, send an authenticated `POST /api/cameras/{id}/firmware?source=<source>&confirm=true` with an empty body. For a local installation, also supply the URL-encoded original filename as `file` and the contents of the matching Reolink `.pak` or `.paks` file as `application/octet-stream` (up to 128 MiB). Use the `source` from the latest read. The bridge checks the camera identity and installed build again before starting. A local installation asks the camera to retain settings.

HTTP 202 acknowledges the transfer or start command. Read the installed version after the camera restarts to verify completion. If a connection fails partway, check the camera before making another request.

## Implementation sources

The router and serializers define the HTTP routes, WebSocket operations and typed camera controls. [HTTP and WebSocket handlers](../src/http.rs) · [REST routes](../src/api.rs) · [Camera controls](../src/reolink/control.rs)
