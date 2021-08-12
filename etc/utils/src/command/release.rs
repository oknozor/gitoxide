use crate::command::release::Options;
use anyhow::{anyhow, bail};
use bstr::ByteSlice;
use cargo_metadata::{
    camino::{Utf8Component, Utf8Path, Utf8PathBuf},
    DependencyKind, Metadata, Package, PackageId,
};
use dia_semver::Semver;
use git_repository::{
    actor,
    hash::ObjectId,
    object,
    odb::{pack, Find, FindExt},
    refs::{
        file,
        file::loose::reference::peel,
        mutable::Target,
        packed,
        transaction::{Change, Create, RefEdit},
    },
    Repository,
};
use std::{collections::BTreeSet, convert::TryInto, path::PathBuf, process::Command, str::FromStr};

struct State {
    root: Utf8PathBuf,
    seen: BTreeSet<String>,
    repo: Repository,
    packed_refs: Option<packed::Buffer>,
}

impl State {
    fn new(repo_path: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let root = repo_path.into();
        let repo = git_repository::discover(&root)?;
        let packed_refs = repo.refs.packed()?;
        Ok(State {
            root: root.try_into()?,
            seen: BTreeSet::new(),
            repo,
            packed_refs,
        })
    }
}

fn will(not_really: bool) -> &'static str {
    if not_really {
        "WOULD"
    } else {
        "Will"
    }
}

/// In order to try dealing with https://github.com/sunng87/cargo-release/issues/224 and also to make workspace
/// releases more selective.
pub fn release(options: Options, version_bump_spec: String, crates: Vec<String>) -> anyhow::Result<()> {
    if crates.is_empty() {
        bail!("Please provide at least one crate name which also is a workspace member");
    }
    release_depth_first(options, crates, &version_bump_spec)?;
    Ok(())
}

fn is_workspace_member(meta: &Metadata, crate_name: &str) -> bool {
    meta.packages
        .iter()
        .find(|p| p.name == crate_name)
        .map_or(false, |p| meta.workspace_members.iter().any(|m| m == &p.id))
}

fn release_depth_first(options: Options, crate_names: Vec<String>, bump_spec: &str) -> anyhow::Result<()> {
    let meta = cargo_metadata::MetadataCommand::new().exec()?;
    let mut state = State::new(std::env::current_dir()?)?;
    let mut names_to_publish = Vec::new();
    let mut index = 0;
    for crate_name in crate_names {
        names_to_publish.push(crate_name);
        while let Some(crate_name) = names_to_publish.get(index) {
            let package = meta
                .packages
                .iter()
                .find(|p| &p.name == crate_name)
                .ok_or_else(|| anyhow!("workspace member must be a listed package: '{}'", crate_name))?;
            for dependency in package.dependencies.iter().filter(|d| d.kind == DependencyKind::Normal) {
                if state.seen.contains(&dependency.name) || !is_workspace_member(&meta, &dependency.name) {
                    continue;
                }
                state.seen.insert(dependency.name.clone());
                names_to_publish.push(dependency.name.clone());
            }
            index += 1;
        }
    }

    for crate_name in names_to_publish.iter().rev() {
        let package = meta
            .packages
            .iter()
            .find(|p| &p.name == crate_name)
            .expect("crate still there");

        if changed_since_last_release(package, &state)? {
            let (new_version, commit_id) = perform_release(&meta, package, options, bump_spec, &state)?;

            let tag_name = tag_name_for(&package.name, &new_version.to_string());
            if options.dry_run {
                log::info!("WOULD create tag {}", tag_name);
            } else {
                for tag in state
                    .repo
                    .refs
                    .transaction()
                    .prepare(
                        Some(RefEdit {
                            change: Change::Update {
                                log: Default::default(),
                                mode: Create::Only,
                                new: Target::Peeled(commit_id),
                            },
                            name: format!("refs/tags/{}", tag_name).try_into()?,
                            deref: false,
                        }),
                        git_lock::acquire::Fail::Immediately,
                    )?
                    .commit(&actor::Signature::empty())?
                {
                    log::info!("Created tag {}", tag.name.as_bstr());
                }
            }
        } else {
            log::info!(
                "{} v{}  - skipped release as it didn't change",
                package.name,
                package.version
            );
        }
    }

    Ok(())
}

fn perform_release(
    meta: &Metadata,
    package: &Package,
    options: Options,
    bump_spec: &str,
    state: &State,
) -> anyhow::Result<(Semver, ObjectId)> {
    let new_version = bump_version(&package.version.to_string(), bump_spec)?;
    log::info!("{} release {} v{}", will(options.dry_run), package.name, new_version);
    let commit_id = edit_manifest_and_fixup_dependent_crates(meta, package, &new_version, options, state)?;
    publish_crate(package, options)?;
    Ok((new_version, commit_id))
}

