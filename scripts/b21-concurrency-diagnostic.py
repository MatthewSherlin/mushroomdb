#!/usr/bin/env python3
"""Neutral same-runner probes: completion concerns evidence, never release acceptance."""
from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import re
import shutil
import signal
import statistics
import subprocess
import sys
import tarfile
import time
import traceback
from typing import Any
import unittest
import urllib.request

ROOT = Path(__file__).resolve().parent.parent
REFS = {
    "v092": "d8637ebd1765ac7a4bb9ab16e0c24b065ae04e01",
    "parent": "1ff587aa296719a3d97cc5d979d3e37203ace711",
    "head": "b9f9f4a1c944e11854dc23a0c1c63feb8b7ec010",
}
MERGE = "db220d3661556b08f3050d13614aa880f9db5606"
HEAD_TREE = "91f6a788e81f2bf18825855013db977b00ab9881"
HARNESS = ("crates/core-bench/tests/concurrency_gate.rs",
           "crates/core-bench/benches/shared/concurrency.rs")
LABELS = tuple(REFS)
ROUNDS = 7
PROBE = re.compile(r"^WRITER_PROBE alone=([0-9.]+) under=([0-9.]+) "
                   r"reads_per_sec=([0-9]+) readers=([0-9]+)$", re.MULTILINE)


def save(path: Path, data: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data, indent=2, allow_nan=False) + "\n")


def sha256(path: Path) -> str:
    with path.open("rb") as handle:
        return hashlib.file_digest(handle, "sha256").hexdigest()


def git(*args: str) -> str:
    return subprocess.check_output(["git", *args], cwd=ROOT, text=True, timeout=60).strip()


def overrides(env: dict[str, str]) -> list[str]:
    keys = {"RUSTFLAGS", "RUSTDOCFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_ENCODED_RUSTDOCFLAGS",
            "RUSTC", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "RUSTC_BOOTSTRAP",
            "CARGO_TARGET_DIR", "CARGO_BUILD_TARGET", "CARGO_BUILD_RUSTFLAGS",
            "LD_PRELOAD", "DYLD_INSERT_LIBRARIES"}
    blocked = [k for k, v in env.items() if v and
               (k in keys or k.startswith(("CARGO_PROFILE_", "CARGO_TARGET_", "MUSHROOMDB_")))]
    for key, expected in (("CARGO_BUILD_JOBS", "3"), ("RUST_TEST_THREADS", "4"), ("CARGO_INCREMENTAL", "0")):
        if env.get(key) and env[key] != expected:
            blocked.append(key)
    return sorted(blocked)


def configs(source: Path) -> list[str]:
    paths = {p / ".cargo" / n for p in (source, *source.parents) for n in ("config", "config.toml")}
    home = Path(os.environ.get("CARGO_HOME", str(Path.home() / ".cargo")))
    paths.update(home / n for n in ("config", "config.toml"))
    return sorted(str(p) for p in paths if p.is_file())


