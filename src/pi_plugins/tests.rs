use super::*;

use std::io::Write as _;

fn write(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A package laid out by convention, no `pi` manifest.
fn conventional() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "package.json",
        r#"{"name":"pi-fixture","version":"1.2.0","description":"A fixture","keywords":["pi-package"]}"#,
    );
    write(
        root,
        "skills/pdf-tools/SKILL.md",
        "---\nname: pdf-tools\ndescription: Extract text from PDFs.\n---\nRun scripts/extract.sh.\n",
    );
    write(root, "skills/pdf-tools/scripts/extract.sh", "#!/bin/sh\n");
    write(
        root,
        "skills/group/deep/SKILL.md",
        "---\ndescription: >\n  Folded over\n  two lines.\ndisable-model-invocation: true\n---\nbody\n",
    );
    write(root, "skills/loose.md", "No frontmatter at all.\n");
    write(root, "skills/.hidden/SKILL.md", "---\nname: hidden\n---\n");
    write(
        root,
        "prompts/review.md",
        "---\ndescription: Review staged changes\nargument-hint: \"[focus]\"\n---\nReview. Focus on ${1:-correctness}. All: $@\n",
    );
    write(root, "prompts/nested/explain.md", "Explain $1 simply.\n");
    write(root, "prompts/model.md", "Clashes with a built-in.\n");
    write(
        root,
        "extensions/index.ts",
        "export default function () {}\n",
    );
    write(
        root,
        "extensions/other.ts",
        "export default function () {}\n",
    );
    write(root, "themes/dark.json", "{}\n");
    dir
}

fn kinds(items: &[Item], kind: ItemKind) -> Vec<&Item> {
    items.iter().filter(|i| i.kind == kind).collect()
}

#[test]
fn conventional_layout_maps_skills_and_prompts_and_refuses_the_rest() {
    let dir = conventional();
    let root = dir.path();
    let info = layout::read_package_info(root);
    assert_eq!(info.name.as_deref(), Some("pi-fixture"));
    assert!(info.manifest.is_none());
    let items: Vec<Item> = layout::plan(root, &info)
        .into_iter()
        .map(|p| p.item)
        .collect();

    let skills = kinds(&items, ItemKind::Skill);
    let names: Vec<&str> = skills.iter().map(|i| i.name.as_str()).collect();
    // Nested skill dirs are found, a loose .md at the root is a skill, dot
    // directories are not.
    assert_eq!(names, vec!["deep", "loose", "pdf-tools"]);
    assert!(skills.iter().all(|i| i.status == ItemStatus::Ready));
    assert_eq!(skills[2].path.as_deref(), Some("skills/pdf-tools"));

    let prompts = kinds(&items, ItemKind::Prompt);
    let by_name = |n: &str| prompts.iter().find(|i| i.name == n).unwrap();
    assert_eq!(by_name("review").status, ItemStatus::Ready);
    assert_eq!(
        by_name("review").path.as_deref(),
        Some("commands/review.md")
    );
    // Package prompt roots are read recursively, like Pi's collectFiles.
    assert_eq!(by_name("explain").status, ItemStatus::Ready);
    assert_eq!(by_name("model").status, ItemStatus::Skipped);
    assert!(
        by_name("model")
            .reason
            .as_deref()
            .unwrap()
            .contains("/model")
    );

    // An extensions root with index.ts is one extension, as in Pi.
    let extensions = kinds(&items, ItemKind::Extension);
    assert_eq!(extensions.len(), 1);
    assert_eq!(extensions[0].status, ItemStatus::Unsupported);
    assert!(
        extensions[0]
            .reason
            .as_deref()
            .unwrap()
            .starts_with("not supported by Wizard yet")
    );
    let themes = kinds(&items, ItemKind::Theme);
    assert_eq!(themes.len(), 1);
    assert_eq!(themes[0].status, ItemStatus::Unsupported);
}

