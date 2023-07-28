use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{bail, Context};
use memofs::{DirEntry, IoResultExt, Vfs};
use rbx_dom_weak::{types::Ref, Instance, WeakDom};

use crate::{
    snapshot::{
        get_best_syncback_middleware, get_best_syncback_middleware_must_not_serialize_children,
        FsSnapshot, InstanceContext, InstanceMetadata, InstanceSnapshot, InstigatingSource,
        MiddlewareContextAny, NewTuple, OldTuple, OptOldTuple, RojoTree, SnapshotMiddleware,
        SnapshotOverrideTrait, SyncbackContextX, SyncbackNode, SyncbackPlanner,
        PRIORITY_DIRECTORY_CHECK_FALLBACK, PRIORITY_MANY_READABLE, PRIORITY_MODEL_DIRECTORY,
    },
    snapshot_middleware::{get_middleware, get_middleware_inits},
};

use super::{
    get_middlewares, meta_file::MetadataFile, snapshot_from_vfs, util::reconcile_meta_file,
};

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct DirectoryMiddlewareContext {
    init_middleware: Option<&'static str>,
    init_context: Option<Arc<dyn MiddlewareContextAny>>,
    init_path: Option<PathBuf>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct DirectoryMiddleware;

impl SnapshotMiddleware for DirectoryMiddleware {
    fn middleware_id(&self) -> &'static str {
        "directory"
    }

    fn match_only_directories(&self) -> bool {
        true
    }

    fn default_globs(&self) -> &[&'static str] {
        &["**/"]
    }

    fn init_names(&self) -> &[&'static str] {
        &[]
    }

    fn snapshot(
        &self,
        context: &InstanceContext,
        vfs: &Vfs,
        path: &Path,
    ) -> anyhow::Result<Option<InstanceSnapshot>> {
        let mut snapshot = match snapshot_dir_no_meta(context, vfs, path)? {
            Some(snapshot) => snapshot,
            None => return Ok(None),
        };

        if let Some(mut meta) = dir_meta(vfs, path)? {
            meta.apply_all(&mut snapshot)?;
        }

        snapshot.metadata.middleware_id = Some(self.middleware_id());

        Ok(Some(snapshot))
    }

    fn syncback_serializes_children(&self) -> bool {
        true
    }

    fn syncback_priority(
        &self,
        _dom: &WeakDom,
        instance: &Instance,
        consider_descendants: bool,
    ) -> Option<i32> {
        if instance.class == "Folder" {
            if consider_descendants {
                Some(PRIORITY_MANY_READABLE)
            } else {
                Some(PRIORITY_DIRECTORY_CHECK_FALLBACK)
            }
        } else {
            Some(PRIORITY_MODEL_DIRECTORY)
        }
    }

    fn syncback_new_path(
        &self,
        parent_path: &Path,
        name: &str,
        _instance: &Instance,
    ) -> anyhow::Result<std::path::PathBuf> {
        Ok(parent_path.join(name))
    }

    fn syncback(&self, sync: &SyncbackContextX<'_, '_>) -> anyhow::Result<SyncbackNode> {
        if sync.old.is_some() {
            syncback_update(sync)
        } else {
            syncback_new(sync)
        }
    }
}

