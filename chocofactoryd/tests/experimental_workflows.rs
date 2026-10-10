//! The experimental workflows under `workflows/experimental/` are not built
//! in, so nothing else loads them. These tests keep them loadable, keep their
//! copies of shipped files identical, and load the example in
//! `docs/workflows.md`.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use chocofactoryd::workflow_def::{StageKind, WorkflowDefinition};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn exp() -> PathBuf {
    root().join("workflows/experimental")
}

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!("chocofactoryd-expwf-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const COPIES: [&str; 9] = [
    "prompts/planner-system.md",
    "prompts/planner-turn.md",
    "prompts/coder-system.md",
    "prompts/coder-turn-planned.md",
    "prompts/reviewer-system.md",
    "prompts/reviewer-turn-planned.md",
    "scripts/open-pr.sh",
    "scripts/ci-checks.sh",
    "scripts/await-review.sh",
];

const NEW_FILES: [&str; 3] = [
    "prompts/security-review.md",
    "prompts/architecture-review.md",
    "prompts/lead-review.md",
];

fn load(path: &Path) -> WorkflowDefinition {
    WorkflowDefinition::load(path)
        .unwrap_or_else(|e| panic!("{} failed to load: {e}", path.display()))
}

fn agent_turn_parts(kind: &StageKind) -> (&str, Option<&PathBuf>, &Vec<String>) {
    match kind {
        StageKind::AgentTurn {
            role,
            prompt_file,
            report_sections,
            ..
        } => (role, prompt_file.as_ref(), report_sections),
        other => panic!("expected agent_turn, got {}", other.name()),
    }
}

#[test]
fn every_experimental_workflow_loads() {
    let mut files = Vec::new();
    for entry in fs::read_dir(exp()).unwrap() {
        let path = entry.unwrap().path();
        let ext = path.extension().and_then(|e| e.to_str());
        if path.is_file() && matches!(ext, Some("yaml" | "yml")) {
            files.push(path);
        }
    }
    assert!(
        !files.is_empty(),
        "workflows/experimental/ holds no YAML workflow"
    );
    for f in files {
        if let Err(e) = WorkflowDefinition::load(&f) {
            panic!("{} does not load: {e}", f.display());
        }
    }
}

#[test]
fn review_panel_has_the_decided_shape() {
    let def = load(&exp().join("review-panel.yaml"));
    let groups: Vec<_> = def
        .stages
        .iter()
        .filter(|(_, s)| matches!(s.kind, StageKind::Parallel { .. }))
        .collect();
    assert_eq!(groups.len(), 1, "exactly one parallel stage");
    let (name, group) = groups[0];
    assert_eq!(name, "review_panel");
    let StageKind::Parallel { branches } = &group.kind else {
        unreachable!()
    };
    let names: Vec<&str> = branches.keys().map(String::as_str).collect();
    assert_eq!(
        names,
        ["security_review", "architecture_review", "internal_review"]
    );
    for (bname, b) in branches {
        let (role, _, _) = agent_turn_parts(&b.def.kind);
        let r = def
            .roles
            .get(role)
            .unwrap_or_else(|| panic!("branch {bname}: role {role} missing"));
        assert!(r.read_only, "branch {bname}: role {role} is not read_only");
    }
    let clean = vec!["clean".to_string(), "blocking".to_string()];
    assert_eq!(branches["security_review"].results, clean);
    assert_eq!(branches["architecture_review"].results, clean);
    assert_eq!(
        branches["internal_review"].results,
        ["approved", "changes_requested"]
    );
    let on: Vec<_> = group.on.iter().collect();
    assert_eq!(on, [(&"done".to_string(), &"lead_review".to_string())]);

    let lead = &def.stages["lead_review"];
    assert_eq!(lead.on["approved"], "open_pr");
    assert_eq!(lead.on["changes_requested"], "revising");
    assert_eq!(lead.on.len(), 2);
    let g = lead.loop_guard.as_ref().expect("lead_review loop guard");
    assert_eq!(
        (g.on.as_str(), g.max, g.then.as_str()),
        ("changes_requested", 3, "escalate_to_human")
    );

    let (_, pf, sections) = agent_turn_parts(&branches["internal_review"].def.kind);
    let pf = pf.expect("internal_review prompt_file");
    assert_eq!(pf.file_name().unwrap(), "reviewer-turn-planned.md");
    assert_eq!(
        fs::read(pf).unwrap(),
        fs::read(root().join("workflows/prompts/reviewer-turn-planned.md")).unwrap()
    );
    let shipped = load(&root().join("workflows/coding-task-planned.yaml"));
    let (_, _, shipped_sections) = agent_turn_parts(&shipped.stages["internal_review"].kind);
    assert_eq!(sections, shipped_sections);
}