#[test]
fn a_pi_manifest_replaces_convention_and_honors_globs_and_overrides() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "package.json",
        r#"{"name":"@s/manifest","pi":{
            "skills":["./res/skills","!res/skills/off"],
            "prompts":["res/prompts/*.md","!skip.md"],
            "extensions":["./src/ext.ts"]
        }}"#,
    );
    write(root, "res/skills/on/SKILL.md", "---\ndescription: d\n---\n");
    write(
        root,
        "res/skills/off/SKILL.md",
        "---\ndescription: d\n---\n",
    );
    write(root, "res/prompts/go.md", "Go.\n");
    write(root, "res/prompts/skip.md", "Skip.\n");
    write(root, "src/ext.ts", "export default () => {}\n");
    // Conventional directories are ignored once a manifest exists, and a
    // type the manifest leaves out contributes nothing.
    write(
        root,
        "skills/ignored/SKILL.md",
        "---\ndescription: d\n---\n",
    );
    write(root, "themes/t.json", "{}");

    let info = layout::read_package_info(root);
    assert!(info.manifest.is_some());
    let items: Vec<Item> = layout::plan(root, &info)
        .into_iter()
        .map(|p| p.item)
        .collect();
    let names: Vec<(ItemKind, &str)> = items.iter().map(|i| (i.kind, i.name.as_str())).collect();
    assert_eq!(
        names,
        vec![
            (ItemKind::Skill, "on"),
            (ItemKind::Prompt, "go"),
            (ItemKind::Extension, "src/ext.ts"),
        ]
    );
}

#[test]
fn manifest_paths_cannot_climb_out_of_the_package() {
    let outer = tempfile::tempdir().unwrap();
    write(
        outer.path(),
        "secret/SKILL.md",
        "---\ndescription: x\n---\n",
    );
    let root = outer.path().join("pkg");
    write(
        &root,
        "package.json",
        r#"{"name":"p","pi":{"skills":["../secret","/etc"]}}"#,
    );
    let info = layout::read_package_info(&root);
    assert!(layout::discover(&root, &info, ItemKind::Skill).is_empty());
}

#[test]
fn frontmatter_differences_are_reported_per_skill() {
    let dir = conventional();
    let root = dir.path();
    let deep = layout::plan_skill(root, &root.join("skills/group/deep/SKILL.md")).item;
    assert_eq!(deep.name, "deep");
    assert!(
        deep.notes
            .iter()
            .any(|n| n.contains("disable-model-invocation")),
        "{:?}",
        deep.notes
    );
    // A folded description still counts as a description.
    assert!(!deep.notes.iter().any(|n| n.contains("no description")));

    let loose = layout::plan_skill(root, &root.join("skills/loose.md")).item;
    assert_eq!(loose.name, "loose");
    assert!(loose.notes.iter().any(|n| n.contains("no description")));

    let clean = layout::plan_skill(root, &root.join("skills/pdf-tools/SKILL.md")).item;
    assert!(clean.notes.is_empty());

    // The Pi-only keys are read from the raw block; nested lines are not
    // top-level keys.
    let pairs = layout::frontmatter_pairs("---\nname: a\nmetadata:\n  always: true\n---\n");
    assert_eq!(
        pairs,
        vec![("name".into(), "a".into()), ("metadata".into(), "".into())]
    );
    assert!(layout::frontmatter_pairs("no fences").is_empty());
    assert!(layout::frontmatter_pairs("---\nname: a\nunclosed\n").is_empty());
}

