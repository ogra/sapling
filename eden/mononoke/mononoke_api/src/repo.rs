/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::fmt;
use std::{
    collections::HashMap,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{format_err, Error};
use blobrepo::BlobRepo;
use blobrepo_factory::{BlobrepoBuilder, ReadOnlyStorage};
use blobrepo_hg::BlobRepoHg;
use blobstore::Loadable;
use blobstore_factory::make_metadata_sql_factory;
pub use bookmarks::Freshness as BookmarkFreshness;
use bookmarks::{BookmarkKind, BookmarkName, BookmarkPagination, BookmarkPrefix, Bookmarks};
use changeset_info::ChangesetInfo;
use context::CoreContext;
use cross_repo_sync::{
    types::Target, CandidateSelectionHint, CommitSyncContext, CommitSyncRepos, CommitSyncer,
};
use derived_data::BonsaiDerivable;
use fbinit::FacebookInit;
use filestore::{Alias, FetchKey};
use futures::compat::Stream01CompatExt;
use futures::stream::{self, Stream, StreamExt, TryStreamExt};
use futures::try_join;
use futures_watchdog::WatchdogExt;
use hook_manager_factory::make_hook_manager;
use hooks::HookManager;
use itertools::Itertools;
use live_commit_sync_config::TestLiveCommitSyncConfig;
use live_commit_sync_config::{CfgrLiveCommitSyncConfig, LiveCommitSyncConfig};
use mercurial_derived_data::MappedHgChangesetId;
use mercurial_types::Globalrev;
use metaconfig_types::{CommonConfig, RepoConfig};
use metaconfig_types::{
    HookManagerParams, InfinitepushNamespace, InfinitepushParams, SourceControlServiceParams,
};
use mononoke_types::{
    hash::{GitSha1, Sha1, Sha256},
    Generation, RepositoryId, Svnrev,
};
use mutable_counters::SqlMutableCounters;
use permission_checker::{ArcPermissionChecker, PermissionCheckerBuilder};
use reachabilityindex::LeastCommonAncestorsHint;
use regex::Regex;
use repo_read_write_status::{RepoReadWriteFetcher, SqlRepoReadWriteStatus};
use revset::AncestorsNodeStream;
use segmented_changelog::{CloneData, Location, SegmentedChangelog, StreamCloneData};
use skiplist::{fetch_skiplist_index, SkiplistIndex};
use slog::{debug, error, o, Logger};
use sql_construct::facebook::FbSqlConstruct;
use sql_construct::SqlConstruct;
use sql_construct::SqlConstructFromMetadataDatabaseConfig;
use sql_ext::facebook::MysqlOptions;
use stats::prelude::*;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use synced_commit_mapping::{SqlSyncedCommitMapping, SyncedCommitMapping};
use warm_bookmarks_cache::{BookmarkUpdateDelay, WarmBookmarksCache, WarmBookmarksCacheBuilder};

use crate::changeset::ChangesetContext;
use crate::errors::MononokeError;
use crate::file::{FileContext, FileId};
use crate::repo_write::{PermissionsModel, RepoWriteContext};
use crate::specifiers::{
    ChangesetId, ChangesetPrefixSpecifier, ChangesetSpecifier, ChangesetSpecifierPrefixResolution,
    HgChangesetId,
};
use crate::tree::{TreeContext, TreeId};
use crate::xrepo::CandidateSelectionHintArgs;
use crate::{MononokeEnvironment, WarmBookmarksCacheDerivedData};

define_stats! {
    prefix = "mononoke.api";
    staleness: dynamic_singleton_counter(
        "staleness.secs.{}.{}",
        (repoid: ::mononoke_types::RepositoryId, bookmark: String)
    ),
    missing_from_cache: dynamic_singleton_counter(
        "missing_from_cache.{}.{}",
        (repoid: ::mononoke_types::RepositoryId, bookmark: String)
    ),
    missing_from_repo: dynamic_singleton_counter(
        "missing_from_repo.{}.{}",
        (repoid: ::mononoke_types::RepositoryId, bookmark: String)
    ),
}

pub struct Repo {
    pub(crate) name: String,
    pub(crate) blob_repo: BlobRepo,
    pub(crate) skiplist_index: Arc<SkiplistIndex>,
    pub(crate) warm_bookmarks_cache: Arc<WarmBookmarksCache>,
    // This doesn't really belong here, but until we have production mappings, we can't do a better job
    pub(crate) synced_commit_mapping: Arc<dyn SyncedCommitMapping>,
    pub(crate) config: RepoConfig,
    pub(crate) repo_permission_checker: ArcPermissionChecker,
    pub(crate) service_permission_checker: ArcPermissionChecker,
    pub(crate) live_commit_sync_config: Arc<dyn LiveCommitSyncConfig>,
    pub(crate) hook_manager: Arc<HookManager>,
    pub(crate) readonly_fetcher: RepoReadWriteFetcher,
}

#[derive(Clone)]
pub struct RepoContext {
    ctx: CoreContext,
    repo: Arc<Repo>,
}

impl fmt::Debug for RepoContext {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "RepoContext(repo={:?})", self.name())
    }
}

pub async fn open_synced_commit_mapping(
    fb: FacebookInit,
    config: RepoConfig,
    mysql_options: &MysqlOptions,
    readonly_storage: ReadOnlyStorage,
    logger: &Logger,
) -> Result<Arc<SqlSyncedCommitMapping>, Error> {
    let sql_factory = make_metadata_sql_factory(
        fb,
        config.storage_config.metadata,
        mysql_options.clone(),
        readonly_storage,
        logger,
    )
    .await?;

    Ok(Arc::new(
        sql_factory.open::<SqlSyncedCommitMapping>().await?,
    ))
}

