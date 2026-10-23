# Railway config-as-code cannot name an image, so this one-line-FROM Dockerfile
# pulls the published image. Use it from a fork or clone of the repository
# (Railway builds railway.json and this file from the repo).
#
# SINGLE WRITER - replicas must be 1 (railway.json sets numReplicas to 1).
#
# Service variables to set in the Railway dashboard (none are stored here):
#   MUSHROOMDB_TOKEN   <set-me>   required; the server refuses a public bind without it
#   PORT               8080       the port the server binds; the health check uses it too
#   RAILWAY_RUN_UID    0          Railway volumes mount root-owned and the image runs as
#                                 uid 65532, so the server cannot write /data without it.
#                                 This runs the container as root on Railway (a platform
#                                 setting; the image is unchanged). UNVERIFIED without a
#                                 real deploy.
#
# Bolt is OFF in this template: Railway reaches a service only through its HTTP
# domain unless you enable a TCP proxy, and the TCP proxy is not documented to
# encrypt (UNVERIFIED), so this file never opens it for you. To use Bolt,
# enable the proxy yourself (service Settings > Networking > TCP Proxy, enter
# internal port 7687), then set these service variables:
#   MUSHROOMDB_BOLT_PORT             7687   turns the listener on
#   MUSHROOMDB_BOLT_ADVERTISED_HOST  <the proxy domain Railway shows you>
#   MUSHROOMDB_BOLT_ADVERTISED_PORT  <the proxy port Railway shows you>
# Bolt takes the same token as HTTP. Prefer private networking, or run
# --tls-cert/--tls-key on the server, over plaintext on a public proxy.
# Railway's config as code has no TCP proxy setting that was found in its docs
# (UNVERIFIED), so this is dashboard-only.
#   MUSHROOMDB_BACKUP_S3_URL    s3://<bucket>/<prefix>   continuous backup, off unless set;
#                                                        AWS_* credentials as service variables
#   MUSHROOMDB_RESTORE_FROM_S3  s3://<bucket>/<prefix>   restore on boot: an empty volume restores
#                                                        the newest backup there before serving
# (both commented out here: set them in the dashboard; docs/site/deployment.md)
# TODO(HO-14/HO-15): the rest of the backup variables go here.
FROM ghcr.io/matthewsherlin/mushroomdb:latest
# Explicit command: no --demo-if-empty, so a production volume is never seeded.
CMD ["serve", "/data", "--addr", "0.0.0.0:8080", "--snapshot-every", "300"]
