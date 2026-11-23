use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "osb-test-{}-{}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join("vault/research")).unwrap();
        Self(root)
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_osb"))
            .args(args)
            .current_dir(&self.0)
            .output()
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn json(output: &Output) -> serde_json::Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn refuses_to_create_an_index_inside_the_vault() {
    let f = Fixture::new();
    let result = f.run(&["index", "vault", "vault/generated/index"]);
    assert!(!result.status.success());
    assert!(!f.0.join("vault/generated").exists());
}

#[test]
fn ignores_hidden_paths_and_symlinks() {
    let f = Fixture::new();
    fs::create_dir(f.0.join("vault/.obsidian")).unwrap();
    fs::write(f.0.join("vault/.obsidian/private.md"), "secretcanary").unwrap();
    fs::write(f.0.join("vault/.secret.md"), "secretcanary").unwrap();
    fs::write(f.0.join("outside.md"), "secretcanary").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(f.0.join("outside.md"), f.0.join("vault/link.md")).unwrap();
    let report = json(&f.run(&["index", "vault", "index"]));
    assert_eq!(report["files"], 0);
    let result = json(&f.run(&["search", "index", "secretcanary"]));
    assert!(result["hits"].as_array().unwrap().is_empty());
}

#[test]
fn rebuilding_removes_deleted_and_outdated_passages() {
    let f = Fixture::new();
    fs::write(f.0.join("vault/updated.md"), "obsoletecanary").unwrap();
    fs::write(f.0.join("vault/deleted.md"), "obsoletecanary").unwrap();
    json(&f.run(&["index", "vault", "index"]));
    fs::write(f.0.join("vault/updated.md"), "replacementcanary").unwrap();
    fs::remove_file(f.0.join("vault/deleted.md")).unwrap();
    json(&f.run(&["index", "vault", "index"]));
    assert!(
        json(&f.run(&["search", "index", "obsoletecanary"]))["hits"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        json(&f.run(&["search", "index", "replacementcanary"]))["hits"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn invalid_queries_and_limits_fail_clearly() {
    let f = Fixture::new();
    json(&f.run(&["index", "vault", "index"]));
    for args in [
        vec!["search", "index", ""],
        vec!["search", "index", "\""],
        vec!["search", "index", "term", "--limit", "0"],
    ] {
        let result = f.run(&args);
        assert!(!result.status.success());
        assert!(!result.stderr.is_empty());
    }
}

#[test]
fn searches_unicode_text() {
    let f = Fixture::new();
    fs::write(
        f.0.join("vault/한국어.md"),
        "# 연구\n에이전트 설명가능성 연구\n",
    )
    .unwrap();
    json(&f.run(&["index", "vault", "index"]));
    assert_eq!(
        json(&f.run(&["search", "index", "설명가능성"]))["hits"][0]["path"],
        "한국어.md"
    );
}

#[test]
fn code_fences_do_not_create_false_sections() {
    let f = Fixture::new();
    fs::write(
        f.0.join("vault/code.md"),
        "# Configuration\n```sh\n# fake heading\necho pseudocode\n```\n",
    )
    .unwrap();
    json(&f.run(&["index", "vault", "index"]));
    let result = json(&f.run(&["search", "index", "pseudocode"]));
    assert_eq!(result["hits"][0]["heading"], "Configuration");
}

#[test]
fn indexes_sections_and_returns_cited_passages_without_changing_notes() {
    let f = Fixture::new();
    let note = "---\ntags: [research]\n---\n# Agent research\n\n## Failure auditing\nTrace propagation through agent societies.\n\n## Other work\nUnrelated material.\n";
    let path = f.0.join("vault/research/agents.md");
    fs::write(&path, note).unwrap();
    let report = json(&f.run(&["index", "vault", "index"]));
    assert_eq!(report["files"], 1);
    let result = json(&f.run(&["search", "index", "propagation"]));
    let hits = result["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["path"], "research/agents.md");
    assert_eq!(hits[0]["heading"], "Failure auditing");
    assert_eq!(hits[0]["start_line"], 6);
    assert_eq!(hits[0]["end_line"], 8);
    assert!(hits[0]["excerpt"].as_str().unwrap().contains("propagation"));
    assert_eq!(fs::read_to_string(path).unwrap(), note);
}

#[test]
fn plain_terms_match_any_term_and_more_matches_rank_first() {
    let f = Fixture::new();
    let note = "# Notes\n\n## Both\nalphacanary and betacanary together.\n\n## Only alpha\nalphacanary alone.\n";
    fs::write(f.0.join("vault/research/terms.md"), note).unwrap();
    json(&f.run(&["index", "vault", "index"]));
    let result = json(&f.run(&["search", "index", "alphacanary betacanary missingword"]));
    let headings: Vec<_> = result["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| hit["heading"].as_str().unwrap())
        .collect();
    assert_eq!(headings, ["Both", "Only alpha"]);
}

fn hits(f: &Fixture, query: &str) -> usize {
    json(&f.run(&["search", "index", query]))["hits"]
        .as_array()
        .unwrap()
        .len()
}

#[test]
fn sync_adds_edits_and_removes_only_what_changed() {
    let f = Fixture::new();
    fs::write(f.0.join("vault/keep.md"), "keepcanary").unwrap();
    fs::write(f.0.join("vault/edit.md"), "oldcanary").unwrap();
    fs::write(f.0.join("vault/gone.md"), "gonecanary").unwrap();
    json(&f.run(&["index", "vault", "index"]));

    let report = json(&f.run(&["sync", "vault", "index"]));
    assert_eq!(
        (
            report["added"].as_u64(),
            report["updated"].as_u64(),
            report["removed"].as_u64(),
            report["unchanged"].as_u64()
        ),
        (Some(0), Some(0), Some(0), Some(3))
    );

    fs::write(f.0.join("vault/edit.md"), "newcanary").unwrap();
    fs::write(f.0.join("vault/new.md"), "freshcanary").unwrap();
    fs::remove_file(f.0.join("vault/gone.md")).unwrap();
    let report = json(&f.run(&["sync", "vault", "index"]));
    assert_eq!(
        (
            report["added"].as_u64(),
            report["updated"].as_u64(),
            report["removed"].as_u64(),
            report["unchanged"].as_u64()
        ),
        (Some(1), Some(1), Some(1), Some(1))
    );
    assert_eq!(hits(&f, "oldcanary"), 0);
    assert_eq!(hits(&f, "gonecanary"), 0);
    assert_eq!(hits(&f, "newcanary"), 1);
    assert_eq!(hits(&f, "freshcanary"), 1);
    assert_eq!(hits(&f, "keepcanary"), 1);
}

#[test]
fn sync_does_not_duplicate_an_edited_note_with_several_sections() {
    let f = Fixture::new();
    let note = |word: &str| format!("# One\n{word} first\n\n## Two\n{word} second\n");
    fs::write(f.0.join("vault/multi.md"), note("alpha")).unwrap();
    json(&f.run(&["index", "vault", "index"]));
    fs::write(f.0.join("vault/multi.md"), note("beta")).unwrap();
    json(&f.run(&["sync", "vault", "index"]));
    assert_eq!(hits(&f, "alpha"), 0);
    assert_eq!(hits(&f, "beta"), 2);
}

#[test]
fn sync_builds_a_missing_index_and_an_index_without_a_manifest() {
    let f = Fixture::new();
    fs::write(f.0.join("vault/a.md"), "firstcanary").unwrap();
    let report = json(&f.run(&["sync", "vault", "index"]));
    assert_eq!(report["files"], 1);
    assert_eq!(hits(&f, "firstcanary"), 1);

    fs::remove_file(f.0.join("index/osb-manifest.json")).unwrap();
    fs::write(f.0.join("vault/b.md"), "secondcanary").unwrap();
    let report = json(&f.run(&["sync", "vault", "index"]));
    assert_eq!(report["files"], 2);
    assert_eq!(hits(&f, "secondcanary"), 1);
}

#[test]
fn sync_refuses_an_index_built_from_a_different_vault() {
    let f = Fixture::new();
    fs::write(f.0.join("vault/a.md"), "vaultcanary").unwrap();
    fs::create_dir_all(f.0.join("other")).unwrap();
    fs::write(f.0.join("other/b.md"), "othercanary").unwrap();
    json(&f.run(&["index", "vault", "index"]));
    let result = f.run(&["sync", "other", "index"]);
    assert!(!result.status.success());
    assert_eq!(hits(&f, "vaultcanary"), 1);
}