impl Repo {
    pub async fn new(
        env: &MononokeEnvironment<'_>,
        name: String,
        config: RepoConfig,
        common_config: CommonConfig,
    ) -> Result<Self, Error> {
        let logger = env.logger.new(o!("repo" => name.clone()));
        let disabled_hooks = env.disabled_hooks.get(&name).cloned().unwrap_or_default();

        let skiplist_index_blobstore_key = config.skiplist_index_blobstore_key.clone();

        let synced_commit_mapping = open_synced_commit_mapping(
            env.fb,
            config.clone(),
            &env.mysql_options,
            env.readonly_storage,
            &logger,
        )
        .await?;

        let live_commit_sync_config: Arc<dyn LiveCommitSyncConfig> =
            Arc::new(CfgrLiveCommitSyncConfig::new(&logger, &env.config_store)?);

        let builder = BlobrepoBuilder::new(
            env.fb,
            name.clone(),
            &config,
            &env.mysql_options,
            env.caching,
            common_config.censored_scuba_params,
            env.readonly_storage,
            env.blobstore_options.clone(),
            &logger,
            &env.config_store,
        );
        let blob_repo = builder.build().watched(&logger).await?;

        let ctx = CoreContext::new_with_logger(env.fb, logger.clone());

        let repo_permission_checker = async {
            let checker = match &config.hipster_acl {
                Some(acl) => PermissionCheckerBuilder::acl_for_repo(env.fb, acl).await?,
                None => PermissionCheckerBuilder::always_allow(),
            };
            Ok(ArcPermissionChecker::from(checker))
        };

        let service_permission_checker = async {
            let checker = match &config.source_control_service.service_write_hipster_acl {
                Some(acl) => PermissionCheckerBuilder::acl_for_tier(env.fb, acl).await?,
                None => PermissionCheckerBuilder::always_allow(),
            };
            Ok(ArcPermissionChecker::from(checker))
        };

        let hook_manager = {
            let ctx = &ctx;
            let blob_repo = &blob_repo;
            let config = config.clone();
            let name = name.as_str();
            async move {
                let hook_manager =
                    make_hook_manager(ctx, blob_repo, config, name, &disabled_hooks).await?;
                Ok(Arc::new(hook_manager))
            }
        };

        let blobstore = blob_repo.get_blobstore().boxed();
        let skiplist_index = fetch_skiplist_index(&ctx, &skiplist_index_blobstore_key, &blobstore);

        let mut warm_bookmarks_cache_builder = WarmBookmarksCacheBuilder::new(&ctx, &blob_repo);
        match env.warm_bookmarks_cache_derived_data {
            WarmBookmarksCacheDerivedData::HgOnly => {
                warm_bookmarks_cache_builder
                    .add_derived_data_warmers(Some(MappedHgChangesetId::NAME))?;
            }
            WarmBookmarksCacheDerivedData::AllKinds => {
                warm_bookmarks_cache_builder.add_all_derived_data_warmers()?;
            }
            WarmBookmarksCacheDerivedData::None => {}
        }

        if config.warm_bookmark_cache_check_blobimport {
            let db_config = &config.storage_config.metadata;
            let mutable_counters = SqlMutableCounters::with_metadata_database_config(
                env.fb,
                db_config,
                &env.mysql_options,
                env.readonly_storage.0,
            )
            .await?;
            warm_bookmarks_cache_builder.add_blobimport_warmer(Arc::new(mutable_counters));
        }
        let warm_bookmarks_cache =
            warm_bookmarks_cache_builder.build(env.warm_bookmarks_cache_delay);

        let sql_read_write_status = async {
            if let Some(addr) = &config.write_lock_db_address {
                let r = SqlRepoReadWriteStatus::with_xdb(
                    env.fb,
                    addr.clone(),
                    &env.mysql_options,
                    env.readonly_storage.0,
                )
                .await?;
                Ok(Some(r))
            } else {
                Ok(None)
            }
        };

        let (
            repo_permission_checker,
            service_permission_checker,
            skiplist_index,
            warm_bookmarks_cache,
            hook_manager,
            sql_read_write_status,
        ) = try_join!(
            repo_permission_checker.watched(&logger),
            service_permission_checker.watched(&logger),
            skiplist_index.watched(&logger),
            warm_bookmarks_cache.watched(&logger),
            hook_manager.watched(&logger),
            sql_read_write_status.watched(&logger),
        )?;

        let readonly_fetcher = RepoReadWriteFetcher::new(
            sql_read_write_status,
            config.readonly.clone(),
            config.hgsql_name.clone(),
        );

        Ok(Self {
            name,
            blob_repo,
            skiplist_index,
            warm_bookmarks_cache: Arc::new(warm_bookmarks_cache),
            synced_commit_mapping,
            config,
            repo_permission_checker,
            service_permission_checker,
            live_commit_sync_config,
            hook_manager,
            readonly_fetcher,
        })
    }

    /// Construct a Repo from a test BlobRepo
    pub async fn new_test(ctx: CoreContext, blob_repo: BlobRepo) -> Result<Self, Error> {
        Self::new_test_common(
            ctx,
            blob_repo,
            None,
            Arc::new(SqlSyncedCommitMapping::with_sqlite_in_memory()?),
        )
        .await
    }

    /// Construct a Repo from a test BlobRepo and commit_sync_config
    pub async fn new_test_xrepo(
        ctx: CoreContext,
        blob_repo: BlobRepo,
        live_commit_sync_config: Arc<dyn LiveCommitSyncConfig>,
        synced_commit_mapping: Arc<dyn SyncedCommitMapping>,
    ) -> Result<Self, Error> {
        Self::new_test_common(
            ctx,
            blob_repo,
            Some(live_commit_sync_config),
            synced_commit_mapping,
        )
        .await
    }

    /// Construct a Repo from a test BlobRepo and commit_sync_config
    async fn new_test_common(
        ctx: CoreContext,
        blob_repo: BlobRepo,
        live_commit_sync_config: Option<Arc<dyn LiveCommitSyncConfig>>,
        synced_commit_mapping: Arc<dyn SyncedCommitMapping>,
    ) -> Result<Self, Error> {
        let init_commit_sync_config = match live_commit_sync_config.as_ref() {
            None => None,
            Some(lcsc) => Some(
                lcsc.get_current_commit_sync_config(&ctx, blob_repo.get_repoid())
                    .await?,
            ),
        };

        let config = RepoConfig {
            commit_sync_config: init_commit_sync_config,
            infinitepush: InfinitepushParams {
                namespace: Some(InfinitepushNamespace::new(
                    Regex::new("scratch/.+").unwrap(),
                )),
                ..Default::default()
            },
            source_control_service: SourceControlServiceParams {
                permit_writes: true,
                ..Default::default()
            },
            hook_manager_params: Some(HookManagerParams {
                disable_acl_checker: true,
                ..Default::default()
            }),
            ..Default::default()
        };

        let mut warm_bookmarks_cache_builder = WarmBookmarksCacheBuilder::new(&ctx, &blob_repo);
        warm_bookmarks_cache_builder.add_all_derived_data_warmers()?;
        // We are constructing a test repo, so ensure the warm bookmark cache
        // is fully warmed, so that tests see up-to-date bookmarks.
        warm_bookmarks_cache_builder.wait_until_warmed();
        let warm_bookmarks_cache = warm_bookmarks_cache_builder
            .build(BookmarkUpdateDelay::Allow)
            .await?;

        let live_commit_sync_config: Arc<dyn LiveCommitSyncConfig> = match live_commit_sync_config {
            Some(live_commit_sync_config) => live_commit_sync_config,
            None => Arc::new(TestLiveCommitSyncConfig::new_empty()),
        };

        let hook_manager = Arc::new(
            make_hook_manager(&ctx, &blob_repo, config.clone(), "test", &HashSet::new()).await?,
        );

        let readonly_fetcher =
            RepoReadWriteFetcher::new(None, config.readonly.clone(), config.hgsql_name.clone());

        Ok(Self {
            name: String::from("test"),
            blob_repo,
            skiplist_index: Arc::new(SkiplistIndex::new()),
            warm_bookmarks_cache: Arc::new(warm_bookmarks_cache),
            synced_commit_mapping,
            config,
            repo_permission_checker: ArcPermissionChecker::from(
                PermissionCheckerBuilder::always_allow(),
            ),
            service_permission_checker: ArcPermissionChecker::from(
                PermissionCheckerBuilder::always_allow(),
            ),
            live_commit_sync_config,
            hook_manager,
            readonly_fetcher,
        })
    }

