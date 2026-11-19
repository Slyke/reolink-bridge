# Camera controls

Use the camera's `capabilities` snapshot to find available commands. Camera writes are limited to supported PTZ, lights, LEDs, and detection settings. The bridge requires Baichuan for control regardless of the configured event transport.

Send a unique `id` for each intended operation. Replies echo it and include `correlation_id` for log lookup. The bridge remembers the last 128 replayable command IDs per camera in memory. PTZ movement IDs are shared across MQTT, HTTP, and WebSocket. Resending an identical movement with the same remembered ID returns its prior result. Reusing its ID with a different command returns `CONTROL_ID_CONFLICT`. PTZ reads and Stop execute each time.

## MQTT

MQTT also supports a read-only [snapshot command](./snapshots.md) for Base64 PNG images.

Subscribe before publishing. Use MQTT 5 and send an unretained request.

```sh
mosquitto_sub -V mqttv5 -h mqtt.example.net -q 1 -v -t cameras/front-door/command/result
```

```sh
mosquitto_pub -V mqttv5 -h mqtt.example.net -q 1 -t cameras/front-door/command/exec \
  -m '{"id":"porch-left-1","command":{"setting":"ptz","action":{"op":"move","direction":"left","speed":16,"duration_ms":400}}}'
```

## HTTP

POST the same `id` and `command` envelope with `Content-Type: application/json` and the bearer token from `http.token`. For HTTPS use the host named by the bridge certificate. Add `--cacert` for a generated or privately signed server. See [TLS setup](./tls.md).

```sh
curl --fail-with-body \
  -H "Authorization: Bearer $CAMERA_BRIDGE_WS_TOKEN" \
  -H 'Content-Type: application/json' \
  --data '{"id":"porch-preset-1","command":{"setting":"ptz","action":{"op":"goto_preset","preset_id":1}}}' \
  http://bridge.example.net:8080/api/cameras/front-door/command/exec
```

The response includes the `X-Correlation-Id` header. HTTP status codes include `200` for accepted operations, `401` for authentication, `404` for unknown cameras or presets, `409` for conflicts or interrupted operations, `422` for invalid or unsupported control values, `502` for camera command failures, and `503` when discovery or the command worker is unavailable. Check `errorKey` for the specific reason.

## WebSocket

Connect to `/ws` with the same bearer token. Send each request as a separate JSON text message. Requests in flight can finish in a different order. Stop can be sent on the same connection while a move is pending.

```json
{"id":"porch-zoom-1","op":"control","camera_id":"front-door","command":{"setting":"ptz","action":{"op":"set_zoom","position":12}}}
```

```json
{"id":"porch-status-1","op":"control","camera_id":"front-door","command":{"setting":"ptz","action":{"op":"status"}}}
```

## PTZ actions

The following examples are the value of the `action` object inside `{"setting":"ptz","action":{...}}`.

| Operation | Example action | Details |
| --- | --- | --- |
| Timed pan | `{"op":"move","direction":"left","duration_ms":400,"speed":16}` | `left` or `right` |
| Timed tilt | `{"op":"move","direction":"up","duration_ms":200}` | `up` or `down` |
| Diagonal move | `{"op":"move","direction":"right_up","duration_ms":300}` | Also `left_up`, `left_down`, `right_down` |
| Timed zoom | `{"op":"move","direction":"zoom_out","duration_ms":250}` | `zoom_in` or `zoom_out` |
| Interrupt | `{"op":"stop"}` | Interrupts the current movement and cancels earlier queued moves |
| Read position | `{"op":"status"}` | Returns the positions the camera reports |
| List presets | `{"op":"list_presets"}` | Returns IDs and names of usable saved presets |
| Save preset | `{"op":"save_preset","preset_id":1,"name":"Door"}` | Save the current position to a numbered slot on a camera advertising `save_presets`. Use `"replace":true` to replace an occupied slot. The bridge reads the saved list back. |
| Recall preset | `{"op":"goto_preset","preset_id":1}` | Use an ID returned by `list_presets` |
| Absolute zoom | `{"op":"set_zoom","position":12}` | Use an integer inside the camera's current `zoom_range` |

