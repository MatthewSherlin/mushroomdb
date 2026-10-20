//! The deploy templates under `deploy/` exist, warn about the single-writer
//! constraint, use `/ready` as the health check, and carry no secret values.
use std::path::PathBuf;

fn template(relative: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../deploy")
        .join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

const TEMPLATES: [&str; 4] = [
    "fly/fly.toml",
    "render/render.yaml",
    "docker-compose/docker-compose.yml",
    "railway/railway.json",
];

#[test]
fn deploy_templates_single_writer_documented() {
    for name in TEMPLATES {
        assert!(
            template(name).contains("SINGLE WRITER - replicas must be 1"),
            "{name} must warn about the single-writer constraint"
        );
    }
}

#[test]
fn deploy_templates_use_ready_where_a_platform_checks_health() {
    for name in ["fly/fly.toml", "render/render.yaml", "railway/railway.json"] {
        assert!(
            template(name).contains("/ready"),
            "{name} must check /ready"
        );
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
    for name in TEMPLATES {
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
