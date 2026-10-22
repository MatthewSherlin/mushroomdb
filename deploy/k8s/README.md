# mushroomdb on Kubernetes

`statefulset.yaml` holds a ConfigMap, a Secret placeholder, a ClusterIP Service and a one-replica StatefulSet.

1. Set `MUSHROOMDB_TOKEN` in the Secret (or create the Secret yourself and delete that document).
2. `kubectl apply -f deploy/k8s/statefulset.yaml`
3. Keep `replicas: 1`: mushroomdb is single-writer. There is no autoscaler on purpose.
4. `fsGroup: 65532` lets the non-root image write its volume; your storage class must honor `fsGroup`.
5. Readiness is `GET /ready`, liveness is `GET /health`. Backup settings and the Bolt port arrive with HO-14/HO-15.
