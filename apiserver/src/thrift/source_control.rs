// Copyright (c) 2019-present, Facebook, Inc.
// All Rights Reserved.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2 or any later version.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use faster_hex::hex_string;
use fbinit::FacebookInit;
use futures::stream::Stream;
use futures_preview::compat::Future01CompatExt;
use futures_util::try_join;
use mononoke_api::{
    ChangesetContext, ChangesetId, ChangesetSpecifier, CoreContext, FileType, HgChangesetId,
    Mononoke, MononokeError, PathEntry, RepoContext, TreeEntry, TreeId,
};
use scuba_ext::ScubaSampleBuilder;
use slog::Logger;
use source_control::server::SourceControlService;
use source_control::services::source_control_service as service;
use source_control::types as thrift;
use sshrelay::SshEnvVars;
use tracing::TraceContext;
use uuid::Uuid;

const MAX_LIMIT: i64 = 1000;

trait ScubaInfoProvider {
    fn scuba_reponame(&self) -> Option<String> {
        None
    }
    fn scuba_commit(&self) -> Option<String> {
        None
    }
    fn scuba_path(&self) -> Option<String> {
        None
    }
}

impl ScubaInfoProvider for thrift::RepoSpecifier {
    fn scuba_reponame(&self) -> Option<String> {
        Some(self.name.clone())
    }
}

impl ScubaInfoProvider for thrift::CommitSpecifier {
    fn scuba_reponame(&self) -> Option<String> {
        self.repo.scuba_reponame()
    }
    fn scuba_commit(&self) -> Option<String> {
        Some(self.id.to_string())
    }
}

impl ScubaInfoProvider for thrift::CommitPathSpecifier {
    fn scuba_reponame(&self) -> Option<String> {
        self.commit.scuba_reponame()
    }
    fn scuba_commit(&self) -> Option<String> {
        self.commit.scuba_commit()
    }
    fn scuba_path(&self) -> Option<String> {
        Some(self.path.clone())
    }
}

impl ScubaInfoProvider for thrift::TreeSpecifier {
    fn scuba_reponame(&self) -> Option<String> {
        match self {
            thrift::TreeSpecifier::by_commit_path(commit_path) => commit_path.scuba_reponame(),
            thrift::TreeSpecifier::by_id(tree_id) => tree_id.repo.scuba_reponame(),
            thrift::TreeSpecifier::UnknownField(_) => None,
        }
    }
    fn scuba_commit(&self) -> Option<String> {
        match self {
            thrift::TreeSpecifier::by_commit_path(commit_path) => commit_path.scuba_commit(),
            thrift::TreeSpecifier::by_id(_tree_id) => None,
            thrift::TreeSpecifier::UnknownField(_) => None,
        }
    }
    fn scuba_path(&self) -> Option<String> {
        match self {
            thrift::TreeSpecifier::by_commit_path(commit_path) => commit_path.scuba_path(),
            thrift::TreeSpecifier::by_id(_tree_id) => None,
            thrift::TreeSpecifier::UnknownField(_) => None,
        }
    }
}

#[derive(Clone)]
pub struct SourceControlServiceImpl {
    fb: FacebookInit,
    mononoke: Arc<Mononoke>,
    logger: Logger,
    scuba_builder: ScubaSampleBuilder,
}

impl SourceControlServiceImpl {
    pub fn new(
        fb: FacebookInit,
        mononoke: Arc<Mononoke>,
        logger: Logger,
        scuba_builder: ScubaSampleBuilder,
    ) -> Self {
        Self {
            fb,
            mononoke,
            logger,
            scuba_builder,
        }
    }

    fn create_ctx(&self, scuba_info_provider: Option<&dyn ScubaInfoProvider>) -> CoreContext {
        let mut scuba = self.scuba_builder.clone();
        scuba.add_common_server_data().add("type", "thrift");
        if let Some(scuba_info_provider) = scuba_info_provider {
            if let Some(reponame) = scuba_info_provider.scuba_reponame() {
                scuba.add("reponame", reponame);
            }
            if let Some(commit) = scuba_info_provider.scuba_commit() {
                scuba.add("commit", commit);
            }
            if let Some(path) = scuba_info_provider.scuba_path() {
                scuba.add("path", path);
            }
        }
        let uuid = Uuid::new_v4();
        scuba.add("session_uuid", uuid.to_string());
        CoreContext::new(
            self.fb,
            uuid,
            self.logger.clone(),
            scuba,
            None,
            TraceContext::default(),
            None,
            SshEnvVars::default(),
            None,
        )
    }
}