#[test]
fn wizard_loads_an_installed_skill_with_its_description() {
    let dir = conventional();
    let home = tempfile::tempdir().unwrap();
    install_from(&Source::Local(dir.path().into()), dir.path(), home.path()).unwrap();
    let skills = crate::skills::load_skills(&[home.path().join("skills")]).unwrap();
    let deep = skills.iter().find(|s| s.name == "deep").unwrap();
    assert_eq!(
        deep.meta.description.as_deref(),
        Some("Folded over two lines.")
    );
    let pdf = skills.iter().find(|s| s.name == "pdf-tools").unwrap();
    assert!(
        pdf.path
            .parent()
            .unwrap()
            .join("scripts/extract.sh")
            .is_file()
    );
    assert!(skills.iter().any(|s| s.name == "loose"));
    assert!(!skills.iter().any(|s| s.name == "hidden"));
}

#[test]
fn prompts_become_pi_syntax_commands() {
    let text = layout::to_command(
        "---\ndescription: Review staged changes\nargument-hint: \"[focus]\"\n---\nReview ${1:-all}.\n",
    );
    assert_eq!(
        text,
        "---\ndescription: Review staged changes\nsyntax: pi\n---\nReview ${1:-all}.\n"
    );
    // No description: Pi's fallback, the first line cut at 60 characters.
    let long = "x".repeat(70);
    let text = layout::to_command(&format!("\n{long}\nmore\n"));
    assert!(text.contains(&format!("description: {}...\n", "x".repeat(60))));

    let dir = conventional();
    let home = tempfile::tempdir().unwrap();
    install_from(&Source::Local(dir.path().into()), dir.path(), home.path()).unwrap();
    let commands = crate::commands::load_from_dirs(&[home.path().join("commands")]);
    let review = commands.iter().find(|c| c.name == "review").unwrap();
    assert!(review.pi_syntax);
    assert!(review.expects_args());
    assert_eq!(review.description.as_deref(), Some("Review staged changes"));
    assert_eq!(
        crate::commands::expand_custom("/review \"API shape\" docs", &commands).unwrap(),
        "Review. Focus on API shape. All: API shape docs"
    );
    assert_eq!(
        crate::commands::expand_custom("/review", &commands).unwrap(),
        "Review. Focus on correctness. All: "
    );
}

#[test]
fn install_records_what_it_wrote_and_remove_deletes_only_that() {
    let dir = conventional();
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    // Something the user wrote, with a name the package also uses.
    write(home, "skills/loose/SKILL.md", "mine\n");

    let source = Source::Local(dir.path().into());
    let plugin = install_from(&source, dir.path(), home).unwrap();
    assert_eq!(plugin.name, "pi-fixture");
    assert_eq!(plugin.version.as_deref(), Some("1.2.0"));
    assert!(plugin.installed_at.is_some());
    let loose = plugin.items.iter().find(|i| i.name == "loose").unwrap();
    assert_eq!(loose.status, ItemStatus::Skipped);
    assert!(loose.reason.as_deref().unwrap().contains("already exists"));
    assert_eq!(
        std::fs::read_to_string(home.join("skills/loose/SKILL.md")).unwrap(),
        "mine\n"
    );
    assert!(home.join("skills/pdf-tools/SKILL.md").is_file());
    assert!(home.join("commands/review.md").is_file());
    assert!(!home.join("commands/model.md").exists());

    let store = Store::load(home).unwrap();
    assert_eq!(store.plugins.len(), 1);
    let installed: Vec<&str> = store.plugins[0]
        .items
        .iter()
        .filter(|i| i.status == ItemStatus::Installed)
        .filter_map(|i| i.path.as_deref())
        .collect();
    assert_eq!(
        installed,
        vec![
            "skills/deep",
            "skills/pdf-tools",
            "commands/explain.md",
            "commands/review.md"
        ]
    );

    // A reinstall of a version without one of the prompts drops that
    // prompt's command and keeps the rest.
    std::fs::remove_file(dir.path().join("prompts/nested/explain.md")).unwrap();
    install_from(&source, dir.path(), home).unwrap();
    assert!(!home.join("commands/explain.md").exists());
    assert!(home.join("commands/review.md").is_file());
    assert_eq!(Store::load(home).unwrap().plugins.len(), 1);

    // Remove by npm-style spec finds it by name.
    let removed = remove_from("npm:pi-fixture", home).unwrap();
    assert_eq!(removed.name, "pi-fixture");
    assert!(!home.join("skills/pdf-tools").exists());
    assert!(!home.join("commands/review.md").exists());
    assert!(
        home.join("skills/loose/SKILL.md").is_file(),
        "the user's skill stays"
    );
    assert!(Store::load(home).unwrap().plugins.is_empty());
    assert!(remove_from("pi-fixture", home).is_err());
}