fn publish_crate(package: &Package, Options { dry_run, allow_dirty }: Options) -> anyhow::Result<()> {
    let max_attempts = 3;
    for attempt in 1..=max_attempts {
        let mut c = Command::new("cargo");
        c.arg("publish");

        if allow_dirty {
            c.arg("--allow-dirty");
        }
        c.arg("--manifest-path").arg(&package.manifest_path);
        log::info!("{} run {:?}", will(dry_run), c);
        if dry_run || c.status()?.success() {
            break;
        } else if attempt == max_attempts {
            bail!("Could not successfully execute 'cargo publish' even ")
        } else {
            log::warn!(
                "'cargo publish' run {} failed but we retry up to {} times to rule out flakiness",
                attempt,
                max_attempts
            );
        }
    }
    Ok(())
}

fn edit_manifest_and_fixup_dependent_crates(
    meta: &Metadata,
    publishee: &Package,
    new_version: &Semver,
    Options { dry_run, allow_dirty }: Options,
    state: &State,
) -> anyhow::Result<ObjectId> {
    if !allow_dirty {
        assure_clean_working_tree()?;
    }
    let mut package_manifest_lock = git_lock::File::acquire_to_update_resource(
        &publishee.manifest_path,
        git_lock::acquire::Fail::Immediately,
        None,
    )?;
    let mut packages_to_fix = meta
        .workspace_members
        .iter()
        .filter(|id| *id != &publishee.id)
        .map(|id| id_to_package(meta, id))
        .filter(|p| p.dependencies.iter().any(|dep| dep.name == publishee.name))
        .map(|p| {
            git_lock::File::acquire_to_update_resource(&p.manifest_path, git_lock::acquire::Fail::Immediately, None)
                .map(|l| (p, l))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let new_version = new_version.to_string();
    set_manifest_version(publishee, &new_version, &mut package_manifest_lock)?;
    for (package_to_update, out) in packages_to_fix.iter_mut() {
        update_package_dependency(package_to_update, &publishee.name, &new_version, out)?;
    }

    let message = format!("Release {} v{}", publishee.name, new_version);
    if dry_run {
        log::info!("WOULD commit changes to manifests with {:?}", message);
        Ok(ObjectId::null_sha1())
    } else {
        log::info!("Committing changes to manifests");
        package_manifest_lock.commit()?;
        for (_, lock) in packages_to_fix {
            lock.commit()?;
        }
        refresh_cargo_lock(publishee)?;
        commit_changes(message, state)
    }
}

fn id_to_package<'a>(meta: &'a Metadata, id: &PackageId) -> &'a Package {
    meta.packages
        .iter()
        .find(|p| &p.id == id)
        .expect("workspace members are in packages")
}

fn refresh_cargo_lock(package: &Package) -> anyhow::Result<()> {
    cargo_metadata::MetadataCommand::new()
        .manifest_path(&package.manifest_path)
        .exec()?;
    Ok(())
}

fn assure_clean_working_tree() -> anyhow::Result<()> {
    let tracked_changed = !Command::new("git")
        .arg("diff")
        .arg("HEAD")
        .arg("--exit-code")
        .arg("--name-only")
        .status()?
        .success();
    if tracked_changed {
        bail!("Detected working tree changes. Please commit beforehand as otherwise these would be committed as part of manifest changes.")
    }

    let has_untracked = !Command::new("git")
        .arg("ls-files")
        .arg("--exclude-standard")
        .arg("--others")
        .output()?
        .stdout
        .as_slice()
        .trim()
        .is_empty();

    if has_untracked {
        bail!("Found untracked files which would possibly be packaged when publishing.")
    }
    Ok(())
}

fn commit_changes(message: impl AsRef<str>, state: &State) -> anyhow::Result<ObjectId> {
    // TODO: replace with gitoxide one day
    if !Command::new("git")
        .arg("commit")
        .arg("-am")
        .arg(message.as_ref())
        .status()?
        .success()
    {
        bail!("Failed to commit changed manifests");
    }
    Ok(state
        .repo
        .refs
        .loose_find_existing("HEAD")?
        .peel_to_id_in_place(&state.repo.refs, state.packed_refs.as_ref(), peel::none)?
        .to_owned())
}

fn update_package_dependency(
    package_to_update: &Package,
    name_to_find: &str,
    new_version: &str,
    mut out: impl std::io::Write,
) -> anyhow::Result<()> {
    let manifest = std::fs::read_to_string(&package_to_update.manifest_path)?;
    let mut doc = toml_edit::Document::from_str(&manifest)?;
    for dep_type in &["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(name_table) = doc
            .as_table_mut()
            .get_mut(dep_type)
            .and_then(|deps| deps.as_table_mut())
            .and_then(|deps| deps.get_mut(name_to_find).and_then(|name| name.as_inline_table_mut()))
        {
            log::info!(
                "Pending '{}' manifest {} update: '{} = \"{}\"'",
                package_to_update.name,
                dep_type,
                name_to_find,
                new_version,
            );
            *name_table.get_or_insert("version", new_version) = toml_edit::Value::from(new_version);
        }
    }
    out.write_all(doc.to_string_in_original_order().as_bytes())?;

    Ok(())
}