    /// The name of the underlying repo.
    pub fn name(&self) -> &String {
        &self.name
    }

    /// The internal id of the repo. Used for comparing the repo objects with each other.
    pub fn repoid(&self) -> RepositoryId {
        self.blob_repo.get_repoid()
    }

    /// The underlying `BlobRepo`.
    pub fn blob_repo(&self) -> &BlobRepo {
        &self.blob_repo
    }

    /// `LiveCommitSyncConfig` instance to query current state of sync configs.
    pub fn live_commit_sync_config(&self) -> Arc<dyn LiveCommitSyncConfig> {
        self.live_commit_sync_config.clone()
    }

    /// The skiplist index for the referenced repository.
    pub fn skiplist_index(&self) -> &Arc<SkiplistIndex> {
        &self.skiplist_index
    }

    /// The commit sync mapping for the referenced repository
    pub fn synced_commit_mapping(&self) -> &Arc<dyn SyncedCommitMapping> {
        &self.synced_commit_mapping
    }

    /// The warm bookmarks cache for the referenced repository.
    pub fn warm_bookmarks_cache(&self) -> &Arc<WarmBookmarksCache> {
        &self.warm_bookmarks_cache
    }

    /// The hook manager for the referenced repository.
    pub fn hook_manager(&self) -> &Arc<HookManager> {
        &self.hook_manager
    }

    /// The Read/Write or Read-Only status fetcher.
    pub fn readonly_fetcher(&self) -> &RepoReadWriteFetcher {
        &self.readonly_fetcher
    }

    /// The configuration for the referenced repository.
    pub fn config(&self) -> &RepoConfig {
        &self.config
    }

    /// A mutable reference to this repository's config. This is typically only useful for tests.
    pub fn config_mut(&mut self) -> &mut RepoConfig {
        &mut self.config
    }

    pub async fn report_monitoring_stats(&self, ctx: &CoreContext) -> Result<(), MononokeError> {
        match self.config.source_control_service_monitoring.as_ref() {
            None => {}
            Some(monitoring_config) => {
                for bookmark in monitoring_config.bookmarks_to_report_age.iter() {
                    self.report_bookmark_age_difference(ctx, &bookmark).await?;
                }
            }
        }

        Ok(())
    }

    fn report_bookmark_missing_from_cache(&self, ctx: &CoreContext, bookmark: &BookmarkName) {
        error!(
            ctx.logger(),
            "Monitored bookmark does not exist in the cache: {}, repo: {}",
            bookmark,
            self.blob_repo.name()
        );

        STATS::missing_from_cache.set_value(
            ctx.fb,
            1,
            (self.blob_repo.get_repoid(), bookmark.to_string()),
        );
    }

    fn report_bookmark_missing_from_repo(&self, ctx: &CoreContext, bookmark: &BookmarkName) {
        error!(
            ctx.logger(),
            "Monitored bookmark does not exist in the repo: {}", bookmark
        );

        STATS::missing_from_repo.set_value(
            ctx.fb,
            1,
            (self.blob_repo.get_repoid(), bookmark.to_string()),
        );
    }

    fn report_bookmark_staleness(
        &self,
        ctx: &CoreContext,
        bookmark: &BookmarkName,
        staleness: i64,
    ) {
        debug!(
            ctx.logger(),
            "Reporting staleness of {} in repo {} to be {}s",
            bookmark,
            self.blob_repo.get_repoid(),
            staleness
        );

        STATS::staleness.set_value(
            ctx.fb,
            staleness,
            (self.blob_repo.get_repoid(), bookmark.to_string()),
        );
    }

    async fn report_bookmark_age_difference(
        &self,
        ctx: &CoreContext,
        bookmark: &BookmarkName,
    ) -> Result<(), MononokeError> {
        let repo = &self.blob_repo;

        let maybe_bcs_id_from_service = self.warm_bookmarks_cache.get(bookmark);
        let maybe_bcs_id_from_blobrepo = repo.get_bonsai_bookmark(ctx.clone(), &bookmark).await?;

        if maybe_bcs_id_from_blobrepo.is_none() {
            self.report_bookmark_missing_from_repo(ctx, bookmark);
        }

        if maybe_bcs_id_from_service.is_none() {
            self.report_bookmark_missing_from_cache(ctx, bookmark);
        }

        if let (Some(service_bcs_id), Some(blobrepo_bcs_id)) =
            (maybe_bcs_id_from_service, maybe_bcs_id_from_blobrepo)
        {
            // We report the difference between current time (i.e. SystemTime::now())
            // and timestamp of the first child of bookmark value from cache (see graph below)
            //
            //       O <- bookmark value from blobrepo
            //       |
            //      ...
            //       |
            //       O <- first child of bookmark value from cache.
            //       |
            //       O <- bookmark value from cache, it's outdated
            //
            // This way of reporting shows for how long the oldest commit not in cache hasn't been
            // imported, and it should work correctly both for high and low commit rates.
            debug!(
                ctx.logger(),
                "Reporting bookmark age difference for {}: latest {} value is {}, cache points to {}",
                repo.get_repoid(),
                bookmark,
                blobrepo_bcs_id,
                service_bcs_id,
            );

            let difference = if blobrepo_bcs_id == service_bcs_id {
                0
            } else {
                let limit = 100;
                let maybe_child = self
                    .try_find_child(ctx, service_bcs_id, blobrepo_bcs_id, limit)
                    .await?;

                // If we can't find a child of a bookmark value from cache, then it might mean
                // that either cache is too far behind or there was a non-forward bookmark move.
                // Either way, we can't really do much about it here, so let's just find difference
                // between current timestamp and bookmark value from cache.
                let compare_bcs_id = maybe_child.unwrap_or(service_bcs_id);

                let compare_timestamp = compare_bcs_id
                    .load(&ctx, repo.blobstore())
                    .await?
                    .author_date()
                    .timestamp_secs();

                let current_timestamp = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(Error::from)?;
                let current_timestamp = current_timestamp.as_secs() as i64;
                current_timestamp - compare_timestamp
            };
            self.report_bookmark_staleness(ctx, bookmark, difference);
        }

        Ok(())
    }

