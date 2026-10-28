//! The deploy templates under `deploy/` exist, warn about the single-writer
//! constraint, use `/ready` as the health check, and carry no secret values.
use std::path::PathBuf;

fn template(relative: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../deploy")
        .join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

const TEMPLATES: [&str; 5] = [
    "fly/fly.toml",
    "render/render.yaml",
    "docker-compose/docker-compose.yml",
    "railway/railway.json",
    "railway/Dockerfile",
];

#[test]
fn deploy_templates_single_writer_documented() {
    // railway.json is strict JSON with no comments; its note lives in the Dockerfile.
    for name in TEMPLATES.iter().filter(|n| **n != "railway/railway.json") {
        assert!(
            template(name).contains("SINGLE WRITER - replicas must be 1"),
            "{name} must warn about the single-writer constraint"
        );
    }
}

#[test]
fn deploy_templates_use_ready_where_a_platform_checks_health() {
    for (name, needle) in [
        ("fly/fly.toml", "path = \"/ready\""),
        ("render/render.yaml", "healthCheckPath: /ready"),
        ("railway/railway.json", "\"healthcheckPath\": \"/ready\""),
    ] {
        assert!(template(name).contains(needle), "{name} must check /ready");
    }
}

#[test]
fn deploy_templates_hold_no_secret_values() {
    for name in TEMPLATES {
        let text = template(name);
        for line in text.lines().filter(|l| l.contains("MUSHROOMDB_TOKEN")) {
            assert!(
                !line.contains("changeme") && !line.contains("secret123"),
                "{name}: token line carries a value: {line}"
            );
        }
    }
}

/// The backup and restore-on-boot variables are shown, commented out, each
/// with what it does; nothing turns them on, and the rest of the backup
/// settings are still to come (HO-14/HO-15).
#[test]
fn deploy_templates_show_backup_and_restore_on_boot_commented_out() {
    let commented = TEMPLATES
        .iter()
        .filter(|n| **n != "railway/railway.json")
        .chain(&["k8s/statefulset.yaml"]);
    for name in commented {
        let text = template(name);
        for variable in ["MUSHROOMDB_BACKUP_S3_URL", "MUSHROOMDB_RESTORE_FROM_S3"] {
            let lines: Vec<&str> = text.lines().filter(|l| l.contains(variable)).collect();
            assert!(!lines.is_empty(), "{name} does not show {variable}");
            for line in lines {
                assert!(
                    line.trim_start().starts_with('#'),
                    "{name}: {variable} must stay commented out: {line}"
                );
            }
        }
        assert!(
            text.contains("restore on boot: an empty")
                || text.contains("Restore on boot: an empty"),
            "{name} does not say what restore on boot does"
        );
        assert!(
            text.contains("Backup stays off until MUSHROOMDB_BACKUP_S3_URL is set"),
            "{name} does not say how backup is turned on"
        );
        assert!(
            !text.contains("TODO(HO-14/HO-15)"),
            "{name} keeps a stale TODO"
        );
    }
    assert!(!template("railway/railway.json").contains("MUSHROOMDB_"));
}

#[test]
fn deploy_templates_name_the_real_image_and_leave_no_placeholder_owner() {
    for name in TEMPLATES {
        assert!(!template(name).contains("<owner>"), "{name} has <owner>");
    }
    for name in [
        "fly/fly.toml",
        "render/render.yaml",
        "docker-compose/docker-compose.yml",
        "railway/Dockerfile",
    ] {
        assert!(
            template(name).contains("ghcr.io/matthewsherlin/mushroomdb:latest"),
            "{name} must name the published image"
        );
    }
}

#[test]
fn deploy_templates_run_an_explicit_serve_command_without_demo_seeding() {
    for (name, needle) in [
        ("fly/fly.toml", "app = \"serve /data --addr 0.0.0.0:8080"),
        (
            "render/render.yaml",
            "dockerCommand: serve /data --addr 0.0.0.0:8080",
        ),
        (
            "docker-compose/docker-compose.yml",
            "command: [\"serve\", \"/data\"",
        ),
        ("railway/Dockerfile", "CMD [\"serve\", \"/data\""),
    ] {
        let text = template(name);
        let command = text
            .lines()
            .find(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("{name} lacks an explicit serve command"));
        assert!(!command.contains("--demo-if-empty"), "{name}: {command}");
        assert!(
            !command.trim_start().starts_with('#'),
            "{name}: command is commented out"
        );
    }
}

#[test]
fn deploy_templates_set_port_8080_on_platforms_that_route_by_port() {
    assert!(template("render/render.yaml").contains("key: PORT\n        value: \"8080\""));
    assert!(template("railway/Dockerfile").contains("#   PORT               8080"));
    assert!(template("fly/fly.toml").contains("internal_port = 8080"));
}

/// `serve` drains, snapshots, then flushes the backup and releases its lease
/// (about 22 s of backup alone at the defaults); a platform that kills it
/// sooner leaves the lease behind.
#[test]
fn deploy_templates_allow_the_shutdown_sequence_to_finish() {
    for (name, needle) in [
        ("fly/fly.toml", "\nkill_timeout = 60\n"),
        (
            "docker-compose/docker-compose.yml",
            "stop_grace_period: 60s",
        ),
    ] {
        assert!(
            template(name).contains(needle),
            "{name} must contain {needle}"
        );
    }
}

/// flyctl lowers a check's `grace_period` over 60 s to 60 s, so the template
/// states the value Fly really uses.
#[test]
fn the_fly_template_keeps_its_check_grace_period_within_flys_limit() {
    let text = template("fly/fly.toml");
    let periods: Vec<&str> = text
        .lines()
        .filter_map(|l| l.trim().strip_prefix("grace_period = "))
        .collect();
    assert_eq!(periods, ["\"60s\""], "fly.toml grace_period lines");
}

/// A Render service with a persistent disk gets a fixed 30 s shutdown delay;
/// `maxShutdownDelaySeconds` is refused with a disk, so it must not be set,
/// and `serve` is told to finish inside the window instead.
#[test]
fn the_render_template_fits_its_fixed_shutdown_window() {
    let text = template("render/render.yaml");
    assert!(text.contains("disk:"), "the template has a persistent disk");
    assert!(
        !text
            .lines()
            .any(|l| l.trim_start().starts_with("maxShutdownDelaySeconds")),
        "maxShutdownDelaySeconds is not allowed with a disk"
    );
    assert!(
        text.lines()
            .any(|l| l.trim_start().starts_with("dockerCommand:")
                && l.contains("--shutdown-timeout-secs 25")),
        "serve must be told to finish inside Render's 30 s"
    );
}

/// Railway drains the old deployment for `drainingSeconds` before SIGKILL;
/// it must cover `serve`'s default 55 s shutdown budget.
#[test]
fn the_railway_template_drains_long_enough_for_the_shutdown_sequence() {
    let text = template("railway/railway.json");
    let seconds: u64 = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("\"drainingSeconds\":"))
        .and_then(|v| v.trim().trim_end_matches(',').parse().ok())
        .expect("railway.json sets deploy.drainingSeconds");
    assert!(seconds >= 55, "drainingSeconds {seconds} < 55");
}

