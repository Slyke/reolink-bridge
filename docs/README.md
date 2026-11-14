# Examples

- [Configuration](./configuration.md) for camera channels, event transports, metadata, native camera webhooks, callback destinations, and logs.
- [MQTT topics and event payloads](./events.md) for live detections, retained state, rules, and bounding boxes.
- [Camera controls](./controls.md) for PTZ, lights, LEDs, motion and AI settings, and the MQTT, HTTP, and WebSocket command envelopes.
- [WebSocket, callbacks, and health probes](./interfaces.md) for subscriptions, callback management, and correlation IDs.
- [MQTT snapshots](./snapshots.md) for Base64 PNG captures and saving the image to a file.
- [TLS](./tls.md) for MQTT broker trust and HTTPS/WSS with generated or mounted certificates.

Examples use `front-door` as the camera ID and `cameras` as the MQTT base topic. Replace the example hostnames with the addresses your clients and cameras can reach.
