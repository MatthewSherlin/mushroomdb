# mushroomdb on Kubernetes

`statefulset.yaml` holds a ConfigMap, a headless and a ClusterIP Service, and a one-replica StatefulSet. It does not contain the token.

1. Create the token Secret first; the pod will not start without it:
   `kubectl create secret generic mushroomdb-token --from-literal=token="$(openssl rand -hex 32)"`
2. Pin the image tag in `statefulset.yaml` (`:latest` is only the default), then `kubectl apply -f deploy/k8s/statefulset.yaml`.
3. Keep `replicas: 1`: mushroomdb is single-writer. There is no autoscaler on purpose.
4. `fsGroup: 65532` lets the non-root image write its volume; your storage class must honor it. If it does not, the pod crash-loops with a permission-denied error on `/data`.
5. Readiness is `GET /ready`, liveness `GET /health`, and a startup probe allows five minutes for a large open. Memory request 512Mi, limit 1Gi: size them to your data.
6. After changing the ConfigMap or Secret, run `kubectl rollout restart statefulset/mushroomdb`.
7. Not yet verified on a live cluster; test with your storage class. Backup settings and the Bolt port arrive with HO-14/HO-15.
