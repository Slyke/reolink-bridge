# Configuration

Copy [the example](../bridge.example.json5) to `./local-only/bridge.json5`. Start the bridge from the repository root. An optional positional argument selects another config file.

```sh
cargo run --locked --release -- ./local-only/bridge.json5
```

The bridge resolves whole JSON5 string values such as `${CAMERA_PASSWORD}` from the environment. An unset variable resolves to null, and defined values keep their string type. Use JSON5 numbers and booleans for options such as ports and flags. Required secrets must be non-empty. Keep the config, private keys, and files with site-specific URLs or credentials under `./local-only/`.

## Camera read retries

Set `api.cameraReadRetries` to an integer from 0 through 10. The default of 2 allows three attempts in total. A temporary connection failure on a supported camera read opens a fresh session with a short backoff. Each operation keeps its existing deadline. Camera writes and movements use a single dispatch. A WUI administrator can override the startup value under **Settings → Private bridges → Bridge connections → HTTP API**.

## Two cameras and a callback

```json5
{
  mqtt: {
    url: "mqtt://mqtt.example.net:1883",
    baseTopic: "site/cameras",
    clientId: "reolink-bridge",
    username: "camera-bridge",
    password: "${CAMERA_BRIDGE_MQTT_PASSWORD}",
  },
  http: {
    listen: "0.0.0.0:8080",
    advertiseBaseUrl: "http://bridge.example.net:8080",
    token: "${CAMERA_BRIDGE_WS_TOKEN}",
    websocket: { path: "/ws" },
  },
  cameras: [
    {
      id: "front-door",
      host: "camera.example.net",
      username: "admin",
      password: "${CAMERA_PASSWORD}",
      eventTransport: "auto",
      coordinates: { allowVideoStream: false },
    },
    {
      id: "driveway",
      host: "nvr.example.net",
      username: "admin",
      password: "${NVR_PASSWORD}",
      channel: 2,
      onvifPort: null,
      baichuanPort: 9000,
      eventTransport: "baichuan",
      ptz: "auto",
      coordinates: { allowVideoStream: true },
    },
  ],
  callbacks: [{
    id: "automation",
    url: "${CAMERA_CALLBACK_URL}",
    cameras: ["front-door", "driveway"],
    events: ["person", "vehicle"],
  }],
}
```

- `id` identifies the camera in all the bridge interfaces. `channel` selects the zero-based camera channel on the configured host. It defaults to `0`.
- `mqtt.baseTopic` is required. The older `prefix` alias still works. Each camera MQTT session appends its ID to `clientId`.
- `onvifPort` defaults to `8000` and `baichuanPort` to `9000`. Set either port to null to disable that protocol. Baichuan handles camera control independently of the event transport.
- `eventTransport: "auto"` tries ONVIF push when `http.advertiseBaseUrl` is set, then ONVIF PullPoint, then Baichuan. Set `"onvif_push"`, `"onvif_pull"`, or `"baichuan"` to select a specific transport.
- `coordinates.allowVideoStream` opts into camera metadata. The bridge tries the Baichuan substream and can use an ONVIF RTSP metadata track. Set `coordinates.rtspUrl` to select an explicit ONVIF metadata stream on the configured camera host. Use a URL such as `rtsp://camera.example.net:554/Preview_01_sub`. The bridge uses the configured camera credentials.
- The WebSocket path defaults to `/ws`. `http.token` serves both WebSocket and the HTTP camera API. For older configs, the top-level `websocket` object with its existing `token` or `tokenEnv` remains valid. Move its path to `http.websocket.path` and its secret to `http.token` when migrating.

## PTZ and fixed cameras

`ptz: "auto"` probes the configured Baichuan channel at startup and during discovery refresh. Use `capabilities.ptz` to see what the camera reports.

| `cameras[].ptz` | Behavior |
| --- | --- |
| `"auto"` | Detect pan, tilt, zoom, presets, and speed support per channel |
| `"disabled"` | Disable PTZ for the channel |
| `"pan_tilt"` | Enable directional pan and tilt when firmware fails to advertise them |
| `"pan_tilt_zoom"` | Enable directional pan, tilt, and zoom |
| `"zoom"` | Enable directional zoom on a zoom-only camera |

Use an override on a channel known to have the corresponding motors. Preset and speed support still depend on camera discovery. Absolute zoom always requires a valid position range from the camera.

## Inbound camera notifications

`http.advertiseBaseUrl` is the base URL the camera reaches, including when the bridge runs behind a reverse proxy or WSL NAT. The bridge registers and renews ONVIF push leases and closes them on shutdown. The camera can instead be polled using `"onvif_pull"` or listened to over `"baichuan"`.

Add one of these options inside a camera entry to register a native webhook. The bridge verifies the registration by reading it back.

```json5
nativeWebhook: {
  protocol: "ha_cfg",
  token: "${CAMERA_WEBHOOK_TOKEN}",
},
```

```json5
nativeWebhook: {
  protocol: "cgi",
  cgiBaseUrl: "http://camera.example.net",
  token: "${CAMERA_WEBHOOK_TOKEN}",
},
```

Use a per-camera URL-safe token of at least 24 characters. For CGI, use the configured camera's HTTP(S) origin. The bridge uses a free webhook slot or updates its existing entry, and deliveries follow the camera's push settings. Native registration is stored on the camera. Enabling ONVIF and a native webhook together can produce events for the same alarm. [Reolink native webhook setup](https://support.reolink.com/articles/46238508855193-Overview-of-Webhook-in-Reolink-Cameras/)

## Logging

```json5
logging: {
  sinks: {
    console: { enabled: true, format: "json", levels: ["info", "warn", "error"] },
    file: { enabled: true, format: "json", path: "./local-only/bridge.log.jsonl" },
  },
},
```

Set `LOG_RUNTIME_ENABLED=true` or add `runtime: { enabled: true }` under `logging` to attach a `runtime` object to each log entry. Supply the values your deployment exposes through `RUNTIME_NAME`, `RUNTIME_DEPLOYMENT`, `RUNTIME_NAMESPACE`, `RUNTIME_IP`, `RUNTIME_IPS`, and `RUNTIME_NODE`. These map to `name`, `deployment`, `namespace`, `ip`, `ips`, and `node`. With per-entry enrichment disabled, startup includes the available values once under `context.runtime`.

Explicit JSON5 logging settings take precedence over environment variables. The runtime names take precedence over their legacy aliases within each source. [Bundled Rust logger](../src/logger/README.md)

Startup logs and health probes use `APP_VERSION` and `APP_BUILD_HASH`. For local runs without these values, the bridge uses the Cargo package version and `unknown`. [Docker build and publish example](./deployment.md)
