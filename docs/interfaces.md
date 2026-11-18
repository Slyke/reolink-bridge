# WebSocket, callbacks, and probes

The WebSocket path defaults to `/ws` on the HTTP listener. Connect with `Authorization: Bearer <token>` using `http.token`. For example, with `wscat` installed:

```sh
wscat --connect ws://bridge.example.net:8080/ws \
  --header "Authorization: Bearer $CAMERA_BRIDGE_WS_TOKEN"
```

For HTTPS setups, use the [WSS example](./tls.md#generate-a-certificate-for-https-and-wss).

## Watch changes and fetch snapshots

Send each request as a separate text message. This subscription receives current snapshots followed by live updates.

```json
{"id":"watch-1","op":"subscribe","cameras":["front-door"],"events":["person","tns1:RuleEngine/CellMotionDetector/Motion"]}
```

Example initial response with one snapshot shown.

```json
{
  "id": "watch-1",
  "ok": true,
  "correlation_id": "426c5846-fb69-4641-9d5c-fea702be9121",
  "snapshots": [{
    "camera_id": "front-door",
    "topic": "availability",
    "payload": {
      "camera_id": "front-door", "online": true, "transport": "onvif_push",
      "reason": "subscribed", "updated_at": "2026-01-20T08:40:08.824Z"
    }
  }]
}
```

An empty camera or event filter matches all. The event filter applies to live events and detection state. Other snapshots for the selected cameras still arrive. `snapshot` fetches current values with the existing subscription filter, or all values before subscribing.

```json
{"id":"now-1","op":"snapshot"}
```

Subsequent updates use `{"op":"event","event":{...}}` and `{"op":"snapshot","snapshot":{...}}`. If the socket falls behind, `{"op":"gap","snapshots":[...]}` refreshes current state. Use `id` to match direct replies and treat updates as separate messages. See [event JSON](./events.md) and [control requests](./controls.md).

## HTTP event subscriptions

Use authenticated `GET /api/events` for the same cached snapshots and live events over HTTP SSE. It supports camera/event filtering, scoped API credentials, eight-second heartbeats, and backlog gap messages. See the [HTTP event stream contract](./api.md#http-event-stream).

## Manage downstream callbacks

Use the [HTTP API](./api.md#authorization-and-enrollment) to manage callbacks with an approved receiver, or use the WebSocket commands below.

Each callback receives an HTTP(S) POST with `Content-Type: application/json` and the same event object sent on MQTT. Match by normalized type or exact vendor topic. Omit filters or send empty arrays to match all. Each destination has a bounded queue and a five-second delivery timeout. The receiver should return a 2xx response. Delivery is best effort.

List, add, edit, and remove callbacks through WebSocket.

```json
{"id":"list-1","op":"callbacks_list"}
```

```json
{"id":"add-1","op":"callbacks_add","callback":{"id":"automation","url":"http://automation.example.net/camera-events","cameras":["front-door"],"events":["person"]}}
```

```json
{"id":"update-1","op":"callbacks_update","callback":{"id":"automation","url":"http://automation.example.net/new-events","events":["motion","vehicle"]}}
```

```json
{"id":"remove-1","op":"callbacks_remove","callback_id":"automation"}
```

Example reply after adding a destination.

```json
{
  "id": "add-1",
  "ok": true,
  "callbacks": [{
    "id": "automation", "url": "http://automation.example.net/camera-events",
    "cameras": ["front-door"], "events": ["person"]
  }],
  "correlation_id": "40168df6-140a-425e-a87a-7807ca38df34"
}
```

Changes are validated and saved atomically in the same config file. The bridge writes formatted JSON, which is valid JSON5, and removes comments. Unedited environment references are preserved. Callback listings show environment references as written and omit query strings from literal URLs. Restart after editing the config file manually.

## Health and correlation IDs

`GET /livez` checks whether the listener responds. `GET /readyz` returns 200 after all configured camera event transports and MQTT sessions are ready, or 500 while a required dependency is unavailable. The probes use the same listener as WebSocket and the API.

```sh
curl --fail-with-body \
  -H 'X-Correlation-Id: 1dbde7cd-7d1f-40d8-a530-45c2bcc98d1a' \
  http://bridge.example.net:8080/readyz
```

```json
{
  "ok": true,
  "probe": "readiness",
  "service": "reolink-bridge",
  "version": "0.1.0",
  "buildHash": "0123456789ab",
  "correlation_id": "1dbde7cd-7d1f-40d8-a530-45c2bcc98d1a",
  "checks": {"cameras": true, "mqtt": true}
}
```

HTTP echoes a valid UUID from `X-Correlation-Id` or generates one. Request failures return `error`, `errorKey`, `errorCode`, and `correlation_id`. Use the correlation ID to locate the matching structured log.
