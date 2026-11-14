# MQTT topics and events

Use `mqtt.baseTopic` to name the MQTT tree. With `baseTopic: "cameras"` and camera ID `front-door`, subscribe with:

```sh
mosquitto_sub -V mqttv5 -h mqtt.example.net -v -t 'cameras/front-door/#'
```

| Suffix | Retained | Payload |
| --- | --- | --- |
| `info` | Yes | Camera identity and firmware reported by available interfaces |
| `capabilities` | Yes | Discovery results, setting support, observed event types, PTZ, and metadata stream availability |
| `rules` | Yes | Rule definitions grouped by source |
| `availability` | Yes | Camera event transport status or the bridge's MQTT last will |
| `state/<type>` | Yes | Current properties and their most recent event |
| `state/settings` | Yes | Readback for supported light, LED, and detection settings |
| `state/ptz` | Yes | Last measured PTZ position after a position read or absolute zoom |
| `events` | No | Live normalized camera events |
| `command/exec` | No | A camera control request |
| `command/result` | No | The corresponding result, including requested Base64 PNG snapshots |

Snapshot sections appear as the bridge obtains data from the camera. For unavailable data, subscribe for updates or check `availability`. Camera initialization updates state without firing a live event. The bridge publishes snapshots and results at QoS 1 and live events at QoS 0. Use retained state to recover the latest values after reconnecting.

## Detection messages

A person detection on `cameras/front-door/events` can look like this.

```json
{
  "camera_id": "front-door",
  "type": "person",
  "source": "onvif",
  "vendor_topic": "tns1:RuleEngine/MyRuleDetector/PeopleDetect",
  "received_at": "2026-01-20T08:40:09.009Z",
  "camera_time": "2026-01-20T08:40:09Z",
  "active": true,
  "operation": "Changed",
  "vendor": {
    "topic_dialect": "http://www.onvif.org/ver10/tev/topicExpression/ConcreteSet",
    "topic_namespace": "http://www.onvif.org/ver10/topics",
    "message": {
      "name": "Message",
      "namespace": "http://www.onvif.org/ver10/schema",
      "attributes": {"UtcTime": "2026-01-20T08:40:09Z", "PropertyOperation": "Changed"},
      "children": [{
        "name": "Data",
        "namespace": "http://www.onvif.org/ver10/schema",
        "children": [{
          "name": "SimpleItem",
          "namespace": "http://www.onvif.org/ver10/schema",
          "attributes": {"Name": "State", "Value": "true"}
        }]
      }]
    }
  }
}
```

A clear has the same event type and `"active": false`. When the camera supplies a rule ID, the event includes `rule_id` as a string. The excerpts below show the fields clients can match across several messages.

| Event | Payload excerpt |
| --- | --- |
| Person cleared | `{"type":"person","active":false,"source":"onvif","vendor_topic":"tns1:RuleEngine/MyRuleDetector/PeopleDetect"}` |
| Motion with rule ID | `{"type":"motion","active":true,"source":"onvif","vendor_topic":"tns1:RuleEngine/CellMotionDetector/Motion","rule_id":"000"}` |
| Vehicle | `{"type":"vehicle","active":true,"source":"baichuan","vendor_topic":"AlarmEvent/vehicle","channel":0}` |
| Pet | `{"type":"pet","active":true,"source":"baichuan","vendor_topic":"AlarmEvent/dog_cat","channel":0}` |
| Doorbell from native webhook | `{"type":"doorbell","source":"reolink_webhook","vendor_topic":"HaCfg/wake","channel":0}` |
| Unrecognized topic | `{"type":"vendor_parcelmoved_37d938e8b76e","source":"onvif","vendor_topic":"tns1:RuleEngine/FutureDetector/ParcelMoved"}` |

Filter using `type` or the exact `vendor_topic`. `received_at` comes from the bridge. `camera_time` is present when the camera supplied a usable timestamp. Some notifications have no Boolean state, so `active` can be absent from the event.

The retained `state/person` snapshot includes `last` with the same event payload and a `properties` array. Here is an excerpt without the `last` event body.

```json
{
  "camera_id": "front-door",
  "type": "person",
  "active": false,
  "properties": [{
    "source": "onvif",
    "vendor_topic": "tns1:RuleEngine/MyRuleDetector/PeopleDetect",
    "rule_id": null,
    "channel": null,
    "active": false,
    "received_at": "2026-01-20T08:40:12.009Z",
    "camera_time": "2026-01-20T08:40:12Z"
  }]
}
```

If any property of the type is active, the snapshot is active. When all reported Boolean properties are clear it is false. When no Boolean state is known it is null.

## Availability and rule snapshots

```json
{
  "camera_id": "front-door",
  "online": true,
  "transport": "onvif_push",
  "reason": "subscribed",
  "updated_at": "2026-01-20T08:40:08.824Z"
}
```

On an unexpected broker disconnect, the retained last will is `{"camera_id":"front-door","online":false,"reason":"bridge_disconnected"}`.

Example excerpt from `rules`. The source and configuration identify the scope for rule IDs. Geometry and vendor parameters vary by rule type.

```json
{
  "camera_id": "front-door",
  "onvif": [{
    "configuration_id": "analytics",
    "source": "onvif",
    "id": "000",
    "name": "000",
    "type": "CellMotionDetector",
    "enabled": true,
    "parameters": {
      "name": "Parameters",
      "children": [{"name": "SimpleItem", "attributes": {"Name": "Enable", "Value": "true"}}]
    }
  }]
}
```

## Coordinates

With `coordinates.allowVideoStream` enabled on a supported camera, `type: "objects"` includes reported detections and bounding boxes. When available, tracking IDs identify camera tracks across frames. This example uses normalized coordinates in the Reolink AI frame.

```json
{
  "camera_id": "front-door",
  "type": "objects",
  "source": "baichuan_metadata",
  "vendor_topic": "BcMedia/additionalHeader",
  "received_at": "2026-01-20T08:40:09.059Z",
  "channel": 0,
  "objects": [{
    "class": "person",
    "confidence": 0.91,
    "bounding_box": {
      "left": 0.3, "top": 0.2, "right": 0.5, "bottom": 0.7,
      "coordinate_space": "normalized_top_left",
      "reference_frame": "Reolink AI processing frame (substream)",
      "frame_width": 640,
      "frame_height": 360
    }
  }],
  "vendor": {"stream": "sub", "frame_microseconds": 411691, "video_dimensions": [640, 360]}
}
```

For `normalized_top_left`, multiply horizontal and vertical values by the corresponding reference frame dimensions. `pixels_top_left` already uses pixels. Check the reference frame and timestamps before aligning boxes to video. On a frame that clears all tracked objects, `objects` may be omitted.

The bridge sends the same normalized event JSON to downstream callbacks and in WebSocket event messages. See [WebSocket and callbacks](./interfaces.md).