/// The k8s manifest as JSON documents. Parsing is done by python3's PyYAML (no
/// YAML crate may be added); the test is skipped with a message where that is
/// unavailable, and fails when `CI` is set.
fn k8s_documents() -> Option<Vec<serde_json::Value>> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../deploy/k8s/statefulset.yaml");
    let script =
        "import sys, json, yaml; print(json.dumps(list(yaml.safe_load_all(open(sys.argv[1])))))";
    let out = std::process::Command::new("python3")
        .args(["-c", script])
        .arg(&path)
        .output();
    match out {
        Ok(o) if o.status.success() => Some(serde_json::from_slice(&o.stdout).expect("json")),
        other => {
            // A skip in CI would leave the manifest unchecked without anyone seeing it.
            assert!(
                std::env::var_os("CI").is_none(),
                "python3 with PyYAML is required in CI: {other:?}"
            );
            eprintln!("skipping: python3 with PyYAML unavailable: {other:?}");
            None
        }
    }
}

fn k8s_statefulset(documents: &[serde_json::Value]) -> &serde_json::Value {
    documents
        .iter()
        .find(|d| d["kind"] == "StatefulSet")
        .expect("a StatefulSet")
}

#[test]
fn k8s_statefulset_is_single_writer_with_ready_and_health_probes() {
    let text = template("k8s/statefulset.yaml");
    assert!(text.contains("SINGLE WRITER - replicas must be 1"));
    let Some(documents) = k8s_documents() else {
        return;
    };
    assert!(
        documents
            .iter()
            .all(|d| d["kind"] != "HorizontalPodAutoscaler"),
        "no HPA resource"
    );
    let set = k8s_statefulset(&documents);
    assert_eq!(set["spec"]["replicas"], 1);
    assert_eq!(
        set["spec"]["volumeClaimTemplates"][0]["spec"]["accessModes"][0],
        "ReadWriteOnce"
    );
    let pod = &set["spec"]["template"]["spec"];
    assert_eq!(pod["securityContext"]["fsGroup"], 65532);
    assert_eq!(
        pod["securityContext"]["fsGroupChangePolicy"],
        "OnRootMismatch"
    );
    assert_eq!(pod["securityContext"]["runAsNonRoot"], true);
    assert_eq!(pod["terminationGracePeriodSeconds"], 120);
    let container = &pod["containers"][0];
    assert_eq!(
        container["image"],
        "ghcr.io/matthewsherlin/mushroomdb:latest"
    );
    assert_eq!(container["readinessProbe"]["httpGet"]["path"], "/ready");
    assert_eq!(container["readinessProbe"]["initialDelaySeconds"], 60);
    assert_eq!(container["livenessProbe"]["httpGet"]["path"], "/health");
    assert_eq!(container["livenessProbe"]["initialDelaySeconds"], 30);
    assert_eq!(container["startupProbe"]["httpGet"]["path"], "/health");
}

