//! In-memory `Mds` implementation for fast unit tests.
//!
//! Gated by `#[cfg(any(test, feature = "mem-store"))]` per spec
//! §Trait boundary discipline. Phase 0 stub.

use crate::error::Result;
use crate::store::Mds;
use crate::types::*;

pub struct MemMds;

impl MemMds {
    pub fn new() -> Self {
        Self
    }
}

impl Default for MemMds {
    fn default() -> Self {
        Self::new()
    }
}

impl Mds for MemMds {
    fn create_set(&self, _set: &SetId) -> Result<()> {
        unimplemented!()
    }
    fn delete_set(&self, _set: &SetId) -> Result<DeleteReport> {
        unimplemented!()
    }
    fn list_sets(&self) -> Result<Vec<SetId>> {
        unimplemented!()
    }

    fn create_container(
        &self,
        _set: &SetId,
        _parent: Option<&ContainerId>,
        _name: &str,
        _attrs: ContainerAttrs,
    ) -> Result<ContainerId> {
        unimplemented!()
    }
    fn rename_container(
        &self,
        _set: &SetId,
        _id: &ContainerId,
        _new_parent: Option<&ContainerId>,
        _new_name: &str,
    ) -> Result<()> {
        unimplemented!()
    }
    fn delete_container(&self, _set: &SetId, _id: &ContainerId) -> Result<()> {
        unimplemented!()
    }
    fn list_containers(&self, _set: &SetId) -> Result<Vec<ContainerInfo>> {
        unimplemented!()
    }
    fn container_status(&self, _set: &SetId, _id: &ContainerId) -> Result<ContainerStatus> {
        unimplemented!()
    }

    fn put_blob(&self, _bytes: &[u8]) -> Result<BlobHash> {
        unimplemented!()
    }
    fn get_blob(&self, _hash: &BlobHash) -> Result<Vec<u8>> {
        unimplemented!()
    }
    fn blob_size(&self, _hash: &BlobHash) -> Result<u64> {
        unimplemented!()
    }
    fn blob_exists(&self, _hash: &BlobHash) -> Result<bool> {
        unimplemented!()
    }

    fn add_item(
        &self,
        _set: &SetId,
        _blob: &BlobHash,
        _memberships: &[Membership],
    ) -> Result<AddReport> {
        unimplemented!()
    }
    fn copy_item(
        &self,
        _set: &SetId,
        _id: &ItemId,
        _dest: &ContainerId,
        _flags: Flags,
    ) -> Result<CopyReport> {
        unimplemented!()
    }
    fn move_item(
        &self,
        _set: &SetId,
        _id: &ItemId,
        _src: &ContainerId,
        _dest: &ContainerId,
        _flags: Flags,
    ) -> Result<MoveReport> {
        unimplemented!()
    }
    fn remove_membership(
        &self,
        _set: &SetId,
        _id: &ItemId,
        _container: &ContainerId,
    ) -> Result<()> {
        unimplemented!()
    }
    fn store_flags(
        &self,
        _set: &SetId,
        _id: &ItemId,
        _container: &ContainerId,
        _flags: Flags,
    ) -> Result<ChangeToken> {
        unimplemented!()
    }
    fn store_membership_keywords(
        &self,
        _set: &SetId,
        _id: &ItemId,
        _container: &ContainerId,
        _flags: Flags,
        _tags: Tags,
    ) -> Result<ChangeToken> {
        unimplemented!()
    }
    fn store_item_keywords(
        &self,
        _set: &SetId,
        _id: &ItemId,
        _flags: Flags,
        _tags: Tags,
    ) -> Result<Vec<(ContainerId, ChangeToken)>> {
        unimplemented!()
    }
    fn item_memberships(
        &self,
        _set: &SetId,
        _id: &ItemId,
    ) -> Result<Vec<(ContainerId, Flags, Tags)>> {
        unimplemented!()
    }
    fn fetch_item(
        &self,
        _set: &SetId,
        _id: &ItemId,
        _container: &ContainerId,
    ) -> Result<ItemRecord> {
        unimplemented!()
    }
    fn fetch_item_meta(&self, _set: &SetId, _id: &ItemId) -> Result<ItemMeta> {
        unimplemented!()
    }
    fn find_items_by_blob_hash(&self, _set: &SetId, _blob_hash: &BlobHash) -> Result<Vec<ItemId>> {
        unimplemented!()
    }
    fn search_items(&self, _set: &SetId, _needle: &str) -> Result<Vec<ItemId>> {
        unimplemented!()
    }
    fn list_items(
        &self,
        _set: &SetId,
        _container: &ContainerId,
        _range: SeqRange,
    ) -> Result<Vec<ItemRecord>> {
        unimplemented!()
    }
    fn changes_since(
        &self,
        _set: &SetId,
        _container: &ContainerId,
        _since: ChangeToken,
    ) -> Result<Vec<Change>> {
        unimplemented!()
    }
    fn changes_since_set(
        &self,
        _set: &SetId,
        _since: SetChangeToken,
        _limit: usize,
    ) -> Result<(Vec<SetChange>, Option<SetChangeToken>)> {
        unimplemented!()
    }

    fn subscribe(
        &self,
        _set: &SetId,
        _container: &ContainerId,
    ) -> tokio::sync::broadcast::Receiver<ContainerEvent> {
        unimplemented!()
    }

    fn subscribe_existing(
        &self,
        _set: &SetId,
        _container: &ContainerId,
    ) -> Result<tokio::sync::broadcast::Receiver<ContainerEvent>> {
        unimplemented!()
    }

    fn rebuild_index(&self) -> Result<RebuildReport> {
        unimplemented!()
    }
    fn verify_blobs(&self, _scope: VerifyScope) -> Result<VerifyReport> {
        unimplemented!()
    }
    fn gc(&self, _dry_run: bool) -> Result<GcReport> {
        unimplemented!()
    }
    fn prune_changelog(
        &self,
        _set: &SetId,
        _stream: ChangelogStream,
        _keep_n: u64,
    ) -> Result<PruneReport> {
        unimplemented!()
    }
    fn changelog_floor(&self, _set: &SetId, _stream: ChangelogStream) -> Result<u64> {
        unimplemented!()
    }
    fn stats(&self) -> Result<MdsStats> {
        unimplemented!()
    }
    fn stats_per_set(&self) -> Result<Vec<PerSetStats>> {
        unimplemented!()
    }
    fn export_set(&self, _set: &SetId, _dest: &std::path::Path) -> Result<ExportReport> {
        unimplemented!()
    }
    fn import_set(&self, _tarball: &std::path::Path) -> Result<ImportReport> {
        unimplemented!()
    }
}