    /// Try to find a changeset that's ancestor of `descendant` and direct child of
    /// `ancestor`. Returns None if this commit doesn't exist (for example if `ancestor` is not
    /// actually an ancestor of `descendant`) or if child is too far away from descendant.
    async fn try_find_child(
        &self,
        ctx: &CoreContext,
        ancestor: ChangesetId,
        descendant: ChangesetId,
        limit: u64,
    ) -> Result<Option<ChangesetId>, Error> {
        // This is a generation number beyond which we don't need to traverse
        let min_gen_num = self.fetch_gen_num(ctx, &ancestor).await?;

        let mut ancestors = AncestorsNodeStream::new(
            ctx.clone(),
            &self.blob_repo.get_changeset_fetcher(),
            descendant,
        )
        .compat();

        let mut traversed = 0;
        while let Some(cs_id) = ancestors.next().await {
            traversed += 1;
            if traversed > limit {
                return Ok(None);
            }

            let cs_id = cs_id?;
            let parents = self
                .blob_repo
                .get_changeset_parents_by_bonsai(ctx.clone(), cs_id)
                .await?;

            if parents.contains(&ancestor) {
                return Ok(Some(cs_id));
            } else {
                let gen_num = self.fetch_gen_num(ctx, &cs_id).await?;
                if gen_num < min_gen_num {
                    return Ok(None);
                }
            }
        }

        Ok(None)
    }

    async fn fetch_gen_num(
        &self,
        ctx: &CoreContext,
        cs_id: &ChangesetId,
    ) -> Result<Generation, Error> {
        let maybe_gen_num = self
            .blob_repo
            .get_generation_number(ctx.clone(), *cs_id)
            .await?;
        maybe_gen_num.ok_or(format_err!("gen num for {} not found", cs_id))
    }

    async fn check_permissions(&self, ctx: &CoreContext, mode: &str) -> Result<(), MononokeError> {
        let identities = ctx.metadata().identities();

        if !self
            .repo_permission_checker
            .check_set(&*identities, &[mode])
            .await?
        {
            debug!(
                ctx.logger(),
                "Permission denied: {} access to {}", mode, self.name
            );
            let identities = if identities.is_empty() {
                "<none>".to_string()
            } else {
                identities.iter().join(",")
            };
            return Err(MononokeError::PermissionDenied {
                mode: mode.to_string(),
                identities,
                reponame: self.name.clone(),
            });
        }
        Ok(())
    }

    async fn check_service_permissions(
        &self,
        ctx: &CoreContext,
        service_identity: String,
    ) -> Result<(), MononokeError> {
        let identities = ctx.metadata().identities();

        if !self
            .service_permission_checker
            .check_set(&*identities, &[&service_identity])
            .await?
        {
            debug!(
                ctx.logger(),
                "Permission denied: access to {} on behalf of {}", self.name, service_identity,
            );
            let identities = if identities.is_empty() {
                "<none>".to_string()
            } else {
                identities.iter().join(",")
            };
            return Err(MononokeError::ServicePermissionDenied {
                identities,
                reponame: self.name.clone(),
                service_identity,
            });
        }
        Ok(())
    }
}

#[derive(Default)]
pub struct Stack {
    pub draft: Vec<ChangesetId>,
    pub public: Vec<ChangesetId>,
    pub leftover_heads: Vec<ChangesetId>,
}

/// A context object representing a query to a particular repo.
impl RepoContext {
    pub async fn new(ctx: CoreContext, repo: Arc<Repo>) -> Result<Self, MononokeError> {
        // Check the user is permitted to access this repo.
        repo.check_permissions(&ctx, "read").await?;
        Ok(Self { repo, ctx })
    }

    /// Initializes the repo without the ACL check.
    ///
    /// Should be used in the internal services that don't serve user queries so all operations are
    /// trusted.
    pub async fn new_bypass_acl_check(
        ctx: CoreContext,
        repo: Arc<Repo>,
    ) -> Result<Self, MononokeError> {
        Ok(Self { repo, ctx })
    }

    /// The context for this query.
    pub fn ctx(&self) -> &CoreContext {
        &self.ctx
    }

    /// The name of the underlying repo.
    pub fn name(&self) -> &str {
        &self.repo.name()
    }

    /// The internal id of the repo. Used for comparing the repo objects with each other.
    pub fn repoid(&self) -> RepositoryId {
        self.repo.repoid()
    }

    /// The underlying `BlobRepo`.
    pub fn blob_repo(&self) -> &BlobRepo {
        &self.repo.blob_repo()
    }

    /// `LiveCommitSyncConfig` instance to query current state of sync configs.
    pub fn live_commit_sync_config(&self) -> Arc<dyn LiveCommitSyncConfig> {
        self.repo.live_commit_sync_config()
    }

    /// The skiplist index for the referenced repository.
    pub fn skiplist_index(&self) -> &Arc<SkiplistIndex> {
        self.repo.skiplist_index()
    }

    /// The commit sync mapping for the referenced repository
    pub fn synced_commit_mapping(&self) -> &Arc<dyn SyncedCommitMapping> {
        self.repo.synced_commit_mapping()
    }

    /// The warm bookmarks cache for the referenced repository.
    pub fn warm_bookmarks_cache(&self) -> &Arc<WarmBookmarksCache> {
        self.repo.warm_bookmarks_cache()
    }

    /// The hook manager for the referenced repository.
    pub fn hook_manager(&self) -> &Arc<HookManager> {
        self.repo.hook_manager()
    }

    /// The Read/Write or Read-Only status fetcher.
    pub fn readonly_fetcher(&self) -> &RepoReadWriteFetcher {
        self.repo.readonly_fetcher()
    }

    /// The configuration for the referenced repository.
    pub fn config(&self) -> &RepoConfig {
        self.repo.config()
    }