def source_hashes(source: Path) -> dict[str, str]:
    """Rust input/manifest integrity, with full archive/tree identity recorded separately."""
    paths = {source / name for name in ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml")}
    for name in ("core-api", "core-query", "core-rules", "core-storage", "core-bench"):
        crate = source / "crates" / name
        paths.add(crate / "Cargo.toml")
        paths.update(crate.rglob("*.rs"))
    return {str(p.relative_to(source)): sha256(p) for p in sorted(paths) if p.is_file()}


def host() -> dict[str, Any]:
    files = [Path(n) for n in ("/proc/cpuinfo", "/proc/self/status", "/proc/meminfo",
                              "/sys/fs/cgroup/cpu.max", "/etc/os-release")]
    return {"platform": platform.platform(), "machine": platform.machine(), "cpu_count": os.cpu_count(),
            "affinity": sorted(os.sched_getaffinity(0)), "load": os.getloadavg(),
            "files": {str(p): p.read_text() for p in files if p.is_file()},
            "runner": {k: os.environ.get(k) for k in ("RUNNER_OS", "RUNNER_ARCH", "RUNNER_NAME",
                       "ImageOS", "ImageVersion", "GITHUB_RUN_ID", "GITHUB_RUN_ATTEMPT", "GITHUB_SHA", "GITHUB_REF")}}


def run(args: list[str], cwd: Path, env: dict[str, str], stem: Path, timeout: int) -> None:
    """Only this newly created process group belongs to this command."""
    stem.parent.mkdir(parents=True, exist_ok=True)
    receipt: dict[str, Any] = {"command": args, "cwd": str(cwd), "timeout_seconds": timeout}
    start = time.monotonic()
    with stem.with_suffix(".stdout").open("wb") as out, stem.with_suffix(".stderr").open("wb") as err:
        process = subprocess.Popen(args, cwd=cwd, env=env, stdout=out, stderr=err, start_new_session=True)
        receipt["owned_process_group"] = process.pid
        receipt["timed_out"] = False
        try:
            process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            receipt["timed_out"] = True
            try:
                os.killpg(process.pid, signal.SIGTERM)
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=5)
            except ProcessLookupError:
                process.wait(timeout=5)
        receipt.update(returncode=process.returncode, elapsed_seconds=time.monotonic() - start)
    save(stem.with_suffix(".process.json"), receipt)
    if receipt["timed_out"] or receipt["returncode"] != 0:
        raise RuntimeError(f"command failed; retained {stem}.process.json")


def parse_probe(text: str) -> dict[str, float | int]:
    found = PROBE.findall(text)
    if len(found) != 1:
        raise ValueError("expected exactly one complete WRITER_PROBE line")
    a, u, rate, readers = found[0]
    result = dict(alone_ms=float(a), under_ms=float(u), reads_per_sec=int(rate), readers=int(readers))
    if any(not math.isfinite(v) or v <= 0 for v in result.values()):
        raise ValueError("nonpositive or nonfinite probe measurement")
    return result


def order(index: int) -> tuple[str, ...]:
    offset = index % len(LABELS)
    return LABELS[offset:] + LABELS[:offset]


def summarize(samples: list[dict[str, Any]]) -> dict[str, Any]:
    measured = [s for s in samples if s["phase"] == "measured"]
    expected = {(i, label) for i in range(ROUNDS) for label in LABELS}
    actual = [(s["round"], s["label"]) for s in measured]
    if len(actual) != len(expected) or set(actual) != expected:
        raise ValueError("incomplete/duplicate cohort; no favorable-subset summary")
    if len({s["probe"]["readers"] for s in samples}) != 1:
        raise ValueError("reader counts differ across versions/processes")
    result: dict[str, Any] = {"interpretation": "diagnostic ratios only; no acceptance verdict"}
    metrics = ("alone_ms", "under_ms", "reads_per_sec")
    for label in LABELS:
        rows = [s["probe"] for s in measured if s["label"] == label]
        result[label] = {k: dict(median=statistics.median(r[k] for r in rows),
                                min=min(r[k] for r in rows), max=max(r[k] for r in rows)) for k in metrics}
    result["comparisons"] = {}
    for numerator, denominator in (("head", "parent"), ("parent", "v092"), ("head", "v092")):
        pairs = []
        for index in range(ROUNDS):
            rows = {s["label"]: s["probe"] for s in measured if s["round"] == index}
            pairs.append({k: rows[numerator][k] / rows[denominator][k] for k in metrics})
        result["comparisons"][f"{numerator}/{denominator}"] = {
            "paired_ratios": pairs, "paired_medians": {k: statistics.median(p[k] for p in pairs) for k in metrics},
            "ratios_of_medians": {k: result[numerator][k]["median"] / result[denominator][k]["median"] for k in metrics}}
    return result


def artifact(record: dict[str, Any]) -> str:
    p = record["profile"]
    if (record.get("reason") != "compiler-artifact" or record["target"]["name"] != "concurrency_gate"
            or p.get("opt_level") != "3" or p.get("debug_assertions") is not False
            or p.get("overflow_checks") is not False or not record.get("executable")):
        raise ValueError("unexpected executable/release profile")
    return str(record["executable"])


