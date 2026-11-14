# MQTT camera snapshots

A snapshot request captures one JPEG from the Reolink main stream through Baichuan. The bridge returns it as a standard Base64-encoded PNG with a maximum size of 1920×1080 pixels. It preserves the aspect ratio, keeps smaller frames at their original dimensions when they fit, and downsizes further when the encoded reply exceeds the MQTT budget. Capturing works on supported fixed and PTZ cameras with `baichuanPort` enabled.

## Request and save a PNG

First, subscribe to the camera's result topic. This example receives one reply. For a shared topic, filter replies by the request ID in your subscriber before decoding the image. Run it from the repository root and use a fresh request ID for each capture.

```sh
install -d -m 0700 ./local-only/snapshots
umask 077
set -o pipefail
mosquitto_sub -V mqttv5 -h mqtt.example.net -q 1 \
  -C 1 -W 45 -t cameras/front-door/command/result |
  jq -er 'select(.id == "front-door-image-1" and .ok) | .result.data_base64' |
  base64 --decode > ./local-only/snapshots/front-door.png
```

In another terminal, publish the request. Use the same camera ID and request ID as above.

```sh
mosquitto_pub -V mqttv5 -h mqtt.example.net -q 1 \
  -t cameras/front-door/command/exec \
  -m '{"id":"front-door-image-1","command":{"setting":"snapshot"}}'
```

The reply is a non-retained MQTT message. `result.data_base64` contains the complete PNG in one string. Example reply with the image data shortened for display.

```json
{
  "id": "front-door-image-1",
  "ok": true,
  "correlation_id": "8f55147b-5790-4076-b717-08b2b699326a",
  "result": {
    "action": "snapshot",
    "content_type": "image/png",
    "encoding": "base64",
    "width": 1920,
    "height": 1080,
    "original_width": 2560,
    "original_height": 1440,
    "resized": true,
    "captured_at": "2026-01-20T08:40:09.126Z",
    "data_base64": "iVBORw0KGgo..."
  }
}
```

The bridge handles one capture at a time. If a capture is already in progress, the reply has `errorKey: "SNAPSHOT_CAPTURE_BUSY"`. Send another request after the first capture finishes. Duplicate MQTT deliveries can request another capture, so consumers can filter or deduplicate replies using `id`. Use broker ACLs to restrict access to `command/result`, which carries the image. Keep downloaded PNG files under `local-only/`.

## Size limits

The default maximum JSON reply is 1,000,000 bytes. Set `mqtt.snapshotMaxPayloadBytes` to a smaller limit when the broker or subscriber requires it. Increasing it can preserve 1080p on more detailed images. The range is 4096 through 25,165,824 bytes.

```json5
mqtt: {
  url: "mqtts://mqtt.example.net:8883",
  baseTopic: "cameras",
  snapshotMaxPayloadBytes: 1500000,
},
```

If the broker advertises a smaller MQTT 5 Maximum Packet Size, the bridge accounts for the topic and packet headers before fitting the image. Some brokers also set a payload limit that is separate from the advertised packet size, such as Mosquitto's `message_size_limit`. Set the bridge's payload limit to fit that setting and your subscriber's limit. Base64 adds about one third to the PNG size before the JSON envelope. [Mosquitto packet and payload settings](https://mosquitto.org/man/mosquitto-conf-5.html)