#[test]
fn a_second_package_cannot_take_over_the_firsts_files() {
    let first = conventional();
    let second = conventional();
    write(second.path(), "package.json", r#"{"name":"other"}"#);
    let home = tempfile::tempdir().unwrap();
    install_from(
        &Source::Local(first.path().into()),
        first.path(),
        home.path(),
    )
    .unwrap();
    let other = install_from(
        &Source::Local(second.path().into()),
        second.path(),
        home.path(),
    )
    .unwrap();
    let pdf = other.items.iter().find(|i| i.name == "pdf-tools").unwrap();
    assert_eq!(pdf.status, ItemStatus::Skipped);
    assert!(pdf.reason.as_deref().unwrap().contains("pi-fixture"));
    // Removing the second leaves the first's files alone.
    remove_from("other", home.path()).unwrap();
    assert!(home.path().join("skills/pdf-tools/SKILL.md").is_file());
}

#[test]
fn recorded_paths_outside_skills_and_commands_are_never_touched() {
    let home = tempfile::tempdir().unwrap();
    write(home.path(), "config.toml", "keep\n");
    assert!(resolve(home.path(), "config.toml").is_none());
    assert!(resolve(home.path(), "skills/../config.toml").is_none());
    assert!(resolve(home.path(), "/etc/passwd").is_none());
    assert!(resolve(home.path(), "skills/ok").is_some());
    assert!(!delete_path(home.path(), "config.toml"));
    assert!(home.path().join("config.toml").is_file());
}

#[test]
fn list_marks_what_the_loaders_actually_resolve() {
    let dir = conventional();
    let home = tempfile::tempdir().unwrap();
    let plugin = install_from(&Source::Local(dir.path().into()), dir.path(), home.path()).unwrap();
    let mut plugins = vec![plugin];
    // A later root with the same skill name shadows the install.
    let shadow = tempfile::tempdir().unwrap();
    write(shadow.path(), "deep/SKILL.md", "---\nname: deep\n---\n");
    let skills =
        crate::skills::load_skills(&[home.path().join("skills"), shadow.path().into()]).unwrap();
    let commands = crate::commands::load_from_dirs(&[home.path().join("commands")]);
    mark_loaded(&mut plugins, home.path(), &skills, &commands);
    let loaded = |name: &str| {
        plugins[0]
            .items
            .iter()
            .find(|i| i.name == name && i.status == ItemStatus::Installed)
            .and_then(|i| i.loaded)
    };
    assert_eq!(loaded("pdf-tools"), Some(true));
    assert_eq!(loaded("review"), Some(true));
    assert_eq!(loaded("deep"), Some(false));
    // Unsupported items carry no loaded flag at all.
    assert!(
        plugins[0]
            .items
            .iter()
            .filter(|i| i.status == ItemStatus::Unsupported)
            .all(|i| i.loaded.is_none())
    );
}

#[test]
fn summary_counts_what_works_and_what_does_not() {
    let dir = conventional();
    let (plugin, _) = plan_package(&Source::Local(dir.path().into()), dir.path());
    assert_eq!(
        plugin.summary(),
        (
            "3 skills, 2 prompts".to_string(),
            "1 prompt, 1 extension, 1 theme".to_string()
        )
    );
}

#[test]
fn json_report_schema_is_stable() {
    let dir = conventional();
    let home = tempfile::tempdir().unwrap();
    let plugin = install_from(&Source::Local(dir.path().into()), dir.path(), home.path()).unwrap();
    let report = Report::new(
        Target::Both,
        Some(Outcome {
            ok: true,
            error: None,
            plugin: Some(plugin),
            source: None,
        }),
        Some(Outcome::failed(anyhow!("Pi isn't installed"))),
    );
    let value = serde_json::to_value(&report).unwrap();
    assert_eq!(value["schema"], 1);
    assert_eq!(value["beta"], true);
    assert_eq!(value["ok"], false);
    assert_eq!(value["target"], "both");
    assert_eq!(value["wizard"]["ok"], true);
    assert_eq!(value["wizard"]["plugin"]["name"], "pi-fixture");
    assert_eq!(value["wizard"]["plugin"]["version"], "1.2.0");
    let item = &value["wizard"]["plugin"]["items"][0];
    for key in ["kind", "name", "status", "from", "path"] {
        assert!(item.get(key).is_some(), "item lacks {key}: {item}");
    }
    assert_eq!(item["kind"], "skill");
    assert_eq!(item["status"], "installed");
    assert!(value["wizard"]["plugin"].get("installedAt").is_some());
    assert_eq!(value["pi"]["ok"], false);
    assert_eq!(value["pi"]["error"], "Pi isn't installed");
    // And it reads back into the same type the desktop app deserializes.
    let back: Report = serde_json::from_value(value).unwrap();
    assert_eq!(back, report);

    let error = json_error(&anyhow!("boom"));
    assert_eq!(error["ok"], false);
    assert_eq!(error["error"], "boom");
}

#[test]
fn specs_parse_the_way_pi_reads_them() {
    let spec = |s: &str| source::parse(s).unwrap().spec();
    assert_eq!(spec("pi-tools"), "npm:pi-tools");
    assert_eq!(spec("@scope/tools@^1"), "npm:@scope/tools@^1");
    assert_eq!(spec("npm:@scope/tools"), "npm:@scope/tools");
    assert_eq!(spec("github:o/r"), "git:github.com/o/r");
    assert_eq!(spec("github.com/o/r@v1"), "git:github.com/o/r@v1");
    assert_eq!(spec("git:github.com/o/r"), "git:github.com/o/r");
    assert_eq!(spec("git@github.com:o/r.git"), "git:git@github.com:o/r");
    assert_eq!(spec("https://github.com/o/r"), "https://github.com/o/r");
    match source::parse("git:github.com/o/r@v2").unwrap() {
        Source::Git { url, reference, .. } => {
            assert_eq!(url, "https://github.com/o/r");
            assert_eq!(reference.as_deref(), Some("v2"));
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(source::parse("./pkg").unwrap(), Source::Local(_)));
    for bad in [
        "",
        "--local",
        "-e",
        "npm:",
        "npm:UPPER",
        "has space",
        "https://github.com/only",
        "github.com/o/r@-x",
        "Not_A_Name!",
    ] {
        assert!(source::parse(bad).is_err(), "{bad:?} should be refused");
    }
}

#[test]
fn npm_versions_resolve_exact_tag_and_range() {
    let packument = serde_json::json!({
        "dist-tags": {"latest": "1.4.0", "next": "2.0.0-beta.1"},
        "versions": {"1.0.0": {}, "1.4.0": {}, "1.5.0-rc.1": {}, "2.0.0-beta.1": {}}
    });
    let pick = |w: Option<&str>| source::pick_version(&packument, w);
    assert_eq!(pick(None).as_deref(), Some("1.4.0"));
    assert_eq!(pick(Some("1.0.0")).as_deref(), Some("1.0.0"));
    assert_eq!(pick(Some("next")).as_deref(), Some("2.0.0-beta.1"));
    assert_eq!(pick(Some("^1")).as_deref(), Some("1.4.0"));
    assert_eq!(pick(Some("1.x")).as_deref(), Some("1.4.0"));
    assert_eq!(pick(Some("^3")), None);
}

#[test]
fn tarballs_are_checked_and_unpacked_without_escaping() {
    use base64::Engine as _;
    use sha2::Digest as _;

    let mut builder = tar::Builder::new(Vec::new());
    let mut add = |path: &str, data: &[u8]| {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        // `set_path` refuses `..`, which is the point of the test, so the
        // name goes into the raw header.
        header.as_old_mut().name[..path.len()].copy_from_slice(path.as_bytes());
        header.set_cksum();
        builder.append(&header, data).unwrap();
    };
    add("package/package.json", br#"{"name":"t"}"#);
    add("package/skills/a/SKILL.md", b"---\ndescription: d\n---\n");
    add("package/../../escaped.txt", b"nope");
    let tar = builder.into_inner().unwrap();
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(&tar).unwrap();
    let bytes = gz.finish().unwrap();

    let good = format!(
        "sha512-{}",
        base64::engine::general_purpose::STANDARD.encode(sha2::Sha512::digest(&bytes))
    );
    source::verify_integrity(&bytes, Some(&good)).unwrap();
    assert!(source::verify_integrity(&bytes, Some("sha512-AAAA")).is_err());
    assert!(source::verify_integrity(&bytes, Some("sha1-abc")).is_err());
    assert!(source::verify_integrity(&bytes, None).is_err());

    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("out").join("package");
    source::unpack_npm(&bytes, &dest).unwrap();
    assert!(dest.join("package.json").is_file());
    assert!(dest.join("skills/a/SKILL.md").is_file());
    assert!(!dir.path().join("escaped.txt").exists());
    assert!(!dir.path().join("out/escaped.txt").exists());
}

#[test]
fn gallery_search_hits_parse() {
    let body = serde_json::json!({"objects":[
        {"downloads":{"weekly":337311},"package":{"name":"pi-mcp-adapter","version":"2.38.0",
         "description":" MCP adapter ","publisher":{"username":"nico"}}},
        {"package":{"name":"@s/p","version":"0.1.0"}},
        {"package":{"version":"no name"}}
    ]});
    let hits = parse_search(&body);
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].source, "npm:pi-mcp-adapter");
    assert_eq!(hits[0].description.as_deref(), Some("MCP adapter"));
    assert_eq!(hits[0].publisher.as_deref(), Some("nico"));
    assert_eq!(hits[0].weekly_downloads, Some(337_311));
    assert_eq!(hits[1].weekly_downloads, None);
}

#[test]
fn targets_split_into_harnesses() {
    assert!(Target::Both.wizard() && Target::Both.pi());
    assert!(Target::Wizard.wizard() && !Target::Wizard.pi());
    assert!(!Target::Pi.wizard() && Target::Pi.pi());
    assert_eq!(serde_json::to_value(Target::Both).unwrap(), "both");
}

#[test]
fn a_package_with_nothing_pi_can_load_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "brave-search/SKILL.md",
        "---\ndescription: d\n---\n",
    );
    let home = tempfile::tempdir().unwrap();
    let err = install_from(&Source::Local(dir.path().into()), dir.path(), home.path())
        .unwrap_err()
        .to_string();
    assert!(err.contains("no Pi resources"), "{err}");
    assert!(Store::load(home.path()).unwrap().plugins.is_empty());
}

#[test]
fn a_package_wizard_cannot_run_any_of_is_refused_for_wizard() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "package.json", r#"{"name":"ext-only"}"#);
    write(
        dir.path(),
        "extensions/index.ts",
        "export default () => {}\n",
    );
    let home = tempfile::tempdir().unwrap();
    let err = install_from(&Source::Local(dir.path().into()), dir.path(), home.path())
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("nothing in ext-only runs in Wizard yet (1 extension)"),
        "{err}"
    );
    assert!(Store::load(home.path()).unwrap().plugins.is_empty());
}
