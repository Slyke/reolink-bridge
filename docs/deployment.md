# Docker deployment

Build from the `reolink-bridge` repository root with Podman or Docker, Cargo, Git, and `jq`. Save your release settings in a trusted `./local-only/.build.env` file.

```bash
BUILD_USERNAME=your-team
BUILD_APP_NAME=reolink-bridge
BUILD_VERSION=v0.1.0
BUILDER_BIN=podman

# Optional mirror registry and release aliases.
# MIRROR_DOMAIN=registry.example.com
# PUSH_TAGS="beta preview"
```

Authenticate the selected builder to Docker Hub and any configured mirror. Run the script from a clean release checkout.

```bash
./build.sh
```

The script checks the Cargo package version and any existing release tag before building. It pushes the version and commit-specific tags to `docker.io/$BUILD_USERNAME/$BUILD_APP_NAME`. Set `PUSH_TAGS` to a space-separated list of additional tags. After verifying a stable release, set `PUSH_TAGS=latest` to promote it. The version must use `MAJOR.MINOR.PATCH` with an optional leading `v` to publish `latest`.

Run `./build.sh --build-only` to inspect and test the commit-tagged image locally. Then run `./build.sh --push-only` from the same revision and with the same build settings to publish that image by its local image ID. Both modes accept the same config path and optional mirror arguments.

Set `MIRROR_DOMAIN` to publish the same tags to a mirror. The script attempts every tag for each configured registry even if a publish step fails. It returns the first failure's exit status after all attempts finish. The script uses the mirror hostname in temporary registry tags and removes each local mirror tag after its push attempt. To select a mirror for one run, pass the hostname after the config path.

```bash
./build.sh ./local-only/.build.env registry.example.com
```

Set `BUILDER_BIN` to `docker` or `podman`. The default is `podman`. Set `BUILD_CI_RUN_ID` to identify a CI run.

The image exposes the build inputs as `APP_BUILD_HASH`, `APP_VERSION`, `APP_BUILD_DATE`, `APP_GIT_SHA`, `APP_GIT_DIRTY`, and `APP_CI_RUN_ID`. Startup logs and both health probes report the version and short build hash.

## Run with local configuration

Mount `./local-only` to supply `bridge.json5` and persist generated TLS files. The image defaults to UID and GID `10001`. The following example uses the host account so the container can read and write a directory it owns. Replace `v0.1.0-abc1234def56` with the image tag you built, and export the environment variables referenced by your config.

```bash
docker run --rm \
  --user "$(id -u):$(id -g)" \
  --mount "type=bind,source=$PWD/local-only,target=/app/local-only" \
  -p 8080:8080 \
  -e CAMERA_PASSWORD -e CAMERA_BRIDGE_WS_TOKEN \
  -e LOG_RUNTIME_ENABLED=true -e RUNTIME_NAME=camera-bridge \
  reolink-bridge:v0.1.0-abc1234def56
```

Set `http.listen` to `0.0.0.0:8080` for this port mapping. Check `/livez` and `/readyz` over HTTP or HTTPS as configured. [Runtime metadata and logging](./configuration.md#logging) · [TLS options](./tls.md)

## Recording playback temporary files

Set `api.recordings.temporaryDirectory` to the recording workspace. It defaults to `/tmp`. Set the value to `null` to disable recorded-video playback. The runtime user needs write access to the selected directory. Missing or unwritable storage disables recorded-video playback. Card statistics and file searches are exposed through their read endpoints.

For Kubernetes, mount a disk-backed `emptyDir` at the configured path and give user/group `10001` write access, for example with `fsGroup: 10001`. The temporary cache is reconstructed after pod replacement. Retain the existing certificate volume independently.

Set `api.recordings.cache.maxBytes` and `api.recordings.cache.ttlSeconds` to bound the converted-video cache by total bytes and inactivity. Defaults are 2147483648 bytes (2 GiB) and 600 seconds (10 minutes). Playback renews the idle lifetime, and size pressure evicts the least recently played clips. Oversized clips fail playback. Allow space beyond the cache budget for native downloads, conversions, and files held by active responses. A volume size limit can cap the entire workspace.

~~~json
{
  "api": {
    "recordings": {
      "temporaryDirectory": "/tmp",
      "cache": { "maxBytes": 2147483648, "ttlSeconds": 600 }
    }
  }
}
~~~