    pub fn derive_changeset_info_enabled(&self) -> bool {
        self.blob_repo()
            .get_derived_data_config()
            .is_enabled(ChangesetInfo::NAME)
    }

    pub fn derive_hgchangesets_enabled(&self) -> bool {
        self.blob_repo()
            .get_derived_data_config()
            .is_enabled(MappedHgChangesetId::NAME)
    }

    /// Look up a changeset specifier to find the canonical bonsai changeset
    /// ID for a changeset.
    pub async fn resolve_specifier(
        &self,
        specifier: ChangesetSpecifier,
    ) -> Result<Option<ChangesetId>, MononokeError> {
        let id = match specifier {
            ChangesetSpecifier::Bonsai(cs_id) => {
                let exists = self
                    .blob_repo()
                    .changeset_exists_by_bonsai(self.ctx.clone(), cs_id)
                    .await?;
                match exists {
                    true => Some(cs_id),
                    false => None,
                }
            }
            ChangesetSpecifier::Hg(hg_cs_id) => {
                self.blob_repo()
                    .get_bonsai_from_hg(self.ctx.clone(), hg_cs_id)
                    .await?
            }
            ChangesetSpecifier::Globalrev(rev) => {
                self.blob_repo()
                    .get_bonsai_from_globalrev(&self.ctx, rev)
                    .await?
            }
            ChangesetSpecifier::Svnrev(rev) => {
                self.blob_repo()
                    .bonsai_svnrev_mapping()
                    .get_bonsai_from_svnrev(&self.ctx, rev)
                    .await?
            }
            ChangesetSpecifier::GitSha1(git_sha1) => {
                self.blob_repo()
                    .bonsai_git_mapping()
                    .get_bonsai_from_git_sha1(&self.ctx, git_sha1)
                    .await?
            }
        };
        Ok(id)
    }

    /// Resolve a bookmark to a changeset.
    pub async fn resolve_bookmark(
        &self,
        bookmark: impl AsRef<str>,
        freshness: BookmarkFreshness,
    ) -> Result<Option<ChangesetContext>, MononokeError> {
        let bookmark = BookmarkName::new(bookmark.as_ref())?;

        let mut cs_id = match freshness {
            BookmarkFreshness::MaybeStale => self.warm_bookmarks_cache().get(&bookmark),
            BookmarkFreshness::MostRecent => None,
        };

        // If the bookmark wasn't found in the warm bookmarks cache, it might
        // be a scratch bookmark, so always do the look-up.
        if cs_id.is_none() {
            cs_id = self
                .blob_repo()
                .get_bonsai_bookmark(self.ctx.clone(), &bookmark)
                .await?
        }

        Ok(cs_id.map(|cs_id| ChangesetContext::new(self.clone(), cs_id)))
    }

    /// Resolve a changeset id by its prefix
    pub async fn resolve_changeset_id_prefix(
        &self,
        prefix: ChangesetPrefixSpecifier,
    ) -> Result<ChangesetSpecifierPrefixResolution, MononokeError> {
        const MAX_LIMIT_AMBIGUOUS_IDS: usize = 10;
        let resolved = match prefix {
            ChangesetPrefixSpecifier::Hg(prefix) => ChangesetSpecifierPrefixResolution::from(
                self.blob_repo()
                    .get_bonsai_hg_mapping()
                    .get_many_hg_by_prefix(
                        &self.ctx,
                        self.blob_repo().get_repoid(),
                        prefix,
                        MAX_LIMIT_AMBIGUOUS_IDS,
                    )
                    .await?,
            ),
            ChangesetPrefixSpecifier::Bonsai(prefix) => ChangesetSpecifierPrefixResolution::from(
                self.blob_repo()
                    .get_changesets_object()
                    .get_many_by_prefix(
                        self.ctx.clone(),
                        self.blob_repo().get_repoid(),
                        prefix,
                        MAX_LIMIT_AMBIGUOUS_IDS,
                    )
                    .await?,
            ),
            ChangesetPrefixSpecifier::Globalrev(prefix) => {
                ChangesetSpecifierPrefixResolution::from(
                    self.blob_repo()
                        .bonsai_globalrev_mapping()
                        .get_closest_globalrev(&self.ctx, self.blob_repo().get_repoid(), prefix)
                        .await?,
                )
            }
        };
        Ok(resolved)
    }

    /// Look up a changeset by specifier.
    pub async fn changeset(
        &self,
        specifier: impl Into<ChangesetSpecifier>,
    ) -> Result<Option<ChangesetContext>, MononokeError> {
        let changeset = self
            .resolve_specifier(specifier.into())
            .await?
            .map(|cs_id| ChangesetContext::new(self.clone(), cs_id));
        Ok(changeset)
    }

    /// Get Mercurial ID for multiple changesets
    ///
    /// This is a more efficient version of:
    /// ```ignore
    /// let ids: Vec<ChangesetId> = ...;
    /// ids.into_iter().map(|id| {
    ///     let hg_id = repo
    ///         .changeset(ChangesetSpecifier::Bonsai(id))
    ///         .await
    ///         .hg_id();
    ///     (id, hg_id)
    /// });
    /// ```
    pub async fn changeset_hg_ids(
        &self,
        changesets: Vec<ChangesetId>,
    ) -> Result<Vec<(ChangesetId, HgChangesetId)>, MononokeError> {
        let mapping = self
            .blob_repo()
            .get_hg_bonsai_mapping(self.ctx.clone(), changesets)
            .await?
            .into_iter()
            .map(|(hg_cs_id, cs_id)| (cs_id, hg_cs_id))
            .collect();
        Ok(mapping)
    }

    /// Similar to changeset_hg_ids, but returning Git-SHA1s.
    pub async fn changeset_git_sha1s(
        &self,
        changesets: Vec<ChangesetId>,
    ) -> Result<Vec<(ChangesetId, GitSha1)>, MononokeError> {
        let mapping = self
            .blob_repo()
            .bonsai_git_mapping()
            .get(&self.ctx, changesets.into())
            .await?
            .into_iter()
            .map(|entry| (entry.bcs_id, entry.git_sha1))
            .collect();
        Ok(mapping)
    }

    /// Similar to changeset_hg_ids, but returning Globalrevs.
    pub async fn changeset_globalrev_ids(
        &self,
        changesets: Vec<ChangesetId>,
    ) -> Result<Vec<(ChangesetId, Globalrev)>, MononokeError> {
        let mapping = self
            .blob_repo()
            .get_bonsai_globalrev_mapping(&self.ctx, changesets)
            .await?
            .into_iter()
            .collect();
        Ok(mapping)
    }

