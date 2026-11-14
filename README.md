# Reolink MQTT Bridge

A local Rust service for powered Reolink Wi-Fi and PoE cameras. It uses LAN camera interfaces and an MQTT 5 broker. The bridge runs during internet outages while the camera and broker remain reachable on the LAN.

## Run

Run the bridge from this directory in Linux or WSL. Build it with current stable Rust and a C toolchain.

```sh
install -d -m 0700 local-only
test -e local-only/bridge.json5 || install -m 0600 bridge.example.json5 local-only/bridge.json5
read -rsp 'Camera password: ' CAMERA_PASSWORD && echo
export CAMERA_PASSWORD
export CAMERA_BRIDGE_WS_TOKEN="$(openssl rand -hex 24)"
cargo run --locked --release
```

Edit `local-only/bridge.json5` with your broker, camera addresses, and enabled ports. Keep site-specific config, callback URLs, credential files, and local logs in `local-only/`. The directory is ignored by Git. Use `cargo run --locked --release -- ./path/to/bridge.json5` to select a different file. Enable ONVIF and the Reolink TCP service on the camera as needed. For ONVIF push, set `http.advertiseBaseUrl` to an address the **camera** can reach, including when the bridge runs under WSL NAT. `eventTransport: "auto"` tries ONVIF push, PullPoint, and Baichuan in that order. Set `eventTransport` to `"onvif_push"`, `"onvif_pull"`, or `"baichuan"` to select a single transport. A null `onvifPort` or `baichuanPort` disables that protocol.

The config supports whole-value environment references such as `${CAMERA_PASSWORD}`. Values retain their string type. Unset variables resolve to null, and required secrets must be non-empty. Use JSON5 numbers and booleans for typed settings. The older `passwordEnv` and `tokenEnv` fields still work as aliases.

Keep the HTTP listener on a trusted LAN and use a TLS reverse proxy for WSS across networks. Use broker credentials and ACLs to grant writes to `<baseTopic>/<camera_id>/command/exec` only to clients authorized to control that camera. The bridge accepts `mqtt://` and `mqtts://` broker URLs.

## Topics and events

Set `mqtt.baseTopic` to a topic such as `cameras` or `site/cameras`. Each camera gets its own branch under `<baseTopic>/<camera_id>/`. Existing configs may still use `prefix`.

| Topic | Contents |
| --- | --- |
| `info`, `capabilities`, `rules` | Retained camera identity, probed features, observed topics, and reported ONVIF and Baichuan rules |
| `availability` | Retained event transport status, with an offline MQTT last will |
| `state/<type>` | Retained detection or status snapshot. `active` is true when any reported property of that type is active, and null for unknown or non-boolean status. |
| `state/settings` | Retained readback for supported light, LED, and detection settings |
| `events` | Live, non-retained events. Camera initialization goes only to snapshots. |
| `command/exec`, `command/result` | Non-retained control requests and replies |

MQTT events and downstream HTTP(S) POSTs carry the same event object. WebSocket wraps it as `{"op":"event","event":{...}}`. Each event has a camera ID, normalized type, vendor topic, source, and bridge receive time. Camera time, rule ID, tracking IDs, confidence, and boxes are included when available. Rule identifiers are scoped to their source and ONVIF configuration. Match filters on a normalized event type such as `person` or the exact vendor topic.

Set `coordinates.allowVideoStream` for a camera to enable metadata. The bridge tries the low-bandwidth Baichuan substream or an ONVIF RTSP metadata track. Set `coordinates.rtspUrl` to select an explicit ONVIF metadata stream. Camera-provided boxes are sent as `objects` events with a coordinate space and reference frame. The video and analytics frame dimensions may differ. Timestamp or reference-frame information can be used to align metadata with a video feed. Overlay support depends on the firmware.

## WebSocket and callbacks

Connect to the configured path with the HTTP header `Authorization: Bearer <token>`. Send a JSON text message for each request. Replies to valid requests echo the request `id` and contain `ok` and `correlation_id`. Subscribe for a current snapshot and subsequent updates.

```json
{"id":"watch","op":"subscribe","cameras":["front-door"],"events":["person","vehicle"]}
{"id":"now","op":"snapshot"}
{"id":"list","op":"callbacks_list"}
{"id":"add","op":"callbacks_add","callback":{"id":"automation","url":"http://automation.example.net/camera-events","cameras":["front-door"],"events":["person"]}}
{"id":"update","op":"callbacks_update","callback":{"id":"automation","url":"http://automation.example.net/new-events","events":["motion"]}}
{"id":"remove","op":"callbacks_remove","callback_id":"automation"}
```