#[test]
fn k8s_statefulset_runs_an_explicit_serve_command() {
    let Some(documents) = k8s_documents() else {
        return;
    };
    let container = &k8s_statefulset(&documents)["spec"]["template"]["spec"]["containers"][0];
    let args: Vec<&str> = container["args"]
        .as_array()
        .expect("args")
        .iter()
        .map(|a| a.as_str().unwrap())
        .collect();
    assert_eq!(&args[..2], ["serve", "/data"]);
    assert!(!args.contains(&"--demo-if-empty"));
}

#[test]
fn k8s_token_comes_from_an_external_required_secret_and_no_secret_object_ships() {
    let text = template("k8s/statefulset.yaml");
    assert!(!text.contains("<set-me>"), "no placeholder token value");
    assert!(!text.contains("stringData"));
    assert!(text.contains("Backup stays off until MUSHROOMDB_BACKUP_S3_URL is set"));
    let Some(documents) = k8s_documents() else {
        return;
    };
    assert!(documents.iter().all(|d| d["kind"] != "Secret"));
    let container = &k8s_statefulset(&documents)["spec"]["template"]["spec"]["containers"][0];
    let token = container["env"]
        .as_array()
        .expect("env")
        .iter()
        .find(|e| e["name"] == "MUSHROOMDB_TOKEN")
        .expect("MUSHROOMDB_TOKEN env");
    assert!(token.get("value").is_none(), "token must not be inline");
    let reference = &token["valueFrom"]["secretKeyRef"];
    assert_eq!(reference["name"], "mushroomdb-token");
    assert_eq!(reference["key"], "token");
    assert!(
        reference.get("optional").is_none() || reference["optional"] == false,
        "the Secret must be required so the pod fails closed"
    );
}