    /// Similar to changeset_hg_ids, but returning Svnrevs.
    pub async fn changeset_svnrev_ids(
        &self,
        changesets: Vec<ChangesetId>,
    ) -> Result<Vec<(ChangesetId, Svnrev)>, MononokeError> {
        let mapping = self
            .blob_repo()
            .bonsai_svnrev_mapping()
            .get(&self.ctx, changesets.into())
            .await?
            .into_iter()
            .map(|entry| (entry.bcs_id, entry.svnrev))
            .collect();
        Ok(mapping)
    }

    /// Get a list of bookmarks.
    pub fn list_bookmarks(
        &self,
        include_scratch: bool,
        prefix: Option<&str>,
        after: Option<&str>,
        limit: Option<u64>,
    ) -> Result<impl Stream<Item = Result<(String, ChangesetId), MononokeError>> + '_, MononokeError>
    {
        if include_scratch {
            if prefix.is_none() {
                return Err(MononokeError::InvalidRequest(
                    "prefix required to list scratch bookmarks".to_string(),
                ));
            }
            if limit.is_none() {
                return Err(MononokeError::InvalidRequest(
                    "limit required to list scratch bookmarks".to_string(),
                ));
            }
        }

        let prefix = match prefix {
            Some(prefix) => BookmarkPrefix::new(prefix).map_err(|e| {
                MononokeError::InvalidRequest(format!(
                    "invalid bookmark prefix '{}': {}",
                    prefix, e
                ))
            })?,
            None => BookmarkPrefix::empty(),
        };

        let pagination = match after {
            Some(after) => {
                let name = BookmarkName::new(after).map_err(|e| {
                    MononokeError::InvalidRequest(format!(
                        "invalid bookmark name '{}': {}",
                        after, e
                    ))
                })?;
                BookmarkPagination::After(name)
            }
            None => BookmarkPagination::FromStart,
        };

        if include_scratch {
            // Scratch bookmarks must be queried directly from the blobrepo as
            // they are not stored in the cache.  To maintain ordering with
            // public bookmarks, query all the bookmarks we are interested in.
            let blob_repo = self.blob_repo();
            let cache = self.warm_bookmarks_cache();
            let bookmarks = blob_repo
                .attribute_expected::<dyn Bookmarks>()
                .list(
                    self.ctx.clone(),
                    BookmarkFreshness::MaybeStale,
                    &prefix,
                    &BookmarkKind::ALL,
                    &pagination,
                    limit.unwrap_or(std::u64::MAX),
                )
                .try_filter_map(move |(bookmark, cs_id)| async move {
                    if bookmark.kind() == &BookmarkKind::Scratch {
                        Ok(Some((bookmark.into_name().into_string(), cs_id)))
                    } else {
                        // For non-scratch bookmarks, always return the value
                        // from the cache so that clients only ever see the
                        // warm value.  If the bookmark is newly created and
                        // has no warm value, this might mean we have to
                        // filter this bookmark out.
                        let bookmark_name = bookmark.into_name();
                        let maybe_cs_id = cache.get(&bookmark_name);
                        Ok(maybe_cs_id.map(|cs_id| (bookmark_name.into_string(), cs_id)))
                    }
                })
                .map_err(MononokeError::from)
                .boxed();
            Ok(bookmarks)
        } else {
            // Public bookmarks can be fetched from the warm bookmarks cache.
            let cache = self.warm_bookmarks_cache();
            Ok(stream::iter(cache.list(&prefix, &pagination, limit))
                .map(|(bookmark, (cs_id, _kind))| Ok((bookmark.into_string(), cs_id)))
                .boxed())
        }
    }

    /// Get a stack for the list of heads (up to the first public commit).
    ///
    /// Limit constrains the number of draft commits returned.
    /// Algo is designed to minimize number of db queries.
    /// Missing changesets are skipped.
    /// Changesets are returned in topological order (requested heads first)
    ///
    /// When the limit is reached returns the heads of which children were not processed to allow
    /// for continuation of the processing.
    pub async fn stack(
        &self,
        changesets: Vec<ChangesetId>,
        limit: usize,
    ) -> Result<Stack, MononokeError> {
        if limit == 0 {
            return Ok(Default::default());
        }

        // initialize visited
        let mut visited: HashSet<_> = changesets.iter().cloned().collect();

        let phases = self.blob_repo().get_phases();

        // get phases
        let public_phases = phases
            .get_public(self.ctx.clone(), changesets.clone(), false)
            .await?;

        // partition
        let (mut public, mut draft): (Vec<_>, Vec<_>) = changesets
            .into_iter()
            .partition(|cs_id| public_phases.contains(cs_id));

        // initialize the queue
        let mut queue: Vec<_> = draft.iter().cloned().collect();

        while !queue.is_empty() {
            // get the unique parents for all changesets in the queue & skip visited & update visited
            let parents: Vec<_> = self
                .blob_repo()
                .get_changesets_object()
                .get_many(
                    self.ctx.clone(),
                    self.blob_repo().get_repoid(),
                    queue.clone(),
                )
                .await?
                .into_iter()
                .map(|cs_entry| cs_entry.parents)
                .flatten()
                .filter(|cs_id| !visited.contains(cs_id))
                .unique()
                .collect();

            visited.extend(parents.iter().cloned());

            // get phases for the parents
            let public_phases = phases
                .get_public(self.ctx.clone(), parents.clone(), false)
                .await?;

            // partition
            let (new_public, new_draft): (Vec<_>, Vec<_>) = parents
                .into_iter()
                .partition(|cs_id| public_phases.contains(cs_id));

            // respect the limit
            if draft.len() + new_draft.len() > limit {
                break;
            }

            // update queue and level
            queue = new_draft.clone();

            // update draft & public
            public.extend(new_public.into_iter());
            draft.extend(new_draft.into_iter());
        }

        Ok(Stack {
            draft,
            public,
            leftover_heads: queue,
        })
    }

    /// Get a Tree by id.  Returns `None` if the tree doesn't exist.
    pub async fn tree(&self, tree_id: TreeId) -> Result<Option<TreeContext>, MononokeError> {
        TreeContext::new_check_exists(self.clone(), tree_id).await
    }

    /// Get a File by id.  Returns `None` if the file doesn't exist.
    pub async fn file(&self, file_id: FileId) -> Result<Option<FileContext>, MononokeError> {
        FileContext::new_check_exists(self.clone(), FetchKey::Canonical(file_id)).await
    }