/// Generate a mapping for a commit's identity into the requested identity
/// schemes.
async fn map_commit_identity(
    changeset_ctx: &ChangesetContext,
    schemes: &BTreeSet<thrift::CommitIdentityScheme>,
) -> Result<BTreeMap<thrift::CommitIdentityScheme, thrift::CommitId>, MononokeError> {
    let mut ids = BTreeMap::new();
    ids.insert(
        thrift::CommitIdentityScheme::BONSAI,
        thrift::CommitId::bonsai(changeset_ctx.id().as_ref().into()),
    );
    if schemes.contains(&thrift::CommitIdentityScheme::HG) {
        if let Some(hg_cs_id) = changeset_ctx.hg_id().await? {
            ids.insert(
                thrift::CommitIdentityScheme::HG,
                thrift::CommitId::hg(hg_cs_id.as_ref().into()),
            );
        }
    }
    Ok(ids)
}

/// Generate mappings for multiple commits' identities into the requested
/// identity schemes.
async fn map_commit_identities(
    repo_ctx: &RepoContext,
    ids: Vec<ChangesetId>,
    schemes: &BTreeSet<thrift::CommitIdentityScheme>,
) -> Result<
    BTreeMap<ChangesetId, BTreeMap<thrift::CommitIdentityScheme, thrift::CommitId>>,
    MononokeError,
> {
    let mut result = BTreeMap::new();
    for id in ids.iter() {
        let mut idmap = BTreeMap::new();
        idmap.insert(
            thrift::CommitIdentityScheme::BONSAI,
            thrift::CommitId::bonsai(id.as_ref().into()),
        );
        result.insert(*id, idmap);
    }
    if schemes.contains(&thrift::CommitIdentityScheme::HG) {
        for (cs_id, hg_cs_id) in repo_ctx.changeset_hg_ids(ids).await?.into_iter() {
            result.entry(cs_id).or_insert_with(BTreeMap::new).insert(
                thrift::CommitIdentityScheme::HG,
                thrift::CommitId::hg(hg_cs_id.as_ref().into()),
            );
        }
    }
    Ok(result)
}

/// Trait to extend CommitId with useful functions.
trait CommitIdExt {
    fn scheme(&self) -> thrift::CommitIdentityScheme;
    fn to_string(&self) -> String;
}

impl CommitIdExt for thrift::CommitId {
    /// Returns the commit identity scheme of a commit ID.
    fn scheme(&self) -> thrift::CommitIdentityScheme {
        match self {
            thrift::CommitId::bonsai(_) => thrift::CommitIdentityScheme::BONSAI,
            thrift::CommitId::hg(_) => thrift::CommitIdentityScheme::HG,
            thrift::CommitId::git(_) => thrift::CommitIdentityScheme::GIT,
            thrift::CommitId::global_rev(_) => thrift::CommitIdentityScheme::GLOBAL_REV,
            thrift::CommitId::UnknownField(t) => (*t).into(),
        }
    }

    /// Convert a `thrift::CommitId` to a string for display. This would normally
    /// be implemented as `Display for thrift::CommitId`, but it is defined in
    /// the generated crate.
    fn to_string(&self) -> String {
        match self {
            thrift::CommitId::bonsai(id) => hex_string(&id).expect("hex_string should never fail"),
            thrift::CommitId::hg(id) => hex_string(&id).expect("hex_string should never fail"),
            thrift::CommitId::git(id) => hex_string(&id).expect("hex_string should never fail"),
            thrift::CommitId::global_rev(rev) => rev.to_string(),
            thrift::CommitId::UnknownField(t) => format!("unknown id type ({})", t),
        }
    }
}

trait FromRequest<T> {
    fn from_request(t: &T) -> Result<Self, thrift::RequestError>
    where
        Self: Sized;
}