A directional move starts the motor and the bridge sends Stop after `duration_ms`, from 50 to 3000 ms. Movement depends on camera speed and command latency. `speed` sets the motor speed from 1 to 64 on cameras that advertise variable speed. Omit it to use the camera default. Pan and tilt positions are read back in native device units. To return to a known view, recall a preset previously saved on the camera. [Reolink PTZ reference](https://github.com/reolink/reolink-cli/blob/main/skills/reolink-cli/references/ptz.md)

Example `capabilities` snapshot for a channel with all PTZ features. Other discovery sections can appear alongside `ptz`.

```json
{
  "camera_id": "front-door",
  "ptz": {
    "discovery": "detected",
    "source": "baichuan",
    "supported": true,
    "pan": true,
    "tilt": true,
    "diagonal": true,
    "directional_zoom": true,
    "presets": true,
    "save_presets": true,
    "speed_range": {"min": 1, "max": 64},
    "zoom_range": {"min": 0, "max": 27},
    "duration_ms": {"min": 50, "max": 3000}
  }
}
```

`pending` means the bridge is waiting for discovery. `unavailable` means the probe could not establish PTZ support. `detected` can also report `supported: false` on a fixed channel. `override` identifies a configured PTZ mode and `disabled` indicates PTZ or Baichuan is disabled for the channel. Each action checks the relevant capability, so a zoom-only channel can accept zoom and reject pan. See [camera configuration](./configuration.md#ptz-and-fixed-cameras).

Example result of a timed move. `stopped` records a Stop acknowledgement. `interrupted` indicates that Stop or shutdown arrived during the move.

```json
{
  "id": "porch-left-1",
  "ok": true,
  "verified": false,
  "result": {
    "action": "move",
    "direction": "left",
    "duration_ms": 400,
    "stopped": true,
    "interrupted": false
  },
  "correlation_id": "c245aed7-bbd2-460f-a693-d7d341a630bb"
}
```

Directional moves and preset recall return `verified: false` because the acknowledgement does not establish the final position. Use `status` for a new readback. Absolute zoom reports `verified: true` when the camera reports the target position during readback. The retained `state/ptz` snapshot contains the latest observed position sample from `status` or `set_zoom`. Check `received_at` before displaying it as current.

```json
{
  "camera_id": "front-door",
  "position": {
    "pan": 725,
    "tilt": 110,
    "zoom": {"position": 12, "range": {"min": 0, "max": 27}},
    "received_at": "2026-01-20T08:40:14.220Z"
  }
}
```

A fixed channel reports an error such as this through any command interface.

```json
{
  "id": "fixed-move-1",
  "ok": false,
  "error": "The camera does not report support for this PTZ operation.",
  "errorKey": "PTZ_UNSUPPORTED",
  "errorCode": "B0EE21BEB0D1C40C",
  "correlation_id": "9b26a0ce-ae79-4092-a6c7-2b723a827757"
}
```

## Lights, LEDs, and detection settings

Put each command object below in the same envelope as a PTZ command.

| Example command | Details |
| --- | --- |
| `{"setting":"spotlight","enabled":true,"duration_seconds":30}` | Duration 1–180 seconds, default 60. Use `"enabled":false` to turn it off. |
| `{"setting":"spotlight_brightness","brightness":60}` | Use the camera's reported brightness range. |
| `{"setting":"status_led","enabled":false}` | Switch the status LED. |
| `{"setting":"ir_led","mode":"auto"}` | Use `auto` or `off`. |
| `{"setting":"motion_detection","enabled":true,"sensitivity":40}` | Send either or both fields. Sensitivity uses the Reolink app scale of 1–50. |
| `{"setting":"ai_detection","class":"person","sensitivity":70,"delay_seconds":2}` | Send sensitivity 0–100, delay 0–8 seconds, or both. Classes are `person`, `vehicle`, `pet`, `face`, and `package` where supported. |

Setting replies contain `state` with readback. Readbacks keep useful `vendor` fields and update `state/settings`. If a write was acknowledged but the value could not be confirmed, the reply can contain `verified: false` and a warning.

## Image orientation

Read `GET /api/cameras/{id}/settings/image` for current orientation. Use the existing typed command endpoint with a fresh request ID to change it.

~~~json
{"id":"orientation-1","command":{"setting":"image_orientation","action":"hflip"}}
~~~

Actions are `hflip`, `vflip`, `clockwise` and `counterclockwise`. The bridge reads and verifies the camera setting around each write and preserves unrelated image fields. Rotation is available when the camera reports support for 90-degree steps. [HTTP API](./api.md#image-orientation)