Empty filter lists match all cameras or events. Callback changes are validated and atomically saved to the same file as formatted JSON, which is valid JSON5. Callback edits preserve environment references in unedited settings. Callback listings show URL references as written and omit query strings from literal URLs. A successful edit reformats the file as JSON and removes comments. Restart the bridge after editing the file by hand. Each callback has a bounded queue and a five-second delivery timeout. Delivery is best effort.

ONVIF push leases are renewed and closed on shutdown. To let the bridge configure a native Reolink webhook, set `nativeWebhook: { protocol: "ha_cfg", token: "${CAMERA_WEBHOOK_TOKEN}" }` on a camera that supports Baichuan HaCfg. For the standalone CGI webhook, use `protocol: "cgi"` and set `cgiBaseUrl` to the camera's HTTP(S) origin. Set the token environment variable to a random URL-safe value such as the output of `openssl rand -hex 24`. The bridge reads back its URL after registration. CGI uses a free slot or its existing entry. The native webhook is stored on the camera, and CGI deliveries follow the camera's push settings. If you enable native notifications alongside ONVIF, both transports may report an alarm. [Reolink webhook setup](https://support.reolink.com/articles/46238508855193-Overview-of-Webhook-in-Reolink-Cameras/)

## Camera controls

The `capabilities` snapshot shows settings the camera reported. Use a new command ID for each operation. For MQTT, publish JSON to `cameras/front-door/command/exec` and listen on `cameras/front-door/command/result`. For WebSocket, send the same `command` in a request with `"op":"control"` and `"camera_id":"front-door"`.

```sh
mosquitto_pub -V mqttv5 -h mqtt.example.net -q 1 -t cameras/front-door/command/exec \
  -m '{"id":"porch-led-1","command":{"setting":"status_led","enabled":false}}'
```

```json
{"id":"porch-motion-1","op":"control","camera_id":"front-door","command":{"setting":"motion_detection","sensitivity":40}}
```

| `setting` | Fields |
| --- | --- |
| `spotlight` | `enabled`, optional `duration_seconds` from 1 to 180 |
| `spotlight_brightness` | `brightness` within the reported camera range |
| `status_led` | `enabled` |
| `ir_led` | `mode` of `auto` or `off` |
| `motion_detection` | `enabled` and/or `sensitivity` from 1 to 50 on the Reolink app scale |
| `ai_detection` | `class` of `person`, `vehicle`, `pet`, `face`, or `package`, with `sensitivity` from 0 to 100 and/or `delay_seconds` from 0 to 8 |

Writes are limited to these settings. The bridge reads the camera's configuration before changing supported fields and reads it back after the write. For manual spotlight switching, `verified` is true when the camera reports the requested light state. MQTT control executes only non-retained commands.

## Diagnostics

Startup logs include the Cargo version and the Git `buildHash` captured at compile time. For builds from a source archive, supply `BUILD_HASH` during the build. The optional `logging` object follows the [StyleGuide Rust logger configuration](https://github.com/Slyke/StyleGuide/blob/77d9a18c8a38e017640728a3e1bc33fad64326b7/logging/rust/README.md). It supports structured logs, level gates, and console, file, HTTP, and syslog sinks. The shared logger is included in the source tree, so building this project requires only this checkout.

Failures returned by MQTT and WebSocket include `error`, `errorKey`, `errorCode`, and `correlation_id`. Use the correlation ID to find the corresponding log. The catalog lives in [errors.json5](./errors.json5). Run `cargo error-validate` to check the catalog. A setting write that the camera acknowledges without confirming the requested value returns `verified: false` and a warning.

`GET /livez` reports that the listener can respond. `GET /readyz` returns 200 when the configured event transports and broker sessions are ready, and 500 while a required dependency is unavailable. Both return service and build metadata, and accept a UUID in `X-Correlation-Id` or generate one.

## Verify

- Run `cargo test --locked` for sanitized protocol and configuration fixtures.
- With Mosquitto installed locally, run `cargo test --locked -- --include-ignored` to exercise the binary with a loopback broker, simulated cameras, HTTP callbacks, and authenticated WebSocket clients.
- Run `mosquitto_sub -V mqttv5 -h mqtt.example.net -v -t 'cameras/front-door/#'`. A new subscriber gets retained snapshots. Trigger and clear a detection to check live `events` and the matching `state/<type>`. Send a supported control and check its `command/result` and `state/settings` readback.

Protocol references include the [ONVIF event service](https://www.onvif.org/onvif/ver10/events/wsdl/event.wsdl), [Bairelay](https://github.com/mgc8/bairelay), [reolink-aio](https://github.com/starkillerOG/reolink_aio), and [nodelink-js Baichuan events and metadata](https://github.com/apocaliss92/nodelink-js/blob/main/documentation/baichuan-api/events.md).