impl FromRequest<thrift::CommitId> for ChangesetSpecifier {
    fn from_request(commit: &thrift::CommitId) -> Result<Self, thrift::RequestError> {
        match commit {
            thrift::CommitId::bonsai(id) => {
                let cs_id = ChangesetId::from_bytes(&id).map_err(|e| {
                    errors::invalid_request(format!(
                        "invalid commit id (scheme={} {}): {}",
                        commit.scheme(),
                        commit.to_string(),
                        e.to_string()
                    ))
                })?;
                Ok(ChangesetSpecifier::Bonsai(cs_id))
            }
            thrift::CommitId::hg(id) => {
                let hg_cs_id = HgChangesetId::from_bytes(&id).map_err(|e| {
                    errors::invalid_request(format!(
                        "invalid commit id (scheme={} {}): {}",
                        commit.scheme(),
                        commit.to_string(),
                        e.to_string()
                    ))
                })?;
                Ok(ChangesetSpecifier::Hg(hg_cs_id))
            }
            _ => Err(errors::invalid_request(format!(
                "unsupported commit identity scheme ({})",
                commit.scheme()
            ))),
        }
    }
}

impl FromRequest<Vec<u8>> for TreeId {
    fn from_request(tree_id: &Vec<u8>) -> Result<Self, thrift::RequestError> {
        TreeId::from_bytes(tree_id).map_err(|e| {
            errors::invalid_request(format!(
                "invalid tree id ({}): {}",
                hex_string(&tree_id).expect("hex_string should never fail"),
                e.to_string()
            ))
        })
    }
}

trait IntoResponse<T> {
    fn into_response(self) -> T;
}

impl IntoResponse<thrift::EntryType> for FileType {
    fn into_response(self) -> thrift::EntryType {
        match self {
            FileType::Regular => thrift::EntryType::FILE,
            FileType::Executable => thrift::EntryType::EXEC,
            FileType::Symlink => thrift::EntryType::LINK,
        }
    }
}

impl IntoResponse<thrift::TreeEntry> for (String, TreeEntry) {
    fn into_response(self) -> thrift::TreeEntry {
        let (name, entry) = self;
        let (type_, info) = match entry {
            TreeEntry::Directory(dir) => {
                let summary = dir.summary();
                let info = thrift::TreeInfo {
                    id: dir.id().as_ref().to_vec(),
                    simple_format_sha1: summary.simple_format_sha1.as_ref().to_vec(),
                    simple_format_sha256: summary.simple_format_sha256.as_ref().to_vec(),
                    child_files_count: summary.child_files_count as i64,
                    child_files_total_size: summary.child_files_total_size as i64,
                    child_dirs_count: summary.child_dirs_count as i64,
                    descendant_files_count: summary.descendant_files_count as i64,
                    descendant_files_total_size: summary.descendant_files_total_size as i64,
                };
                (thrift::EntryType::TREE, thrift::EntryInfo::tree(info))
            }
            TreeEntry::File(file) => {
                let info = thrift::FileInfo {
                    id: file.content_id().as_ref().to_vec(),
                    file_size: file.size() as i64,
                    content_sha1: file.content_sha1().as_ref().to_vec(),
                    content_sha256: file.content_sha256().as_ref().to_vec(),
                };
                (
                    file.file_type().into_response(),
                    thrift::EntryInfo::file(info),
                )
            }
        };
        thrift::TreeEntry { name, type_, info }
    }
}

mod errors {
    use super::thrift;
    use mononoke_api::{ChangesetSpecifier, TreeId};

    pub(super) fn invalid_request(reason: impl ToString) -> thrift::RequestError {
        thrift::RequestError {
            kind: thrift::RequestErrorKind::INVALID_REQUEST,
            reason: reason.to_string(),
        }
    }

    pub(super) fn repo_not_found(reponame: impl AsRef<str>) -> thrift::RequestError {
        thrift::RequestError {
            kind: thrift::RequestErrorKind::REPO_NOT_FOUND,
            reason: format!("repo not found ({})", reponame.as_ref()),
        }
    }

