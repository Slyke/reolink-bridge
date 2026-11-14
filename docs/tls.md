# TLS

The MQTT broker and the shared HTTP/WebSocket listener have independent TLS settings. Deploy PEM files with the bridge under `./local-only/` or at a mounted path. Relative TLS file paths resolve from the bridge's working directory.

## MQTT broker using system trust

Use `mqtts://`. The default TLS port is 8883 and the bridge uses the system trust store.

```json5
mqtt: {
  url: "mqtts://mqtt.example.net:8883",
  baseTopic: "cameras",
  username: "camera-bridge",
  password: "${CAMERA_BRIDGE_MQTT_PASSWORD}",
},
```

The username and password are optional together. Set broker ACLs so control clients can write only the authorized cameras' `command/exec` topics.

## MQTT broker with a private CA or self-signed certificate

Supply a PEM bundle with the issuing CA certificates or the broker's self-signed server certificate. This is the MQTT client's trust set when configured. Use a broker URL whose hostname or IP matches a Subject Alternative Name on the certificate. Certificate and hostname verification stay enabled.

```json5
mqtt: {
  url: "mqtts://mqtt.example.net:8883",
  baseTopic: "cameras",
  tls: { caFile: "./local-only/mqtt/broker-ca.pem" },
},
```

```sh
mosquitto_sub -V mqttv5 -h mqtt.example.net -p 8883 \
  --cafile ./local-only/mqtt/broker-ca.pem -v -t 'cameras/front-door/#'
```

## Generate a certificate for HTTPS and WSS

Providing `http.tls` without file paths generates an ECDSA P-256 key and a one-year self-signed server certificate on first start. Add every DNS name or IP that clients use to `serverNames`. If omitted, the bridge uses the advertised host, the concrete listen IP, or `localhost`, in that order.

```json5
http: {
  listen: "0.0.0.0:8443",
  advertiseBaseUrl: "https://bridge.example.net:8443",
  token: "${CAMERA_BRIDGE_WS_TOKEN}",
  websocket: { path: "/ws" },
  tls: { serverNames: ["bridge.example.net", "localhost", "127.0.0.1"] },
},
```

The generated files are `./local-only/https/server.crt` and `./local-only/https/server.key`. For a config directly inside a `local-only` directory, the bridge writes next to that config under `https/`. Otherwise it uses `./local-only/https/` from the working directory. The pair is reused on restart. To rotate it or change the SANs, stop the bridge, move the pair out of its generation directory, restart, and update client trust with the new certificate. Copy only `server.crt` to clients.

```sh
curl --fail-with-body --cacert ./local-only/https/server.crt \
  https://bridge.example.net:8443/livez
```

```sh
wscat --ca ./local-only/https/server.crt \
  --connect wss://bridge.example.net:8443/ws \
  --header "Authorization: Bearer $CAMERA_BRIDGE_WS_TOKEN"
```

## Use a mounted certificate and key

Supply both a PEM certificate chain in leaf-first order and the PEM private key. Restart after replacing either file.

```json5
http: {
  listen: "0.0.0.0:8443",
  advertiseBaseUrl: "https://bridge.example.net:8443",
  token: "${CAMERA_BRIDGE_WS_TOKEN}",
  websocket: { path: "/ws" },
  tls: {
    certFile: "${BRIDGE_TLS_CERT_FILE}",
    keyFile: "${BRIDGE_TLS_KEY_FILE}",
  },
},
```

For example, set `BRIDGE_TLS_CERT_FILE=./local-only/https/fullchain.pem` and `BRIDGE_TLS_KEY_FILE=./local-only/https/privkey.pem` before starting the bridge.

The API, WebSocket, health checks, and camera notification routes share `http.listen`. With TLS enabled, clients connect via HTTPS or WSS on that port. With TLS omitted, clients use HTTP or WS. `http.advertiseBaseUrl` controls the address registered with cameras. A push-enabled camera must be able to reach the advertised URL and trust its certificate when it uses HTTPS. If the camera cannot use that listener, choose ONVIF PullPoint, Baichuan events, or an advertised address routed by a LAN proxy to the bridge's ingest endpoints.