fn syncback_update(sync: &SyncbackContextX<'_, '_>) -> anyhow::Result<SyncbackNode> {
    let vfs = sync.vfs;
    let _diff = sync.diff;
    let path = sync.path;
    let old = sync.old.as_ref().unwrap();
    let new = sync.new;
    let metadata = sync.metadata;
    let overrides = &sync.overrides;

    let mut metadata = metadata.clone();

    let (old_dom, old_ref, dir_context) = old;
    let _old_inst = old_dom
        .get_instance(*old_ref)
        .with_context(|| "missing ref")?;

    let dir_context = match dir_context {
        Some(middleware_context) => Some(
            middleware_context
                .downcast_ref::<DirectoryMiddlewareContext>()
                .with_context(|| "middleware context was of wrong type")?,
        ),
        None => None,
    };

    let (new_dom, new_ref) = new;
    let new_inst = new_dom.get_by_ref(new_ref).with_context(|| "missing ref")?;

    metadata.middleware_id = Some("directory");
    metadata.instigating_source = Some(InstigatingSource::Path(path.to_path_buf()));
    metadata.relevant_paths = get_middleware_inits()
        .iter()
        .map(|(&init_name, _)| path.join(init_name))
        .collect();

    let mut fs_snapshot = FsSnapshot::new().with_dir(path);

    let mut init_children = None;
    let init_middleware;

    {
        let mut init_old: Option<(
            &RojoTree,
            Ref,
            Option<Arc<(dyn MiddlewareContextAny + 'static)>>,
        )> = None;
        let mut init_path: Option<Cow<Path>> = None;

        let old_init_middleware_pack = match dir_context {
            Some(middleware_context) => (
                middleware_context.init_middleware,
                middleware_context.init_path.as_ref(),
                middleware_context.init_context.clone(),
            ),
            None => (None, None, None),
        };

        match old_init_middleware_pack {
            (Some(old_init_middleware), Some(old_init_path), old_init_context) => {
                init_middleware = get_best_syncback_middleware_must_not_serialize_children(
                    new_dom,
                    new_inst,
                    false,
                    Some(old_init_middleware),
                );

                if let Some(_init_middleware) = init_middleware {
                    init_old = Some((*old_dom, *old_ref, old_init_context));
                    init_path = Some(Cow::Borrowed(&old_init_path));
                }
            }
            (Some(_init_middleware), None, _) => {
                bail!("Missing path for existing middleware")
            }
            (None, _, _) => {
                // TODO: deduplicate this
                init_middleware = get_best_syncback_middleware_must_not_serialize_children(
                    new_dom,
                    new_inst,
                    false,
                    old_init_middleware_pack.0,
                );

                if let Some(init_middleware) = init_middleware {
                    init_path = Some(Cow::Owned(
                        get_middleware(init_middleware)
                            .syncback_new_path(path, "init", new_inst)?,
                    ));
                }
            }
        }

        if let Some(init_middleware) = init_middleware {
            let init_path = init_path.unwrap();
            let init_node = get_middleware(init_middleware)
                .syncback(&SyncbackContextX {
                    path: &init_path,
                    old: init_old,
                    metadata: &InstanceMetadata::new().context(&metadata.context),
                    overrides: None,
                    ..sync.clone()
                })
                .with_context(|| "failed to create instance on filesystem")?;

            metadata.middleware_context = Some(Arc::new(DirectoryMiddlewareContext {
                init_middleware: init_node.instance_snapshot.metadata.middleware_id.clone(),
                init_context: init_node
                    .instance_snapshot
                    .metadata
                    .middleware_context
                    .clone(),
                init_path: init_node
                    .instance_snapshot
                    .metadata
                    .snapshot_source_path(true)
                    .map(|v| v.to_path_buf()),
            }));

            init_children = init_node.get_children;

            if let Some(sub_fs_snapshot) = &init_node.instance_snapshot.metadata.fs_snapshot {
                fs_snapshot = fs_snapshot.merge_with(sub_fs_snapshot);
            }
        } else {
            let meta = reconcile_meta_file(
                vfs,
                &path.join("init.meta.json"),
                new_inst,
                HashSet::new(),
                Some(overrides.known_class_or("Folder")),
                &metadata.context.syncback.property_filters_save,
            )?;

            fs_snapshot = fs_snapshot.with_file_contents_opt(&path.join("init.meta.json"), meta);
        }
    }

    metadata.fs_snapshot = Some(fs_snapshot);

    Ok(SyncbackNode::new(
        (old.opt_id(), new_ref),
        path,
        InstanceSnapshot::new()
            .class_name(&new_inst.class)
            .metadata(metadata)
            .name(&new_inst.name)
            .properties(new_inst.properties.clone()),
    )
    .with_children(move |sync| {
        let vfs = sync.vfs;
        let diff = sync.diff;
        let path = sync.path;
        let old = sync.old.as_ref().unwrap();
        let new = sync.new;
        let _metadata = sync.metadata;
        let overrides = &sync.overrides;

        let mut sync_children = Vec::new();
        let mut sync_removed = HashSet::new();

        if let Some(init_children) = init_children {
            let (init_children, init_removed) = init_children(sync)?;
            sync_children.extend(init_children);
            sync_removed.extend(init_removed);
        }

        if init_middleware != Some("project") {
            let (added, removed, changed, _unchanged) = diff
                .get_children(old.dom().inner(), new.dom(), old.id())
                .with_context(|| "diff failed")?;

            for child_ref in added {
                if let Some(plan) = SyncbackPlanner::from_new(path, new.dom(), child_ref)? {
                    sync_children.push(plan.syncback(vfs, diff, overrides.clone())?);
                }
            }

            for old_child_ref in changed {
                let new_child_ref = diff
                    .get_matching_new_ref(old_child_ref)
                    .with_context(|| "missing ref")?;
                if old
                    .dom()
                    .get_metadata(old_child_ref)
                    .unwrap()
                    .snapshot_source_path(false)
                    .is_none()
                {
                    log::trace!(
                        "skipping {} as directory child because it's sourced from a project",
                        new.dom().get_by_ref(new_child_ref).unwrap().name
                    );
                    continue;
                }

                if let Some(plan) = SyncbackPlanner::from_update(
                    old.dom(),
                    old_child_ref,
                    new.dom(),
                    new_child_ref,
                    None,
                    None,
                )? {
                    sync_children.push(plan.syncback(vfs, diff, overrides.clone())?);
                }
            }

            sync_removed.extend(removed);
        }

        Ok((sync_children, sync_removed))
    }))
}

fn syncback_new(sync: &SyncbackContextX<'_, '_>) -> anyhow::Result<SyncbackNode> {
    let vfs = sync.vfs;
    let path = sync.path;
    let new = sync.new;
    let metadata = sync.metadata;
    let overrides = &sync.overrides;

    let mut metadata = metadata.clone();

    let (new_dom, new_ref) = new;
    let new_inst = new_dom.get_by_ref(new_ref).with_context(|| "missing ref")?;

    metadata.middleware_id = Some("directory");
    metadata.instigating_source = Some(InstigatingSource::Path(path.to_path_buf()));
    metadata.relevant_paths = get_middleware_inits()
        .iter()
        .map(|(&init_name, _)| path.join(init_name))
        .collect();

    let mut fs_snapshot = FsSnapshot::new().with_dir(path);
    let mut init_children = None;

    let init_middleware =
        get_best_syncback_middleware_must_not_serialize_children(new_dom, new_inst, false, None);

    if let Some(init_middleware) = init_middleware {
        let init_file_path =
            get_middleware(init_middleware).syncback_new_path(path, "init", new_inst)?;

        let init_sync = SyncbackContextX {
            path: &init_file_path,
            metadata: &InstanceMetadata::new().context(&metadata.context),
            overrides: None,
            ..sync.clone()
        };

        let init_node = get_middlewares()[init_middleware]
            .syncback(&init_sync)
            .with_context(|| "failed to create instance on filesystem")?;

        metadata.middleware_context = Some(Arc::new(DirectoryMiddlewareContext {
            init_middleware: init_node.instance_snapshot.metadata.middleware_id.clone(),
            init_context: init_node
                .instance_snapshot
                .metadata
                .middleware_context
                .clone(),
            init_path: init_node
                .instance_snapshot
                .metadata
                .snapshot_source_path(true)
                .map(|v| v.to_path_buf()),
        }));

        init_children = match init_node.get_children {
            Some(get_children) => Some(get_children(&init_sync)?),
            None => None,
        };

        if let Some(sub_fs_snapshot) = &init_node.instance_snapshot.metadata.fs_snapshot {
            fs_snapshot = fs_snapshot.merge_with(sub_fs_snapshot);
        }
    } else {
        let meta = reconcile_meta_file(
            vfs,
            &path.join("init.meta.json"),
            new_inst,
            HashSet::new(),
            Some(overrides.known_class_or("Folder")),
            &metadata.context.syncback.property_filters_save,
        )?;

        fs_snapshot = fs_snapshot.with_file_contents_opt(&path.join("init.meta.json"), meta);
    }

    metadata.fs_snapshot = Some(fs_snapshot);

    Ok(SyncbackNode::new(
        new_ref,
        path,
        InstanceSnapshot::new()
            .class_name(&new_inst.class)
            .metadata(metadata)
            .name(&new_inst.name)
            .properties(new_inst.properties.clone()),
    )
    .with_children(move |sync| {
        let path = sync.path;
        let new = sync.new;
        let metadata = sync.metadata;

        let (new_dom, new_ref) = new;
        let new_inst = new_dom.get_by_ref(new_ref).with_context(|| "missing ref")?;

        let mut sync_children = Vec::new();
        let mut sync_removed = HashSet::new();

        if let Some((init_children, init_removed)) = init_children {
            sync_children.extend(init_children);
            sync_removed.extend(init_removed);
        }

        if init_middleware != Some("project") {
            for child_ref in new_inst.children() {
                let child_inst = new_dom
                    .get_by_ref(*child_ref)
                    .with_context(|| "missing ref")?;
                let child_middleware =
                    get_best_syncback_middleware(new_dom, child_inst, true, None);

                if let Some(child_middleware) = child_middleware {
                    let child_path = get_middleware(child_middleware).syncback_new_path(
                        path,
                        &child_inst.name,
                        child_inst,
                    )?;

                    let child_snapshot = get_middlewares()[child_middleware]
                        .syncback(&SyncbackContextX {
                            path: &child_path,
                            old: None,
                            new: (new_dom, *child_ref),
                            metadata: &InstanceMetadata::new().context(&metadata.context),
                            overrides: None,
                            ..sync.clone()
                        })
                        .with_context(|| "failed to create instance on filesystem")?;

                    sync_children.push(child_snapshot);
                } // TODO: warn on skipping (or fail early?)
            }
        }

        Ok((sync_children, sync_removed))
    }))
}

/// Retrieves the meta file that should be applied for this directory, if it
/// exists.
pub fn dir_meta(vfs: &Vfs, path: &Path) -> anyhow::Result<Option<MetadataFile>> {
    let meta_path = path.join("init.meta.json");

    if let Some(meta_contents) = vfs.read(&meta_path).with_not_found()? {
        let metadata = MetadataFile::from_slice(&meta_contents, meta_path)?;
        Ok(Some(metadata))
    } else {
        Ok(None)
    }
}

/// Snapshot a directory without applying meta files; useful for if the
/// directory's ClassName will change before metadata should be applied. For
/// example, this can happen if the directory contains an `init.client.lua`
/// file.
pub fn snapshot_dir_no_meta(
    context: &InstanceContext,
    vfs: &Vfs,
    path: &Path,
) -> anyhow::Result<Option<InstanceSnapshot>> {
    let middlewares = get_middlewares();

    let passes_filter_rules = |child: &DirEntry| {
        context
            .path_ignore_rules
            .iter()
            .all(|rule| rule.passes(child.path()))
    };

    let mut init_names: HashMap<&str, &str> = HashMap::new();
    for (_, middleware) in middlewares.iter() {
        for &name in middleware.init_names() {
            init_names.insert(name, middleware.middleware_id());
        }
    }

    let mut snapshot_children = Vec::new();

    let mut snapshot_parent = None;
    let mut skip_default_children = false;

    for (&middleware_id, middleware) in middlewares.iter() {
        for &name in middleware.init_names() {
            let init_path = path.join(name);
            let metadata = vfs
                .metadata(&init_path)
                .map(Some)
                .or_else(|e| match e.kind() {
                    std::io::ErrorKind::NotFound => Ok(None),
                    _ => Err(e),
                })?;
            if let Some(_metadata) = metadata {
                if let Some(init_snapshot) = middleware.snapshot(context, vfs, &init_path)? {
                    if middleware_id == "project" {
                        skip_default_children = true;
                        snapshot_children = init_snapshot.children.clone(); // TODO: don't do this
                    }
                    snapshot_parent = Some(init_snapshot);
                    break;
                }
            }
        }
    }

    if !skip_default_children {
        for entry in vfs.read_dir(path)? {
            let entry = entry?;

            if !passes_filter_rules(&entry) {
                continue;
            }

            let init_middleware_id =
                init_names.get(entry.path().file_name().unwrap().to_string_lossy().as_ref());
            if let Some(&_init_middleware_id) = init_middleware_id {
                continue;
            }

            if let Some(child_snapshot) = snapshot_from_vfs(context, vfs, entry.path())? {
                snapshot_children.push(child_snapshot);
            }
        }
    }

    let instance_name = path
        .file_name()
        .expect("Could not extract file name")
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("File name was not valid UTF-8: {}", path.display()))?
        .to_string();

    let meta_path = path.join("init.meta.json");

    let mut relevant_paths = vec![path.to_path_buf(), meta_path.clone()];

    for (_, middleware) in middlewares {
        for &name in middleware.init_names() {
            relevant_paths.push(path.join(name));
        }
    }

    let snapshot = match snapshot_parent {
        None => InstanceSnapshot::new()
            .name(instance_name)
            .class_name("Folder")
            .children(snapshot_children)
            .metadata(
                InstanceMetadata::new()
                    .instigating_source(path)
                    .relevant_paths(relevant_paths)
                    .context(context),
            ),
        Some(init_snapshot) => {
            let mut syncback_context = None;
            if let Some(init_middleware_id) = init_snapshot.metadata.middleware_id {
                let init_path = match &init_snapshot.metadata.instigating_source {
                    Some(InstigatingSource::Path(init_path)) => init_path.clone(),
                    _ => bail!("Invalid InstigatingSource from init snapshot"),
                };

                syncback_context = Some(Arc::new(DirectoryMiddlewareContext {
                    init_middleware: Some(init_middleware_id),
                    init_context: init_snapshot.metadata.middleware_context.clone(),
                    init_path: Some(init_path),
                }) as Arc<dyn MiddlewareContextAny>);
            }

            init_snapshot
                .name(instance_name)
                .children(snapshot_children)
                .metadata(
                    InstanceMetadata::new()
                        .instigating_source(path)
                        .relevant_paths(relevant_paths)
                        .middleware_context(syncback_context)
                        .context(&context),
                )
        }
    };

    Ok(Some(snapshot))
}

#[cfg(test)]
mod test {
    use super::*;

    use maplit::hashmap;
    use memofs::{InMemoryFs, VfsSnapshot};

    #[test]
    fn empty_folder() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot("/foo", VfsSnapshot::empty_dir())
            .unwrap();

        let mut vfs = Vfs::new(imfs);

        let instance_snapshot = DirectoryMiddleware
            .snapshot(&InstanceContext::default(), &mut vfs, Path::new("/foo"))
            .unwrap()
            .unwrap();

        insta::assert_yaml_snapshot!(instance_snapshot);
    }

    #[test]
    fn folder_in_folder() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/foo",
            VfsSnapshot::dir(hashmap! {
                "Child" => VfsSnapshot::empty_dir(),
            }),
        )
        .unwrap();

        let mut vfs = Vfs::new(imfs);

        let instance_snapshot = DirectoryMiddleware
            .snapshot(&InstanceContext::default(), &mut vfs, Path::new("/foo"))
            .unwrap()
            .unwrap();

        insta::assert_yaml_snapshot!(instance_snapshot);
    }
}