    pub(super) fn commit_not_found(commit: &ChangesetSpecifier) -> thrift::RequestError {
        thrift::RequestError {
            kind: thrift::RequestErrorKind::COMMIT_NOT_FOUND,
            reason: format!("commit not found ({})", commit),
        }
    }

    pub(super) fn tree_not_found(tree: &TreeId) -> thrift::RequestError {
        thrift::RequestError {
            kind: thrift::RequestErrorKind::TREE_NOT_FOUND,
            reason: format!("tree not found ({})", tree),
        }
    }
}

#[async_trait]
impl SourceControlService for SourceControlServiceImpl {
    async fn list_repos(
        &self,
        _params: thrift::ListReposParams,
    ) -> Result<Vec<thrift::Repo>, service::ListReposExn> {
        let _ctx = self.create_ctx(None);
        let rsp = self
            .mononoke
            .repo_names()
            .map(|repo_name| thrift::Repo {
                name: repo_name.to_string(),
            })
            .collect();
        Ok(rsp)
    }

    /// Resolve a bookmark to a changeset.
    ///
    /// Returns whether the bookmark exists, and the IDs of the changeset in
    /// the requested indentity schemes.
    async fn repo_resolve_bookmark(
        &self,
        repo: thrift::RepoSpecifier,
        params: thrift::RepoResolveBookmarkParams,
    ) -> Result<thrift::RepoResolveBookmarkResponse, service::RepoResolveBookmarkExn> {
        let ctx = self.create_ctx(Some(&repo));
        let repo = self
            .mononoke
            .repo(ctx, &repo.name)?
            .ok_or_else(|| errors::repo_not_found(&repo.name))?;
        match repo.resolve_bookmark(params.bookmark_name).await? {
            Some(cs) => {
                let ids = map_commit_identity(&cs, &params.identity_schemes).await?;
                Ok(thrift::RepoResolveBookmarkResponse {
                    exists: true,
                    ids: Some(ids),
                })
            }
            None => Ok(thrift::RepoResolveBookmarkResponse {
                exists: false,
                ids: None,
            }),
        }
    }

    /// List bookmarks.
    async fn repo_list_bookmarks(
        &self,
        repo: thrift::RepoSpecifier,
        params: thrift::RepoListBookmarksParams,
    ) -> Result<thrift::RepoListBookmarksResponse, service::RepoListBookmarksExn> {
        let ctx = self.create_ctx(Some(&repo));
        let limit = match params.limit {
            0 => None,
            limit @ 1...MAX_LIMIT => Some(limit as u64),
            limit => {
                return Err(errors::invalid_request(format!(
                    "limit ({}) out of range (0..{})",
                    limit, MAX_LIMIT,
                ))
                .into())
            }
        };
        let prefix = if !params.bookmark_prefix.is_empty() {
            Some(params.bookmark_prefix)
        } else {
            None
        };
        let repo = self
            .mononoke
            .repo(ctx, &repo.name)?
            .ok_or_else(|| errors::repo_not_found(&repo.name))?;
        let bookmarks = repo
            .list_bookmarks(params.include_scratch, prefix, limit)
            .collect()
            .compat()
            .await?;
        let ids = bookmarks.iter().map(|(_name, cs_id)| *cs_id).collect();
        let id_mapping = map_commit_identities(&repo, ids, &params.identity_schemes).await?;
        let bookmarks = bookmarks
            .into_iter()
            .map(|(name, cs_id)| match id_mapping.get(&cs_id) {
                Some(ids) => (name, ids.clone()),
                None => (name, BTreeMap::new()),
            })
            .collect();
        Ok(thrift::RepoListBookmarksResponse { bookmarks })
    }

    /// Look up commit.
    async fn commit_lookup(
        &self,
        commit: thrift::CommitSpecifier,
        params: thrift::CommitLookupParams,
    ) -> Result<thrift::CommitLookupResponse, service::CommitLookupExn> {
        let ctx = self.create_ctx(Some(&commit));
        let repo = self
            .mononoke
            .repo(ctx, &commit.repo.name)?
            .ok_or_else(|| errors::repo_not_found(&commit.repo.name))?;
        match repo
            .changeset(ChangesetSpecifier::from_request(&commit.id)?)
            .await?
        {
            Some(cs) => {
                let ids = map_commit_identity(&cs, &params.identity_schemes).await?;
                Ok(thrift::CommitLookupResponse {
                    exists: true,
                    ids: Some(ids),
                })
            }
            None => Ok(thrift::CommitLookupResponse {
                exists: false,
                ids: None,
            }),
        }
    }