    /// Get a File by content sha-1.  Returns `None` if the file doesn't exist.
    pub async fn file_by_content_sha1(
        &self,
        hash: Sha1,
    ) -> Result<Option<FileContext>, MononokeError> {
        FileContext::new_check_exists(self.clone(), FetchKey::Aliased(Alias::Sha1(hash))).await
    }

    /// Get a File by content sha-256.  Returns `None` if the file doesn't exist.
    pub async fn file_by_content_sha256(
        &self,
        hash: Sha256,
    ) -> Result<Option<FileContext>, MononokeError> {
        FileContext::new_check_exists(self.clone(), FetchKey::Aliased(Alias::Sha256(hash))).await
    }

    fn get_target_repo_and_lca_hint(
        &self,
    ) -> (Target<BlobRepo>, Target<Arc<dyn LeastCommonAncestorsHint>>) {
        let blob_repo = self.blob_repo().clone();
        // TODO(ikostia): get rid of cloning the underlying `SkiplistIndex` here
        let lca_hint: Arc<dyn LeastCommonAncestorsHint> =
            Arc::new((*self.repo.skiplist_index).clone());
        (Target(blob_repo), Target(lca_hint))
    }

    async fn build_candidate_selection_hint(
        &self,
        maybe_args: Option<CandidateSelectionHintArgs>,
        other_repo_context: &Self,
    ) -> Result<CandidateSelectionHint, MononokeError> {
        let args = match maybe_args {
            None => return Ok(CandidateSelectionHint::Only),
            Some(args) => args,
        };

        use CandidateSelectionHintArgs::*;
        match args {
            OnlyOrAncestorOfBookmark(bookmark) => {
                let (blob_repo, lca_hint) = other_repo_context.get_target_repo_and_lca_hint();
                Ok(CandidateSelectionHint::OnlyOrAncestorOfBookmark(
                    Target(bookmark),
                    blob_repo,
                    lca_hint,
                ))
            }
            OnlyOrDescendantOfBookmark(bookmark) => {
                let (blob_repo, lca_hint) = other_repo_context.get_target_repo_and_lca_hint();
                Ok(CandidateSelectionHint::OnlyOrDescendantOfBookmark(
                    Target(bookmark),
                    blob_repo,
                    lca_hint,
                ))
            }
            OnlyOrAncestorOfCommit(specifier) => {
                let (blob_repo, lca_hint) = other_repo_context.get_target_repo_and_lca_hint();
                let cs_id = other_repo_context
                    .resolve_specifier(specifier)
                    .await?
                    .ok_or_else(|| {
                        MononokeError::InvalidRequest(format!(
                            "unknown commit specifier {}",
                            specifier
                        ))
                    })?;
                Ok(CandidateSelectionHint::OnlyOrAncestorOfCommit(
                    Target(cs_id),
                    blob_repo,
                    lca_hint,
                ))
            }
            OnlyOrDescendantOfCommit(specifier) => {
                let (blob_repo, lca_hint) = other_repo_context.get_target_repo_and_lca_hint();
                let cs_id = other_repo_context
                    .resolve_specifier(specifier)
                    .await?
                    .ok_or_else(|| {
                        MononokeError::InvalidRequest(format!(
                            "unknown commit specifier {}",
                            specifier
                        ))
                    })?;
                Ok(CandidateSelectionHint::OnlyOrDescendantOfCommit(
                    Target(cs_id),
                    blob_repo,
                    lca_hint,
                ))
            }
            Exact(specifier) => {
                let cs_id = other_repo_context
                    .resolve_specifier(specifier)
                    .await?
                    .ok_or_else(|| {
                        MononokeError::InvalidRequest(format!(
                            "unknown commit specifier {}",
                            specifier
                        ))
                    })?;
                Ok(CandidateSelectionHint::Exact(Target(cs_id)))
            }
        }
    }

    /// Get the equivalent changeset from another repo - it will sync it if needed
    pub async fn xrepo_commit_lookup(
        &self,
        other: &Self,
        specifier: impl Into<ChangesetSpecifier>,
        maybe_candidate_selection_hint_args: Option<CandidateSelectionHintArgs>,
    ) -> Result<Option<ChangesetContext>, MononokeError> {
        let commit_sync_config = self
            .live_commit_sync_config()
            .get_current_commit_sync_config(self.ctx(), self.blob_repo().get_repoid())
            .await
            .map_err(|e| {
                MononokeError::InvalidRequest(format!(
                    "Commits from {} are not configured to be remapped to another repo: {}",
                    self.repo.name, e
                ))
            })?;

        let candidate_selection_hint: CandidateSelectionHint = self
            .build_candidate_selection_hint(maybe_candidate_selection_hint_args, &other)
            .await?;

        let commit_sync_repos = CommitSyncRepos::new(
            self.blob_repo().clone(),
            other.blob_repo().clone(),
            &commit_sync_config,
        )?;

        let specifier = specifier.into();
        let changeset =
            self.resolve_specifier(specifier)
                .await?
                .ok_or(MononokeError::InvalidRequest(format!(
                    "unknown commit specifier {}",
                    specifier
                )))?;

        let commit_syncer = CommitSyncer::new(
            &self.ctx,
            self.synced_commit_mapping().clone(),
            commit_sync_repos,
            self.live_commit_sync_config(),
        );

        let maybe_cs_id = commit_syncer
            .sync_commit(
                &self.ctx,
                changeset,
                candidate_selection_hint,
                CommitSyncContext::ScsXrepoLookup,
            )
            .await?;
        Ok(maybe_cs_id.map(|cs_id| ChangesetContext::new(other.clone(), cs_id)))
    }

    /// Get a write context to make changes to this repository.
    pub async fn write(mut self) -> Result<RepoWriteContext, MononokeError> {
        if !self.config().source_control_service.permit_writes {
            return Err(MononokeError::InvalidRequest(String::from(
                "source control service writes are not enabled for this repo",
            )));
        }

        // Check the user is permitted to write to this repo.
        self.repo.check_permissions(&self.ctx, "write").await?;

        self.ctx = self.ctx.with_mutated_scuba(|mut scuba| {
            scuba.add("write_permissions_model", "any");
            scuba
        });

        self.ctx
            .scuba()
            .clone()
            .log_with_msg("Write request start", None);

        Ok(RepoWriteContext::new(self, PermissionsModel::AllowAnyWrite))
    }