def verify_merge(output: Path) -> None:
    repo, token = os.environ.get("GITHUB_REPOSITORY"), os.environ.get("GH_DIAGNOSTIC_TOKEN")
    if repo != "MatthewSherlin/mushroomdb" or not token:
        raise ValueError("expected repository and ephemeral workflow token")
    request = urllib.request.Request(f"https://api.github.com/repos/{repo}/git/commits/{MERGE}",
              headers={"Accept": "application/vnd.github+json", "Authorization": f"Bearer {token}"})
    with urllib.request.urlopen(request, timeout=30) as response:
        receipt = json.load(response)
    proof = dict(synthetic_commit=receipt["sha"], synthetic_tree=receipt["tree"]["sha"],
                 parents=[p["sha"] for p in receipt["parents"]], head_tree=git("rev-parse", f"{REFS['head']}^{{tree}}"))
    save(output / "synthetic-tree.json", proof)
    if (proof["synthetic_commit"] != MERGE or proof["synthetic_tree"] != HEAD_TREE
            or proof["head_tree"] != HEAD_TREE or proof["parents"] != [REFS["parent"], REFS["head"]]):
        raise ValueError("synthetic checkout differs from immutable head/base")


def build(label: str, work: Path, output: Path, env: dict[str, str]) -> dict[str, Any]:
    source, archive = work / label, work / f"{label}.tar"
    source.mkdir()
    git("archive", "--format=tar", f"--output={archive}", REFS[label])
    with tarfile.open(archive) as contents:
        contents.extractall(source, filter="data")
    if configs(source):
        raise ValueError(f"unexpected Cargo configuration: {configs(source)}")
    before = {n: sha256(source / n) for n in ("Cargo.toml", "Cargo.lock")}
    for name in HARNESS:
        expected = subprocess.check_output(["git", "show", f"{REFS['head']}:{name}"], cwd=ROOT)
        p = source / name
        if label == "v092":
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_bytes(expected)
        if p.read_bytes() != expected:
            raise ValueError(f"{label}: harness differs from immutable head: {name}")
    receipt: dict[str, Any] = dict(label=label, commit=REFS[label], tree=git("rev-parse", f"{REFS[label]}^{{tree}}"),
        archive_sha256=sha256(archive), manifests_before=before, source=str(source),
        harness_sha256={n: sha256(source / n) for n in HARNESS},
        source_hashes_before=source_hashes(source), rustflags=[])
    build_env = dict(env, CARGO_TARGET_DIR=str(source / "target"), CARGO_BUILD_JOBS="3")
    if label == "v092":
        build_env["RUSTFLAGS"] = "--cfg mdb_v092"
        receipt["rustflags"] = ["--cfg", "mdb_v092"]
    save(output / "builds" / f"{label}.source.json", receipt)
    stem = output / "builds" / label
    run(["cargo", "test", "--release", "-p", "core-bench", "-j", "3", "--test", "concurrency_gate",
         "--no-run", "--message-format=json"], source, build_env, stem, 900)
    records = [json.loads(line) for line in stem.with_suffix(".stdout").read_text().splitlines()]
    matches = [r for r in records if r.get("reason") == "compiler-artifact"
               and r.get("target", {}).get("name") == "concurrency_gate" and r.get("executable")]
    if len(matches) != 1:
        raise ValueError(f"{label}: expected one executable artifact")
    binary = Path(artifact(matches[0]))
    after = {n: sha256(source / n) for n in before}
    if before != after or not binary.is_file() or not os.access(binary, os.X_OK):
        raise ValueError(f"{label}: changed manifests/lock or missing executable")
    receipt["source_hashes_after_build"] = source_hashes(source)
    receipt["harness_sha256_after_build"] = {n: sha256(source / n) for n in HARNESS}
    if receipt["source_hashes_after_build"] != receipt["source_hashes_before"]:
        save(output / "builds" / f"{label}.source.json", receipt)
        raise ValueError(f"{label}: Rust source/harness changed during build")
    fps = list((source / "target/release/.fingerprint").glob("core-bench-*/test-integration-test-concurrency_gate.json"))
    fingerprint = [dict(path=str(p), data=json.loads(p.read_text())) for p in fps]
    if len(fps) != 1 or fingerprint[0]["data"].get("rustflags") != receipt["rustflags"]:
        raise ValueError(f"{label}: unexpected fingerprint/rustflags")
    receipt.update(manifests_after=after, compiler_artifact=matches[0], fingerprints=fingerprint,
                   binary=str(binary), binary_sha256=sha256(binary))
    preserved = output / "binaries" / f"{label}-concurrency_gate"
    preserved.parent.mkdir(exist_ok=True)
    shutil.copy2(binary, preserved)
    receipt["preserved_binary"] = str(preserved)
    save(output / "builds" / f"{label}.source.json", receipt)
    return receipt


