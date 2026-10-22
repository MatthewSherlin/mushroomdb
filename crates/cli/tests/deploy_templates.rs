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
fn deploy_templates_hold_no_bolt_port_and_no_secret_values() {
    for name in TEMPLATES {
        let text = template(name);
        assert!(!text.contains("7687:7687"), "{name} must not publish Bolt");
        assert!(
            !text.contains("internal_port = 7687"),
            "{name} must not expose Bolt"
        );
        for line in text.lines().filter(|l| l.contains("MUSHROOMDB_TOKEN")) {
            assert!(
                !line.contains("changeme") && !line.contains("secret123"),
                "{name}: token line carries a value: {line}"
            );
        }
    }
}

#[test]
fn deploy_templates_do_not_reference_the_unmerged_backup_worker_variables() {
    for name in TEMPLATES.iter().filter(|n| **n != "railway/railway.json") {
        let text = template(name);
        for variable in ["MUSHROOMDB_BACKUP_S3_URL", "MUSHROOMDB_RESTORE_FROM_S3"] {
            assert!(!text.contains(variable), "{name} references {variable}");
        }
        assert!(
            text.contains("TODO(HO-14/HO-15)"),
            "{name} lacks the placeholder"
        );
    }
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
        ("fly/fly.toml", "kill_timeout = \"60s\""),
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
