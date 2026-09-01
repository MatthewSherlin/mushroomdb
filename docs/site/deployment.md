# Deployment

---

## Terminating TLS at a reverse proxy (recommended)

The simplest and most operationally robust way to serve mushroomdb over HTTPS
is to run it on loopback and let a reverse proxy handle TLS termination.

### nginx

```nginx
server {
    listen 443 ssl;
    server_name db.example.com;

    ssl_certificate     /etc/letsencrypt/live/db.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/db.example.com/privkey.pem;

    location / {
        proxy_pass http://127.0.0.1:8080;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
    }
}
```

mushroomdb still requires `--token` (or `MUSHROOMDB_TOKEN`) when the
underlying bind is non-loopback. With a loopback bind behind nginx the token
is optional but recommended.

### Caddy

```text
db.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

Caddy manages ACME certificates automatically; no extra configuration is
needed for Let's Encrypt.

---

## Native TLS (`--features tls`)

mushroomdb includes optional rustls support behind a cargo feature flag. Use
this when a reverse proxy is unavailable or inconvenient (e.g., single-binary
deployments without a proxy tier).

### Build

```text
cargo build -p mushroomdb-cli --bin mushroomdb --features tls --release
```

### Run

```text
./target/release/mushroomdb serve ./db \
  --addr 0.0.0.0:8443 \
  --token changeme \
  --tls-cert /path/to/cert.pem \
  --tls-key  /path/to/key.pem
```

Both `--tls-cert` and `--tls-key` must be supplied together; supplying only
one is a startup error. Non-loopback binds still require `--token` or
`MUSHROOMDB_TOKEN` regardless of TLS.

### Self-signed certificates (development only)

Generate a self-signed certificate with `openssl`:

```text
openssl req -x509 -newkey rsa:4096 -keyout key.pem -out cert.pem \
  -days 365 -nodes -subj '/CN=localhost'
```

Self-signed certificates are not trusted by browsers without manual
configuration. For production, use a certificate from a trusted CA (e.g.
Let's Encrypt via `certbot` or `acme.sh`).

### Binary built without the feature

If you pass `--tls-cert` / `--tls-key` to a binary built without `--features
tls`, mushroomdb exits immediately with:

```text
this binary was built without TLS support; rebuild with --features tls
or terminate TLS at a reverse proxy (see docs/site/deployment.md)
```

---

## Loopback-first posture

mushroomdb defaults to `127.0.0.1:8080` and refuses to bind a non-loopback
address without a token. This is intentional: the DB has no network stack of
its own and should be shielded by the OS network boundary whenever possible.
See [SECURITY.md](../../SECURITY.md) for the full threat model and
vulnerability reporting process.
