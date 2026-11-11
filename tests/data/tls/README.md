# TLS test fixtures

`server.pem` / `server.key` are a self-signed certificate and its unencrypted
private key, used only by the TLS tests in `tests/server.rs` and
`scripts/e2e.sh`. They protect nothing; do not use them anywhere else.

The certificate is both the server's identity and the client's trust anchor
(`--ca-cert`), so it must be a valid end-entity cert: `CA:FALSE` plus
`extendedKeyUsage=serverAuth`. A `CA:TRUE` cert in the server slot is rejected
by rustls with `CaUsedAsEndEntity`.

Regenerate (valid 10 years, RSA 2048, SAN `127.0.0.1` and `localhost`):

```bash
openssl req -x509 -newkey rsa:2048 -nodes \
  -keyout server.key -out server.pem -days 3650 \
  -subj "/CN=cache-tool-test" \
  -addext "subjectAltName=IP:127.0.0.1,DNS:localhost" \
  -addext "basicConstraints=critical,CA:FALSE" \
  -addext "keyUsage=critical,digitalSignature,keyEncipherment" \
  -addext "extendedKeyUsage=serverAuth"
```

The committed pair expires on 2036-09-13.