def diagnose(output: Path) -> None:
    env = dict(os.environ)
    env.pop("GH_DIAGNOSTIC_TOKEN", None)  # Subprocesses never need the read-only API token.
    blocked = overrides(env)
    save(output / "environment.json", dict(rejected_override_keys=blocked,
         RUSTUP_TOOLCHAIN=env.get("RUSTUP_TOOLCHAIN"), CARGO_HOME=env.get("CARGO_HOME"),
         CARGO_INCREMENTAL=env.get("CARGO_INCREMENTAL"), CARGO_BUILD_JOBS=env.get("CARGO_BUILD_JOBS"),
         RUST_TEST_THREADS=env.get("RUST_TEST_THREADS")))
    if blocked:
        raise ValueError(f"unexpected environment overrides: {', '.join(blocked)}")
    if platform.system() != "Linux" or platform.machine() != "x86_64":
        raise ValueError("requires x86_64 Linux, with no emulated-host substitution")
    save(output / "host-before.json", host())
    rustc = subprocess.check_output(["rustc", "--version", "--verbose"], text=True, timeout=30)
    save(output / "toolchain.json", dict(rustc=rustc, cargo=subprocess.check_output(
         ["cargo", "--version"], text=True, timeout=30), rustc_path=shutil.which("rustc"), cargo_path=shutil.which("cargo")))
    if "release: 1.92.0\n" not in rustc or "host: x86_64-unknown-linux-gnu" not in rustc:
        raise ValueError("expected CI rustc1.92.0/x86_64-unknown-linux-gnu")
    if not git("symbolic-ref", "--short", "HEAD").startswith("diagnostic/"):
        raise ValueError("requires isolated diagnostic branch")
    save(output / "protocol.json", dict(diagnostic_commit=git("rev-parse", "HEAD"), refs=REFS,
         warmups_per_version=1, measured_rounds=ROUNDS, orders=[order(i) for i in range(ROUNDS)],
         script_sha256=sha256(Path(__file__)), workflow_sha256=sha256(ROOT / ".github/workflows/ci.yml"),
         meaning="neutral probes only; complete evidence never satisfies release acceptance"))
    verify_merge(output)
    work = output.with_name(output.name + "-work")
    work.mkdir(exist_ok=False)
    builds = {label: build(label, work, output, env) for label in LABELS}  # Serial builds, no overlap.
    samples: list[dict[str, Any]] = []
    schedule = [("warmup", -1, LABELS), *[("measured", i, order(i)) for i in range(ROUNDS)]]
    for phase, index, labels in schedule:
        for position, label in enumerate(labels):
            stem = output / "probes" / f"{phase}-{index + 1:02d}-{position + 1}-{label}"
            sample: dict[str, Any] = dict(phase=phase, round=index, position=position, label=label,
                                         load_before=os.getloadavg(), utc_start=time.time())
            run([builds[label]["binary"], "writer_probe", "--ignored", "--nocapture", "--exact", "--test-threads=4"],
                Path(builds[label]["source"]), env, stem, 120)
            sample.update(probe=parse_probe(stem.with_suffix(".stdout").read_text()), load_after=os.getloadavg())
            samples.append(sample)
            save(output / "samples.json", samples)
    save(output / "summary.json", summarize(samples))
    for label in LABELS:
        receipt = builds[label]
        source = Path(receipt["source"])
        receipt["source_hashes_after_probes"] = source_hashes(source)
        receipt["harness_sha256_after_probes"] = {n: sha256(source / n) for n in HARNESS}
        receipt["binary_sha256_after_probes"] = sha256(Path(receipt["binary"]))
        save(output / "builds" / f"{label}.source.json", receipt)
        if receipt["source_hashes_after_probes"] != receipt["source_hashes_before"]:
            raise ValueError(f"{label}: Rust source/harness changed during measurement")
        if receipt["binary_sha256_after_probes"] != receipt["binary_sha256"]:
            raise ValueError(f"{label}: binary changed during measurement")
    save(output / "host-after.json", host())