#[test]
fn copied_files_are_byte_identical() {
    for rel in COPIES {
        let copy = fs::read(exp().join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"));
        let orig = fs::read(root().join("workflows").join(rel)).unwrap();
        assert!(copy == orig, "{rel} differs from the shipped original");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let om = fs::metadata(root().join("workflows").join(rel)).unwrap();
            let cm = fs::metadata(exp().join(rel)).unwrap();
            if om.permissions().mode() & 0o111 != 0 {
                assert!(
                    cm.permissions().mode() & 0o111 != 0,
                    "{rel} lost its executable bit"
                );
            }
        }
    }
}

fn list(dir: &Path, prefix: &str, out: &mut BTreeSet<String>) {
    for e in fs::read_dir(dir).unwrap() {
        let e = e.unwrap();
        if e.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let name = format!("{prefix}{}", e.file_name().to_string_lossy());
        if e.path().is_dir() {
            list(&e.path(), &format!("{name}/"), out);
        } else {
            out.insert(name);
        }
    }
}

#[test]
fn no_unlisted_fork_of_a_shipped_file() {
    let mut found = BTreeSet::new();
    list(&exp().join("prompts"), "prompts/", &mut found);
    list(&exp().join("scripts"), "scripts/", &mut found);
    let mut expected: BTreeSet<String> = COPIES.iter().map(|s| s.to_string()).collect();
    expected.insert("prompts/coder-revise-planned.md".into());
    expected.extend(NEW_FILES.iter().map(|s| s.to_string()));
    assert_eq!(
        found, expected,
        "unlisted or missing file under experimental/"
    );

    let revise = fs::read_to_string(exp().join("prompts/coder-revise-planned.md")).unwrap();
    let orig =
        fs::read_to_string(root().join("workflows/prompts/coder-revise-planned.md")).unwrap();
    assert_ne!(revise, orig);
    assert!(revise.contains("{{ stages.lead_review.summary }}"));
    assert!(!revise.contains("{{ stages.internal_review"));

    let mut shipped = Vec::new();
    for d in ["workflows/prompts", "workflows/scripts"] {
        for e in fs::read_dir(root().join(d)).unwrap() {
            shipped.push((
                e.as_ref().unwrap().path(),
                fs::read(e.unwrap().path()).unwrap(),
            ));
        }
    }
    for rel in NEW_FILES {
        let bytes = fs::read(exp().join(rel)).unwrap();
        for (p, b) in &shipped {
            assert!(&bytes != b, "{rel} is byte-identical to {}", p.display());
        }
    }
}

#[test]
fn the_docs_parallel_example_loads() {
    let docs = fs::read_to_string(root().join("docs/workflows.md")).unwrap();
    let after = docs
        .split_once("\n## Parallel stages\n")
        .expect("docs/workflows.md has no '## Parallel stages' section")
        .1;
    let block = after
        .split_once("```yaml\n")
        .expect("no yaml block in the Parallel stages section")
        .1
        .split_once("\n```")
        .expect("unterminated yaml block")
        .0;
    assert!(block.contains("kind: parallel"));
    let tmp = TempDir::new();
    fs::write(tmp.0.join("example.yaml"), block).unwrap();
    let re = regex::Regex::new(r"(?m)(?:system_)?prompt_file:\s*(\S+)").unwrap();
    for c in re.captures_iter(block) {
        let p = tmp.0.join(&c[1]);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, "Review the change.\n").unwrap();
    }
    let def = load(&tmp.0.join("example.yaml"));
    let groups: Vec<_> = def
        .stages
        .values()
        .filter_map(|s| match &s.kind {
            StageKind::Parallel { branches } => Some(branches),
            _ => None,
        })
        .collect();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].len(), 2);
    for (n, b) in groups[0] {
        let (role, _, _) = agent_turn_parts(&b.def.kind);
        assert!(def.roles[role].read_only, "branch {n} role not read_only");
    }
}