    /// Get a write context to make changes to this repository on behalf of a service.
    pub async fn service_write(
        mut self,
        service_identity: String,
    ) -> Result<RepoWriteContext, MononokeError> {
        if !self.config().source_control_service.permit_service_writes {
            return Err(MononokeError::InvalidRequest(String::from(
                "source control service writes are not enabled for this repo",
            )));
        }

        // Check the user is permitted to speak for the named service.
        self.repo
            .check_service_permissions(&self.ctx, service_identity.clone())
            .await?;

        self.ctx = self.ctx.with_mutated_scuba(|mut scuba| {
            scuba.add("write_permissions_model", "service");
            scuba.add("service_identity", service_identity.as_str());
            scuba
        });

        self.ctx
            .scuba()
            .clone()
            .log_with_msg("Write request start", None);

        Ok(RepoWriteContext::new(
            self,
            PermissionsModel::ServiceIdentity(service_identity),
        ))
    }

    /// A SegmentedChangelog client repository has a compressed shape of the commit graph but
    /// doesn't know the identifiers for all the commits in the graph. It only knows the
    /// identifiers for select commits called "known" commits. These repositories can query
    /// the server to get the identifiers of the commits they don't have using the location
    /// of the desired commit relative to one of the "known" commits.
    /// The current version has all parents of merge commits downloaded to clients so that
    /// locations can be expressed using only the unique descendant distance to one of these
    /// commits. The heads of the repo are also known.
    /// Let's assume our graph is `0 - a - b - c`.
    /// In this example our initial commit is `0`, then we have `a` the first commit, `b` second,
    /// `c` third.
    /// For `descendant = c` and `distance = 2` we want to return `a`.
    pub async fn location_to_changeset_id(
        &self,
        location: Location<ChangesetId>,
        count: u64,
    ) -> Result<Vec<ChangesetId>, MononokeError> {
        let blob_repo = self.blob_repo();
        let segmented_changelog =
            blob_repo
                .attribute::<dyn SegmentedChangelog>()
                .ok_or_else(|| {
                    MononokeError::InvalidRequest(String::from(
                        "Segmented Changelog is not enabled for this repo",
                    ))
                })?;
        let ancestor = segmented_changelog
            .location_to_many_changeset_ids(&self.ctx, location, count)
            .await
            .map_err(MononokeError::from)?;
        Ok(ancestor)
    }

    /// A Segmented Changelog client needs to know how to translate between a commit hash,
    /// for example one that is provided by the user, and the information that it has locally,
    /// the shape of the graph, i.e. a location in the graph.
    pub async fn many_changeset_ids_to_locations(
        &self,
        client_head: ChangesetId,
        cs_ids: Vec<ChangesetId>,
    ) -> Result<HashMap<ChangesetId, Location<ChangesetId>>, MononokeError> {
        let blob_repo = self.blob_repo();
        let segmented_changelog =
            blob_repo
                .attribute::<dyn SegmentedChangelog>()
                .ok_or_else(|| {
                    MononokeError::InvalidRequest(String::from(
                        "Segmented Changelog is not enabled for this repo",
                    ))
                })?;
        let result = segmented_changelog
            .many_changeset_ids_to_locations(&self.ctx, client_head, cs_ids)
            .await
            .map_err(MononokeError::from)?;
        Ok(result)
    }

    pub async fn segmented_changelog_clone_data(
        &self,
    ) -> Result<CloneData<ChangesetId>, MononokeError> {
        let blob_repo = self.blob_repo();
        let segmented_changelog =
            blob_repo
                .attribute::<dyn SegmentedChangelog>()
                .ok_or_else(|| {
                    MononokeError::InvalidRequest(String::from(
                        "Segmented Changelog is not enabled for this repo",
                    ))
                })?;
        let clone_data = segmented_changelog
            .clone_data(&self.ctx)
            .await
            .map_err(MononokeError::from)?;
        Ok(clone_data)
    }

    pub async fn segmented_changelog_full_idmap_clone_data(
        &self,
    ) -> Result<StreamCloneData<ChangesetId>, MononokeError> {
        let blob_repo = self.blob_repo();
        let segmented_changelog =
            blob_repo
                .attribute::<dyn SegmentedChangelog>()
                .ok_or_else(|| {
                    MononokeError::InvalidRequest(String::from(
                        "Segmented Changelog is not enabled for this repo",
                    ))
                })?;
        let clone_data = segmented_changelog
            .full_idmap_clone_data(&self.ctx)
            .await
            .map_err(MononokeError::from)?;
        Ok(clone_data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fixtures::{linear, merge_even};
    use std::str::FromStr;

    #[fbinit::test]
    async fn test_try_find_child(fb: FacebookInit) -> Result<(), Error> {
        let ctx = CoreContext::test_mock(fb);
        let repo = Repo::new_test(ctx.clone(), linear::getrepo(fb).await).await?;

        let ancestor = ChangesetId::from_str(
            "c9f9a2a39195a583d523a4e5f6973443caeb0c66a315d5bf7db1b5775c725310",
        )?;
        let descendant = ChangesetId::from_str(
            "7785606eb1f26ff5722c831de402350cf97052dc44bc175da6ac0d715a3dbbf6",
        )?;

        let maybe_child = repo.try_find_child(&ctx, ancestor, descendant, 100).await?;
        let child = maybe_child.ok_or(format_err!("didn't find child"))?;
        assert_eq!(
            child,
            ChangesetId::from_str(
                "98ef3234c2f1acdbb272715e8cfef4a6378e5443120677e0d87d113571280f79"
            )?
        );

        let maybe_child = repo.try_find_child(&ctx, ancestor, descendant, 1).await?;
        assert!(maybe_child.is_none());

        Ok(())
    }

    #[fbinit::test]
    async fn test_try_find_child_merge(fb: FacebookInit) -> Result<(), Error> {
        let ctx = CoreContext::test_mock(fb);
        let repo = Repo::new_test(ctx.clone(), merge_even::getrepo(fb).await).await?;

        let ancestor = ChangesetId::from_str(
            "35fb4e0fb3747b7ca4d18281d059be0860d12407dc5dce5e02fb99d1f6a79d2a",
        )?;
        let descendant = ChangesetId::from_str(
            "567a25d453cafaef6550de955c52b91bf9295faf38d67b6421d5d2e532e5adef",
        )?;

        let maybe_child = repo.try_find_child(&ctx, ancestor, descendant, 100).await?;
        let child = maybe_child.ok_or(format_err!("didn't find child"))?;
        assert_eq!(child, descendant);
        Ok(())
    }
}

impl PartialEq for RepoContext {
    fn eq(&self, other: &Self) -> bool {
        self.repoid() == other.repoid()
    }
}
impl Eq for RepoContext {}

impl Hash for RepoContext {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.repoid().hash(state);
    }
}
