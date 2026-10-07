# Docker

Each release publishes a multi-platform image (`linux/amd64`,
`linux/arm64`):

```text
ghcr.io/tee8z/satchel:<version>   for example ghcr.io/tee8z/satchel:0.1.0
ghcr.io/tee8z/satchel:latest
```

The image contains the binary from the release archive, checked against
the release's SHA-256 checksum file, on `gcr.io/distroless/cc-debian13`:
glibc, CA certificates, and nothing else (no shell). It runs as uid and gid
`65532`.

| Path or setting | Meaning |
| --- | --- |
| `/etc/satchel/config.toml` | The configuration (`SATCHEL_CONFIG`). Mount it read-only. |
| `/etc/satchel/*` | Credentials named by relative paths in the configuration: `tls.cert`, the macaroon, `admin-password.hash`. |
| `/data` | Volume for the SQLite database (`/data/wallet.db`). |
| `8095` | The HTTP listener. Put an HTTPS reverse proxy in front of it. |
| `SATCHEL_SERVER__BIND_ADDRESS=0.0.0.0:8095` | Set by the image; overrides `server.bind_address`. |
| `SATCHEL_SERVER__DATABASE_PATH=/data/wallet.db` | Set by the image; overrides `server.database_path`. |

Any other key can be set with `-e SATCHEL_<SECTION>__<KEY>=value`; see
[configuration.md](configuration.md#environment-overrides).

## Run

```sh
mkdir satchel-config && cd satchel-config
# Write config.toml (start from example/config.toml.example) and copy in
# tls.cert and the macaroon (see operating.md for baking it).
docker run --rm -i ghcr.io/tee8z/satchel:0.1.0 hash-password \
  < operator-password.txt > admin-password.hash
chmod 0644 config.toml tls.cert admin-password.hash
chmod 0640 wallet.macaroon && sudo chgrp 65532 wallet.macaroon

docker run -d --name satchel --restart unless-stopped \
  -v "$PWD:/etc/satchel:ro" \
  -v satchel-data:/data \
  -p 127.0.0.1:8095:8095 \
  ghcr.io/tee8z/satchel:0.1.0
docker logs -f satchel
```

The files in `/etc/satchel` must be readable by uid or gid `65532`. The
macaroon can spend the node's channel balance, so give it to that group
rather than making it world-readable. A named volume for `/data` is created
with the right owner; for a bind mount, `chown 65532:65532` the directory
first.

## Reaching LND from the container

`lnd.rest_host` must be an `IP:port`, and LND's certificate must cover that
IP. Two ways that work:

- **Host network** (Linux): run the container with `--network host` and use
  `rest_host = "127.0.0.1:8080"`. Drop `-p`; then set
  `SATCHEL_SERVER__BIND_ADDRESS=127.0.0.1:8095` so Satchel does not listen
  on every interface.
- **Bridge network**: make LND listen on an address the container can reach,
  such as the Docker bridge gateway (`restlisten=172.17.0.1:8080` and
  `tlsextraip=172.17.0.1` in `lnd.conf`, then let LND regenerate its
  certificate), and use `rest_host = "172.17.0.1:8080"`. If LND runs in
  Compose too, give it a fixed address on a user-defined network, as
  [`examples/regtest`](../examples/regtest) does.

Behind a reverse proxy on the host, keep `server.client_ip_header` set to the
header the proxy writes ([operating.md](operating.md#reverse-proxy)).

## Compose

```yaml
services:
  satchel:
    image: ghcr.io/tee8z/satchel:0.1.0
    restart: unless-stopped
    volumes:
      - ./satchel-config:/etc/satchel:ro
      - satchel-data:/data
    ports:
      - "127.0.0.1:8095:8095"
    environment:
      SATCHEL_LOG_JSON: "true"

volumes:
  satchel-data:
```

The image has no shell or HTTP client, so container health checks cannot
run inside it. Probe `http://127.0.0.1:8095/healthz` from the host or the
reverse proxy instead. To scrape metrics, set
`SATCHEL_SERVER__METRICS_ADDRESS=0.0.0.0:9095` and publish that port only
to your monitoring network.

## Backups

The image has no `sqlite3`. Run the online backup from a throwaway
container that mounts the same volume (read-write: SQLite readers of a WAL
database need the shared-memory file):

```sh
docker run --rm -v satchel-data:/data -v "$PWD:/backup" alpine:3 \
  sh -c 'apk add --no-cache sqlite >/dev/null && sqlite3 /data/wallet.db ".backup /backup/wallet.db"'
```

See [operating.md](operating.md#backups) for restoring.

## Build the image yourself

The Dockerfile packages release archives; it does not compile Satchel.
Download the archives and checksum files of a release into `dist/`, then
build:

```sh
version=0.1.0
mkdir -p dist
gh release download "v$version" -R tee8z/satchel -D dist \
  -p "satchel-$version-*-linux.tar.gz" -p "satchel-$version-*-linux.tar.gz.sha256"
docker buildx build --build-arg SATCHEL_VERSION="$version" \
  --platform linux/amd64,linux/arm64 -t satchel:"$version" .
```

For one platform, download only that archive and pass only that platform,
with `--load` to keep the image locally. The build checks every archive
against its `.sha256` file and fails if one does not match.

The release workflow does the same with the archives it has just built and
verified, and pushes `ghcr.io/tee8z/satchel:<version>` and `:latest` only
when it publishes a release.