    /// Get commit info.
    async fn commit_info(
        &self,
        commit: thrift::CommitSpecifier,
        params: thrift::CommitInfoParams,
    ) -> Result<thrift::CommitInfo, service::CommitInfoExn> {
        let ctx = self.create_ctx(Some(&commit));
        let repo = self
            .mononoke
            .repo(ctx, &commit.repo.name)?
            .ok_or_else(|| errors::repo_not_found(&commit.repo.name))?;

        let changeset_specifier = ChangesetSpecifier::from_request(&commit.id)?;
        match repo.changeset(changeset_specifier).await? {
            Some(changeset) => {
                async fn map_parent_identities(
                    repo: &RepoContext,
                    changeset: &ChangesetContext,
                    identity_schemes: &BTreeSet<thrift::CommitIdentityScheme>,
                ) -> Result<
                    Vec<BTreeMap<thrift::CommitIdentityScheme, thrift::CommitId>>,
                    MononokeError,
                > {
                    let parents = changeset.parents().await?;
                    let parent_id_mapping =
                        map_commit_identities(&repo, parents.clone(), identity_schemes).await?;
                    Ok(parents
                        .iter()
                        .map(|parent_id| {
                            parent_id_mapping
                                .get(parent_id)
                                .map(Clone::clone)
                                .unwrap_or_else(BTreeMap::new)
                        })
                        .collect())
                }

                let (ids, message, date, author, parents, extra) = try_join!(
                    map_commit_identity(&changeset, &params.identity_schemes),
                    changeset.message(),
                    changeset.author_date(),
                    changeset.author(),
                    map_parent_identities(&repo, &changeset, &params.identity_schemes),
                    changeset.extras(),
                )?;
                Ok(thrift::CommitInfo {
                    ids,
                    message,
                    date: date.timestamp(),
                    author,
                    parents,
                    extra: extra.into_iter().collect(),
                })
            }
            None => Err(errors::commit_not_found(&changeset_specifier).into()),
        }
    }

    /// Returns `true` if this commit is an ancestor of `other_commit`.
    async fn commit_is_ancestor_of(
        &self,
        commit: thrift::CommitSpecifier,
        params: thrift::CommitIsAncestorOfParams,
    ) -> Result<bool, service::CommitIsAncestorOfExn> {
        let ctx = self.create_ctx(Some(&commit));
        let repo = self
            .mononoke
            .repo(ctx, &commit.repo.name)?
            .ok_or_else(|| errors::repo_not_found(&commit.repo.name))?;
        let changeset_specifier = ChangesetSpecifier::from_request(&commit.id)?;
        let other_changeset_specifier = ChangesetSpecifier::from_request(&params.other_commit_id)?;
        let (changeset, other_changeset_id) = try_join!(
            repo.changeset(changeset_specifier),
            repo.resolve_specifier(other_changeset_specifier),
        )?;
        let changeset = changeset.ok_or_else(|| errors::commit_not_found(&changeset_specifier))?;
        let other_changeset_id = other_changeset_id
            .ok_or_else(|| errors::commit_not_found(&other_changeset_specifier))?;
        let is_ancestor_of = changeset.is_ancestor_of(other_changeset_id).await?;
        Ok(is_ancestor_of)
    }

