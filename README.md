# Reolink Bridge

A local Rust service for powered Reolink Wi-Fi and PoE cameras. It publishes camera events, capabilities, rules, and state to MQTT 5. Control supported lights, LEDs, detection settings, and PTZ over MQTT, HTTP, or WebSocket. It uses LAN camera interfaces and works during internet outages while the cameras and broker remain reachable.

## Run

Use stable Rust and a C toolchain on Linux or WSL. Run from this directory.

```sh
install -d -m 0700 ./local-only
test -e ./local-only/bridge.json5 || install -m 0600 ./bridge.example.json5 ./local-only/bridge.json5
read -rsp 'Camera password: ' CAMERA_PASSWORD && echo
export CAMERA_PASSWORD
export CAMERA_BRIDGE_WS_TOKEN="$(openssl rand -hex 24)"
cargo run --locked --release
```

Edit `./local-only/bridge.json5` with your camera and broker settings. Enable ONVIF or the Reolink TCP service on the camera as needed. Enable the Reolink TCP service for camera controls. Keep real endpoints, credentials, private keys, and local logs in the gitignored `./local-only/` directory.

`mqtt.baseTopic` puts each camera's topics under `<baseTopic>/<camera_id>/`. Subscribe to `cameras/front-door/#` for the example camera. Send commands to `cameras/front-door/command/exec` and watch `cameras/front-door/command/result`.

`http` configures a shared listener for the API, WebSocket, camera notifications, and health probes. Enable `http.tls` for HTTPS and WSS with a mounted or locally generated certificate. Use `mqtts://` for MQTT TLS. See the [configuration and payload examples](./docs/README.md).

## Container

Use [the Docker build and publish example](./docs/deployment.md) from this repository's root. Mount the runtime config and any certificates under `/app/local-only`.

## Verify

- Run `cargo test --locked` for protocol and configuration fixtures.
- With Mosquitto installed, run `cargo test --locked -- --include-ignored` for the loopback camera, broker, HTTP, WebSocket, and TLS tests.
- To include the real WUI, run `npm ci && npm run build` in the sibling [WUI repository](https://github.com/Slyke/reolink-wui), then set `WUI_TEST_PROJECT` to its absolute path and `WUI_TEST_NODE` to the absolute Node 24 executable before running the suite with ignored tests. This exercises callback delivery, all three WUI event sources, commands, snapshots and media on loopback.
- Run `cargo error-validate` to validate the [structured error catalog](./errors.json5).
- Check `/livez` for process liveness and `/readyz` for camera and MQTT readiness. Subscribe to the camera's MQTT branch, trigger and clear a detection, and check `events` and the matching retained state.

Protocol references include the [ONVIF event service](https://www.onvif.org/onvif/ver10/events/wsdl/event.wsdl), [Reolink CLI PTZ reference](https://github.com/reolink/reolink-cli/blob/main/skills/reolink-cli/references/ptz.md), [Bairelay](https://github.com/mgc8/bairelay), [reolink-aio](https://github.com/starkillerOG/reolink_aio), and [nodelink-js Baichuan events and metadata](https://github.com/apocaliss92/nodelink-js/blob/main/documentation/baichuan-api/events.md).