def self_test() -> bool:
    class Bookkeeping(unittest.TestCase):
        def test_parse_and_reject_missing_duplicate_or_zero(self) -> None:
            good = "WRITER_PROBE alone=0.01 under=0.02 reads_per_sec=123 readers=2\n"
            self.assertEqual(parse_probe(good)["under_ms"], .02)
            for text in ("", good + good, good.replace("0.01", "0.00"), good.replace("readers=2", "readers=0")):
                with self.subTest(text=text), self.assertRaises(ValueError):
                    parse_probe(text)

        def test_overrides(self) -> None:
            self.assertEqual(overrides(dict(CARGO_BUILD_JOBS="3", RUST_TEST_THREADS="4", CARGO_INCREMENTAL="0")), [])
            self.assertEqual(overrides(dict(CARGO_INCREMENTAL="1")), ["CARGO_INCREMENTAL"])
            for k in ("RUSTFLAGS", "CARGO_PROFILE_RELEASE_CODEGEN_UNITS", "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS", "MUSHROOMDB_VECTOR_SCAN"):
                self.assertEqual(overrides({k: "1"}), [k])

        def test_rotation(self) -> None:
            self.assertEqual([order(i) for i in range(3)], [("v092", "parent", "head"), ("parent", "head", "v092"), ("head", "v092", "parent")])

        def samples(self) -> list[dict[str, Any]]:
            return [dict(phase="measured", round=r, label=label, probe=dict(alone_ms=i + 1,
                         under_ms=2 * (i + 1), reads_per_sec=100 * (i + 1), readers=2))
                    for r in range(ROUNDS) for i, label in enumerate(LABELS)]

        def test_summary_all_rounds(self) -> None:
            result = summarize(self.samples())["comparisons"]["head/parent"]
            self.assertEqual(result["paired_medians"]["under_ms"], 1.5)
            self.assertEqual(len(result["paired_ratios"]), ROUNDS)

        def test_reject_subset_duplicate_or_different_readers(self) -> None:
            rows = self.samples()
            for bad in (rows[:-1], rows[:-1] + rows[:1]):
                with self.assertRaises(ValueError):
                    summarize(bad)
            rows[0]["probe"]["readers"] = 3
            with self.assertRaises(ValueError):
                summarize(rows)

        def test_release_artifact(self) -> None:
            value = dict(reason="compiler-artifact", target=dict(name="concurrency_gate"),
                         profile=dict(opt_level="3", debug_assertions=False, overflow_checks=False), executable="/owned/probe")
            self.assertEqual(artifact(value), "/owned/probe")
            value["profile"]["opt_level"] = "0"
            with self.assertRaises(ValueError):
                artifact(value)

    return unittest.TextTestRunner().run(unittest.defaultTestLoader.loadTestsFromTestCase(Bookkeeping)).wasSuccessful()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        return 0 if self_test() else 1
    if args.output is None:
        parser.error("--output is required unless --self-test is selected")
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    status = dict(evidence_complete=False, release_acceptance="not evaluated")
    save(output / "status.json", status)
    code = 0
    try:
        diagnose(output)
        status["evidence_complete"] = True
    except Exception as error:
        code = 1
        status["error"] = f"{type(error).__name__}: {error}"
        (output / "error.txt").write_text(traceback.format_exc())
        print(status["error"], file=sys.stderr)
    finally:
        save(output / "status.json", status)
        save(output / "evidence-files.json", {str(p.relative_to(output)): dict(sha256=sha256(p), bytes=p.stat().st_size)
             for p in sorted(output.rglob("*")) if p.is_file() and p.name != "evidence-files.json"})
    print(f"Diagnostic evidence: {output}; release acceptance remains unevaluated")
    return code


if __name__ == "__main__":
    sys.exit(main())
