use std::{
    collections::{HashMap, VecDeque},
    path::Path,
};

use anyhow::Context;
use memofs::Vfs;
use rbx_dom_weak::{types::Ref, InstanceBuilder, WeakDom};

use crate::{
    snapshot::{InstanceContext, InstanceMetadata, InstanceSnapshot},
    syncback::{hash_tree, FsSnapshot, SyncbackReturn, SyncbackSnapshot},
};

#[profiling::function]
pub fn snapshot_rbxm(
    context: &InstanceContext,
    vfs: &Vfs,
    path: &Path,
    name: &str,
) -> anyhow::Result<Option<InstanceSnapshot>> {
    let temp_tree = rbx_binary::from_reader(vfs.read(path)?.as_slice())
        .with_context(|| format!("Malformed rbxm file: {}", path.display()))?;

    let root_instance = temp_tree.root();
    let children = root_instance.children();

    if children.len() == 1 {
        let child = children[0];
        let snapshot = InstanceSnapshot::from_tree(temp_tree, child)
            .name(name)
            .metadata(
                InstanceMetadata::new()
                    .instigating_source(path)
                    .relevant_paths(vec![path.to_path_buf()])
                    .context(context),
            );

        Ok(Some(snapshot))
    } else {
        anyhow::bail!(
            "Rojo currently only supports model files with one top-level instance.\n\n \
             Check the model file at path {}",
            path.display()
        );
    }
}

pub fn syncback_rbxm<'new, 'old>(
    snapshot: &SyncbackSnapshot<'new, 'old>,
    file_name: &str,
) -> anyhow::Result<SyncbackReturn<'new, 'old>> {
    let inst = snapshot.new_inst();
    let path = snapshot.parent_path.join(file_name);
    // Long-term, we probably want to have some logic for if this contains a
    // script. That's a future endeavor though.

    let (dom, referent) = clone_and_filter(snapshot);
    if let Some(old_ref) = snapshot.old {
        log::trace!("Comparing two rbxmx trees to avoid extra writes");
        let new_hashes = hash_tree(&dom, referent);
        let old_hashes = hash_tree(snapshot.old_tree(), old_ref);

        if new_hashes.get(&referent) == old_hashes.get(&old_ref) {
            return Ok(SyncbackReturn {
                inst_snapshot: InstanceSnapshot::from_instance(dom.get_by_ref(referent).unwrap()),
                fs_snapshot: FsSnapshot::new(),
                children: Vec::new(),
                removed_children: Vec::new(),
            });
        }
    }

    let mut serialized = Vec::new();
    rbx_binary::to_writer(&mut serialized, &dom, &[referent])
        .context("failed to serialize new rbxm")?;

    Ok(SyncbackReturn {
        inst_snapshot: InstanceSnapshot::from_instance(inst),
        fs_snapshot: FsSnapshot::new().with_added_file(path, serialized),
        children: Vec::new(),
        removed_children: Vec::new(),
    })
}

fn clone_and_filter(snapshot: &SyncbackSnapshot) -> (WeakDom, Ref) {
    // We want to: filter an Instance's properties, insert it into a new DOM,
    // then do the same for its children. The challenge is matching parents up.

    let mut new_dom = WeakDom::new(InstanceBuilder::empty());
    // A map of old referents to their parent referent in the new DOM.
    let mut old_to_parent = HashMap::new();
    old_to_parent.insert(snapshot.new, new_dom.root_ref());

    let mut queue = VecDeque::new();
    queue.push_back(snapshot.new);

    // Note that this is back-in, front-out. This is important because
    // VecDeque::extend is the equivalent to using push_back.
    while let Some(referent) = queue.pop_front() {
        let inst = snapshot
            .new_tree()
            .get_by_ref(referent)
            .expect("all Instances should be in the new subtree");
        let builder = InstanceBuilder::new(&inst.class)
            .with_properties(
                snapshot
                    .get_filtered_properties(referent, None)
                    .unwrap()
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v.clone())),
            )
            .with_name(&inst.name);
        let parent = old_to_parent
            .get(&referent)
            .expect("children should come after parents");
        let new = new_dom.insert(*parent, builder);

        old_to_parent.extend(inst.children().iter().copied().map(|r| (r, new)));
        queue.extend(inst.children());
    }

    let new_ref = new_dom.root().children()[0];
    (new_dom, new_ref)
}

#[cfg(test)]
mod test {
    use super::*;

    use memofs::{InMemoryFs, VfsSnapshot};

    #[test]
    fn model_from_vfs() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/foo.rbxm",
            VfsSnapshot::file(include_bytes!("../../assets/test-folder.rbxm").to_vec()),
        )
        .unwrap();

        let mut vfs = Vfs::new(imfs);

        let instance_snapshot = snapshot_rbxm(
            &InstanceContext::default(),
            &mut vfs,
            Path::new("/foo.rbxm"),
            "foo",
        )
        .unwrap()
        .unwrap();

        assert_eq!(instance_snapshot.name, "foo");
        assert_eq!(instance_snapshot.class_name, "Folder");
        assert_eq!(instance_snapshot.children, Vec::new());

        // We intentionally don't assert on properties. rbx_binary does not
        // distinguish between String and BinaryString. The sample model was
        // created by Roblox Studio and has an empty BinaryString "Tags"
        // property that currently deserializes incorrectly.
        // See: https://github.com/Roblox/rbx-dom/issues/49
    }
}