#[test]
fn k8s_container_is_hardened_and_has_resources() {
    let Some(documents) = k8s_documents() else {
        return;
    };
    let pod = &k8s_statefulset(&documents)["spec"]["template"]["spec"];
    let container = &pod["containers"][0];
    let security = &container["securityContext"];
    assert_eq!(security["allowPrivilegeEscalation"], false);
    assert_eq!(security["readOnlyRootFilesystem"], true);
    assert_eq!(security["capabilities"]["drop"][0], "ALL");
    assert_eq!(security["seccompProfile"]["type"], "RuntimeDefault");
    assert!(container["resources"]["requests"]["memory"].is_string());
    assert!(container["resources"]["requests"]["cpu"].is_string());
    assert!(container["resources"]["limits"]["memory"].is_string());
    let mounts: Vec<&str> = container["volumeMounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["mountPath"].as_str().unwrap())
        .collect();
    assert!(mounts.contains(&"/data") && mounts.contains(&"/tmp"));
}

#[test]
fn k8s_governing_service_is_headless() {
    let Some(documents) = k8s_documents() else {
        return;
    };
    let governing = k8s_statefulset(&documents)["spec"]["serviceName"].clone();
    let service = documents
        .iter()
        .find(|d| d["kind"] == "Service" && d["metadata"]["name"] == governing)
        .expect("governing Service");
    assert_eq!(service["spec"]["clusterIP"], "None");
}

/// Lines of `text` that are not comments (`#`).
fn live_lines(text: &str) -> Vec<&str> {
    text.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect()
}

/// Bolt is exposed only where the platform can carry it safely (HO-15).
#[test]
fn fly_exposes_bolt_only_behind_the_tls_handler() {
    let text = template("fly/fly.toml");
    let live = live_lines(&text).join("\n");
    assert!(live.contains("internal_port = 7687"));
    assert!(live.contains("MUSHROOMDB_BOLT_PORT = \"7687\""));
    assert!(live.contains("MUSHROOMDB_BOLT_ADVERTISED_HOST"));
    // A placeholder that resolves could belong to anyone: it must fail closed.
    assert!(live.contains("MUSHROOMDB_BOLT_ADVERTISED_HOST = \"CHANGE-ME.invalid\""));
    assert!(live.contains("MUSHROOMDB_BOLT_ADVERTISED_PORT = \"7687\""));
    // The public 7687 port carries exactly the `tls` handler, never raw TCP.
    let block = live
        .split("internal_port = 7687")
        .nth(1)
        .expect("bolt service block");
    let block = block.split("[[services]]").next().unwrap();
    assert!(
        block.contains("port = 7687\n    handlers = [\"tls\"]"),
        "{block}"
    );
    assert!(!block.contains("http"), "Bolt is not HTTP: {block}");
    assert_eq!(block.matches("handlers").count(), 1);
}

#[test]
fn compose_binds_bolt_to_loopback_only() {
    let text = template("docker-compose/docker-compose.yml");
    let live = live_lines(&text);
    assert!(live.iter().any(|l| l.trim() == "- \"127.0.0.1:7687:7687\""));
    assert!(
        !live.iter().any(|l| l.trim_start().starts_with("- \"")
            && l.contains("7687")
            && !l.contains("127.0.0.1")),
        "a Bolt port mapping that is not loopback"
    );
    assert!(live
        .iter()
        .any(|l| l.contains("MUSHROOMDB_BOLT_PORT: \"7687\"")));
}

#[test]
fn render_and_railway_do_not_open_bolt() {
    for name in [
        "render/render.yaml",
        "railway/Dockerfile",
        "railway/railway.json",
    ] {
        let text = template(name);
        assert!(
            !live_lines(&text)
                .iter()
                .any(|l| l.contains("7687") || l.contains("BOLT")),
            "{name} must not enable Bolt"
        );
    }
    assert!(template("render/render.yaml").contains("NO BOLT"));
    assert!(template("railway/Dockerfile").contains("Bolt is OFF"));
}

#[test]
fn k8s_exposes_bolt_in_cluster_only() {
    let text = template("k8s/statefulset.yaml");
    assert!(text.contains("MUSHROOMDB_BOLT_PORT: \"7687\""));
    let Some(documents) = k8s_documents() else {
        return;
    };
    let container = &k8s_statefulset(&documents)["spec"]["template"]["spec"]["containers"][0];
    assert!(container["ports"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["name"] == "bolt" && p["containerPort"] == 7687));
    let service = documents
        .iter()
        .find(|d| d["kind"] == "Service" && d["metadata"]["name"] == "mushroomdb")
        .expect("client Service");
    assert_eq!(service["spec"]["type"], "ClusterIP");
    assert!(service["spec"]["ports"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["name"] == "bolt" && p["port"] == 7687));
    assert!(documents
        .iter()
        .all(|d| d["kind"] != "Service" || d["spec"]["type"] != "LoadBalancer"));
}

fn repo_file(relative: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The self-host guide exists, links the reference pages, and names every
/// variable and flag it teaches as the code reads it: each name is checked in
/// the source that reads it first, so a rename there breaks this test instead
/// of leaving the guide wrong.
#[test]
fn selfhost_guide_exists() {
    let guide = repo_file("docs/site/self-host.md");
    let backup_source = repo_file("crates/backup/src/store.rs");
    let cli_source = repo_file("crates/cli/src/lib.rs");
    let summary = repo_file("docs/site/SUMMARY.md");
    assert!(
        summary.contains("(self-host.md)"),
        "SUMMARY.md must link the guide"
    );
    assert!(
        guide.contains("single-writer"),
        "the guide states the single-writer rule"
    );
    assert!(
        guide.contains("/ready"),
        "the guide covers the /ready probe"
    );
    for page in ["service.md", "deployment.md", "http.md", "limits.md"] {
        assert!(
            guide.contains(&format!("({page}")),
            "the guide links {page}"
        );
    }

    for variable in [
        "MUSHROOMDB_BACKUP_S3_URL",
        "MUSHROOMDB_BACKUP_WAL_INTERVAL_MS",
        "MUSHROOMDB_BACKUP_MANIFEST_INTERVAL_S",
        "MUSHROOMDB_BACKUP_RETAIN_N",
        "MUSHROOMDB_BACKUP_SSE",
        "MUSHROOMDB_BACKUP_LEASE_TTL",
        "MUSHROOMDB_BACKUP_LEASE_CONDITIONAL",
        "MUSHROOMDB_BACKUP_OP_TIMEOUT_MS",
        "MUSHROOMDB_BACKUP_SHUTDOWN_GRACE_MS",
    ] {
        assert!(
            backup_source.contains(variable),
            "{variable} is not read by the backup crate"
        );
        assert!(guide.contains(variable), "the guide must name {variable}");
    }
    for variable in [
        "MUSHROOMDB_RESTORE_FROM_S3",
        "MUSHROOMDB_CORS_ORIGINS",
        "MUSHROOMDB_LOG_FORMAT",
        "MUSHROOMDB_BOLT_PORT",
        "MUSHROOMDB_TOKEN",
    ] {
        let in_code = ["crates/cli/src", "crates/server/src"].iter().any(|dir| {
            std::fs::read_dir(
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../..")
                    .join(dir),
            )
            .unwrap()
            .flatten()
            .filter_map(|e| std::fs::read_to_string(e.path()).ok())
            .any(|text| text.contains(&format!("\"{variable}\"")))
        });
        assert!(
            in_code,
            "{variable} is not read by the cli or server source"
        );
        assert!(guide.contains(variable), "the guide must name {variable}");
    }
    for flag in [
        "--snapshot-every",
        "--shutdown-timeout-secs",
        "--restore-from",
        "--tls-cert",
        "--tls-key",
        "--bolt-port",
        "--tx-max",
        "--bolt-max-connections",
    ] {
        assert!(cli_source.contains(flag), "{flag} is not a serve flag");
        assert!(guide.contains(flag), "the guide must name {flag}");
    }
}