fn set_manifest_version(package: &Package, new_version: &str, mut out: impl std::io::Write) -> anyhow::Result<()> {
    let manifest = std::fs::read_to_string(&package.manifest_path)?;
    let mut doc = toml_edit::Document::from_str(&manifest)?;
    doc["package"]["version"] = toml_edit::value(new_version);
    log::info!(
        "Pending '{}' manifest version update: \"{}\"",
        package.name,
        new_version
    );
    out.write_all(doc.to_string_in_original_order().as_bytes())?;
    Ok(())
}

fn bump_version(version: &str, bump_spec: &str) -> anyhow::Result<Semver> {
    let v = Semver::parse(version).map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err.to_string()))?;
    Ok(match bump_spec {
        "major" => v.new_major(),
        "minor" => v.new_minor(),
        "patch" => v.new_patch(),
        _ => bail!("Invalid version specification: '{}'", bump_spec),
    }
    .expect("no overflow"))
}

fn tag_name_for(package: &str, version: &str) -> String {
    format!("{}-v{}", package, version)
}

fn changed_since_last_release(package: &Package, state: &State) -> anyhow::Result<bool> {
    let version_tag_name = tag_name_for(&package.name, &package.version.to_string());
    let mut tag_ref = match state.repo.refs.find(&version_tag_name, state.packed_refs.as_ref())? {
        None => {
            log::info!(
                "Package {} wasn't tagged with {} yet and thus needs a release",
                package.name,
                version_tag_name
            );
            return Ok(true);
        }
        Some(r) => r,
    };
    let repo_relative_crate_dir = package
        .manifest_path
        .parent()
        .expect("parent of a file is always present")
        .strip_prefix(&state.root)
        .expect("workspace members are releative to the root directory");

    let target = peel_ref_fully(&mut state.repo.refs.find_existing("HEAD", None)?, state)?;
    let released_target = peel_ref_fully(&mut tag_ref, state)?;

    let mut buf = Vec::new();

    let current_dir_id = find_directory_id_in_tree(
        repo_relative_crate_dir,
        resolve_tree_id_from_ref_target(target, &state.repo, &mut buf)?,
        &state.repo,
        &mut buf,
    )?;
    let released_dir_id = find_directory_id_in_tree(
        repo_relative_crate_dir,
        resolve_tree_id_from_ref_target(released_target, &state.repo, &mut buf)?,
        &state.repo,
        &mut buf,
    )?;

    Ok(released_dir_id != current_dir_id)
}

fn find_directory_id_in_tree(
    path: &Utf8Path,
    id: ObjectId,
    repo: &Repository,
    buf: &mut Vec<u8>,
) -> anyhow::Result<ObjectId> {
    let mut tree_id = None::<ObjectId>;

    for component in path.components() {
        match component {
            Utf8Component::Normal(c) => {
                let mut tree_iter = repo
                    .odb
                    .find_existing(tree_id.take().unwrap_or(id), buf, &mut pack::cache::Never)?
                    .into_tree_iter()
                    .expect("tree");
                tree_id = tree_iter
                    .find_map(|e| {
                        let e = e.expect("tree parseable");
                        (e.filename == c).then(|| e.oid)
                    })
                    .map(ToOwned::to_owned);
                if tree_id.is_none() {
                    break;
                }
            }
            _ => panic!(
                "only normal components are expected in relative manifest paths: '{}'",
                path
            ),
        }
    }

    tree_id.ok_or_else(|| anyhow!("path '{}' didn't exist in tree {}", path, id))
}

fn peel_ref_fully(reference: &mut file::Reference<'_>, state: &State) -> anyhow::Result<ObjectId> {
    reference
        .peel_to_id_in_place(&state.repo.refs, state.packed_refs.as_ref(), |oid, buf| {
            state
                .repo
                .odb
                .find(oid, buf, &mut pack::cache::Never)
                .map(|r| r.map(|obj| (obj.kind, obj.data)))
        })
        .map_err(Into::into)
}

/// Note that borrowchk doesn't like us to return an immutable, decoded tree which we would otherwise do. Chalk/polonius could allow that,
/// preventing a duplicate lookup.
fn resolve_tree_id_from_ref_target(mut id: ObjectId, repo: &Repository, buf: &mut Vec<u8>) -> anyhow::Result<ObjectId> {
    let mut cursor = repo.odb.find_existing(id, buf, &mut pack::cache::Never)?;
    loop {
        match cursor.kind {
            object::Kind::Tree => return Ok(id),
            object::Kind::Commit => {
                id = cursor.into_commit_iter().expect("commit").tree_id().expect("id");
                cursor = repo.odb.find_existing(id, buf, &mut pack::cache::Never)?;
            }
            object::Kind::Tag | object::Kind::Blob => {
                bail!(
                    "A ref ultimately points to a blob or tag {} but we need a tree, peeling takes care of tags",
                    id
                )
            }
        }
    }
}