    /// Returns information about the file or directory at a path in a commit.
    async fn commit_path_info(
        &self,
        commit_path: thrift::CommitPathSpecifier,
        _params: thrift::CommitPathInfoParams,
    ) -> Result<thrift::CommitPathInfoResponse, service::CommitPathInfoExn> {
        let ctx = self.create_ctx(Some(&commit_path));
        let repo = self
            .mononoke
            .repo(ctx, &commit_path.commit.repo.name)?
            .ok_or_else(|| errors::repo_not_found(&commit_path.commit.repo.name))?;
        let changeset_specifier = ChangesetSpecifier::from_request(&commit_path.commit.id)?;
        let changeset = repo
            .changeset(changeset_specifier)
            .await?
            .ok_or_else(|| errors::commit_not_found(&changeset_specifier))?;
        let path = changeset.path(&commit_path.path)?;

        let response = match path.entry().await? {
            PathEntry::NotPresent => thrift::CommitPathInfoResponse {
                exists: false,
                type_: None,
                info: None,
            },
            PathEntry::Tree(tree) => {
                let summary = tree.summary().await?;
                let tree_info = thrift::TreeInfo {
                    id: tree.id().as_ref().to_vec(),
                    simple_format_sha1: summary.simple_format_sha1.as_ref().to_vec(),
                    simple_format_sha256: summary.simple_format_sha256.as_ref().to_vec(),
                    child_files_count: summary.child_files_count as i64,
                    child_files_total_size: summary.child_files_total_size as i64,
                    child_dirs_count: summary.child_dirs_count as i64,
                    descendant_files_count: summary.descendant_files_count as i64,
                    descendant_files_total_size: summary.descendant_files_total_size as i64,
                };
                thrift::CommitPathInfoResponse {
                    exists: true,
                    type_: Some(thrift::EntryType::TREE),
                    info: Some(thrift::EntryInfo::tree(tree_info)),
                }
            }
            PathEntry::File(file, file_type) => {
                let metadata = file.metadata().await?;
                let file_info = thrift::FileInfo {
                    id: metadata.content_id.as_ref().to_vec(),
                    file_size: metadata.total_size as i64,
                    content_sha1: metadata.sha1.as_ref().to_vec(),
                    content_sha256: metadata.sha256.as_ref().to_vec(),
                };
                thrift::CommitPathInfoResponse {
                    exists: true,
                    type_: Some(file_type.into_response()),
                    info: Some(thrift::EntryInfo::file(file_info)),
                }
            }
        };
        Ok(response)
    }

    /// List the contents of a directory.
    async fn tree_list(
        &self,
        tree: thrift::TreeSpecifier,
        params: thrift::TreeListParams,
    ) -> Result<thrift::TreeListResponse, service::TreeListExn> {
        let ctx = self.create_ctx(Some(&tree));
        let tree = match tree {
            thrift::TreeSpecifier::by_commit_path(commit_path) => {
                let repo = self
                    .mononoke
                    .repo(ctx, &commit_path.commit.repo.name)?
                    .ok_or_else(|| errors::repo_not_found(&commit_path.commit.repo.name))?;
                let changeset_specifier = FromRequest::from_request(&commit_path.commit.id)?;
                let changeset = repo
                    .changeset(changeset_specifier)
                    .await?
                    .ok_or_else(|| errors::commit_not_found(&changeset_specifier))?;
                let path = changeset.path(&commit_path.path)?;
                path.tree().await?
            }
            thrift::TreeSpecifier::by_id(tree_id) => {
                let repo = self
                    .mononoke
                    .repo(ctx, &tree_id.repo.name)?
                    .ok_or_else(|| errors::repo_not_found(&tree_id.repo.name))?;
                let tree_id = TreeId::from_request(&tree_id.id)?;
                let tree = repo
                    .tree(tree_id)
                    .await?
                    .ok_or_else(|| errors::tree_not_found(&tree_id))?;
                Some(tree)
            }
            thrift::TreeSpecifier::UnknownField(id) => {
                return Err(errors::invalid_request(format!(
                    "tree specifier type not supported: {}",
                    id
                ))
                .into());
            }
        };
        if let Some(tree) = tree {
            let summary = tree.summary().await?;
            let entries = tree
                .list()
                .await?
                .skip(params.offset as usize)
                .take(params.limit as usize)
                .map(IntoResponse::into_response)
                .collect();
            let response = thrift::TreeListResponse {
                entries,
                count: (summary.child_files_count + summary.child_dirs_count) as i64,
            };
            Ok(response)
        } else {
            // Listing a path that is not a directory just returns an empty list.
            Ok(thrift::TreeListResponse {
                entries: Vec::new(),
                count: 0,
            })
        }
    }
}
