/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::Error;
use anyhow::anyhow;
use ascii::AsciiString;
use assert_matches::assert_matches;
use blobrepo_hg::BlobRepoHg;
use blobstore::Loadable;
use bonsai_hg_mapping::BonsaiHgMappingRef;
use bookmarks::BookmarkKey;
use bookmarks::BookmarkUpdateLogArc;
use bookmarks::BookmarkUpdateLogId;
use bookmarks::BookmarkUpdateLogRef;
use bookmarks::BookmarkUpdateReason;
use bookmarks::BookmarksArc;
use bookmarks::BookmarksMaybeStaleExt;
use bookmarks::BookmarksRef;
use bookmarks::Freshness;
use changesets_creation::save_changesets;
use cloned::cloned;
use commit_graph::CommitGraphRef;
use commit_transformation::upload_commits;
use context::CoreContext;
use cross_repo_sync::CHANGE_XREPO_MAPPING_EXTRA;
use cross_repo_sync::CandidateSelectionHint;
use cross_repo_sync::CommitSyncContext;
use cross_repo_sync::CommitSyncData;
use cross_repo_sync::CommitSyncOutcome;
use cross_repo_sync::CommitSyncRepos;
use cross_repo_sync::SubmoduleDeps;
use cross_repo_sync::get_git_submodule_action_by_version;
use cross_repo_sync::rewrite_commit;
use cross_repo_sync::test_utils::TestRepo;
use cross_repo_sync::unsafe_always_rewrite_sync_commit;
use cross_repo_sync::unsafe_sync_commit;
use fbinit::FacebookInit;
use fixtures::Linear;
use fixtures::TestRepoFixture;
use futures::FutureExt;
use futures::TryFutureExt;
use futures::TryStreamExt;
use futures::future;
use futures_ext::FbTryFutureExt;
use justknobs::test_helpers::JustKnobsInMemory;
use justknobs::test_helpers::KnobVal;
use justknobs::test_helpers::override_just_knobs;
use justknobs::test_helpers::with_just_knobs_async;
use live_commit_sync_config::TestLiveCommitSyncConfig;
use manifest::Entry;
use manifest::ManifestOps;
use maplit::btreemap;
use maplit::hashmap;
use mercurial_derivation::DeriveHgChangeset;
use mercurial_types::HgChangesetId;
use metaconfig_types::CommitSyncConfig;
use metaconfig_types::CommitSyncConfigVersion;
use metaconfig_types::CommitSyncDirection;
use metaconfig_types::CommonCommitSyncConfig;
use metaconfig_types::DefaultSmallToLargeCommitSyncPathAction;
use metaconfig_types::SmallRepoCommitSyncConfig;
use metaconfig_types::SmallRepoPermanentConfig;
use mononoke_macros::mononoke;
use mononoke_types::ChangesetId;
use mononoke_types::NonRootMPath;
use mononoke_types::RepositoryId;
use movers::Movers;
use mutable_counters::MutableCountersArc;
use pretty_assertions::assert_eq;
use rendezvous::RendezVousOptions;
use repo_blobstore::RepoBlobstoreRef;
use repo_identity::RepoIdentityRef;
use sql_construct::SqlConstruct;
use synced_commit_mapping::EquivalentWorkingCopyEntry;
use synced_commit_mapping::SqlSyncedCommitMapping;
use synced_commit_mapping::SqlSyncedCommitMappingBuilder;
use synced_commit_mapping::SyncedCommitMapping;
use synced_commit_mapping::SyncedCommitMappingEntry;
use synced_commit_mapping::SyncedCommitSourceRepo;
use test_repo_factory::TestRepoFactory;
use tests_utils::CreateCommitContext;
use tests_utils::bookmark;
use tests_utils::create_commit;
use tests_utils::list_working_copy_utf8;
use tests_utils::resolve_cs_id;
use tests_utils::store_files;
use tests_utils::store_rename;
use tokio::runtime::Runtime;
use wireproto_handler::TargetRepoDbs;

use crate::BacksyncLimit;
use crate::backsync_latest;
use crate::format_counter;
use crate::sync_entries;

const REPOMERGE_FOLDER: &str = "repomerge";
const REPOMERGE_FILE: &str = "repomergefile";
const BRANCHMERGE_FILE: &str = "branchmerge";

#[mononoke::fbinit_test]
async fn backsync_linear_simple(fb: FacebookInit) -> Result<(), Error> {
    let (commit_sync_data, small_repo_dbs) =
        init_repos(fb, MoverType::Noop, BookmarkRenamerType::Noop).await?;
    backsync_and_verify_master_wc(fb, commit_sync_data.clone(), small_repo_dbs).await?;

    let ctx = CoreContext::test_mock(fb);
    let target_cs_id = resolve_cs_id(&ctx, commit_sync_data.get_target_repo(), "master").await?;

    let map =
        list_working_copy_utf8(&ctx, commit_sync_data.get_target_repo(), target_cs_id).await?;
    assert_eq!(
        map.into_iter().collect::<BTreeMap<_, _>>(),
        btreemap! {
                NonRootMPath::new("1")? => "1\n".to_string(),
                NonRootMPath::new("2")? => "2\n".to_string(),
                NonRootMPath::new("3")? => "3\n".to_string(),
                NonRootMPath::new("4")? => "4\n".to_string(),
                NonRootMPath::new("5")? => "5\n".to_string(),
                NonRootMPath::new("6")? => "6\n".to_string(),
                NonRootMPath::new("7")? => "7\n".to_string(),
                NonRootMPath::new("8")? => "8\n".to_string(),
                NonRootMPath::new("9")? => "9\n".to_string(),
                NonRootMPath::new("10")? => "modified10\n".to_string(),
                NonRootMPath::new("randomfile")? => "some other content".to_string(),
                NonRootMPath::new("files")? => "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n".to_string(),
        }
    );

    let target_cs_id =
        resolve_cs_id(&ctx, commit_sync_data.get_target_repo(), "anotherbookmark").await?;
    let map =
        list_working_copy_utf8(&ctx, commit_sync_data.get_target_repo(), target_cs_id).await?;
    assert_eq!(
        map.into_iter().collect::<BTreeMap<_, _>>(),
        btreemap! {
                NonRootMPath::new("1")? => "1\n".to_string(),
                NonRootMPath::new("2")? => "2\n".to_string(),
                NonRootMPath::new("3")? => "merged 3".to_string(),
                NonRootMPath::new("4")? => "4\n".to_string(),
                NonRootMPath::new("5")? => "5\n".to_string(),
                NonRootMPath::new("6")? => "6\n".to_string(),
                NonRootMPath::new("7")? => "7\n".to_string(),
                NonRootMPath::new("8")? => "8\n".to_string(),
                NonRootMPath::new("9")? => "9\n".to_string(),
                NonRootMPath::new("10")? => "modified10\n".to_string(),
                NonRootMPath::new("files")? => "branchmerge files content".to_string(),
                NonRootMPath::new("branchmerge")? => "new branch merge content".to_string(),
                NonRootMPath::new("repomergefile")? => "some content".to_string(),
                NonRootMPath::new("randomfile")? => "some other content".to_string(),
                NonRootMPath::new("repomerge/first")? => "new repo content".to_string(),
                NonRootMPath::new("repomerge/movedest")? => "moved content".to_string(),
                NonRootMPath::new("repomerge/second")? => "new repo second content".to_string(),
                NonRootMPath::new("repomerge/toremove")? => "new repo content".to_string(),

        }
    );

    Ok(())
}

#[mononoke::fbinit_test]
fn test_sync_entries(fb: FacebookInit) -> Result<(), Error> {
    // Test makes sure sync_entries() actually sync ALL entries even if transaction
    // for updating bookmark and/or counter failed. This transaction failure is benign and
    // expected, it means that two backsyncers doing the same job in parallel

    let runtime = Runtime::new()?;
    runtime.block_on(async move {
        let (commit_sync_data, small_repo_dbs) =
            init_repos(fb, MoverType::Noop, BookmarkRenamerType::Noop).await?;

        let small_repo_dbs = Arc::new(small_repo_dbs);
        // Backsync a few entries
        let ctx = CoreContext::test_mock(fb);
        let fut = backsync_latest(
            ctx.clone(),
            commit_sync_data.clone(),
            small_repo_dbs.clone(),
            BacksyncLimit::Limit(2),
            Arc::new(AtomicBool::new(false)),
            CommitSyncContext::Backsyncer,
            false,
            Box::new(future::ready(())),
        )
        .map_err(Error::from)
        .await?;

        let large_repo = commit_sync_data.get_source_repo();

        let next_log_entries: Vec<_> = large_repo
            .bookmark_update_log()
            .read_next_bookmark_log_entries(
                ctx.clone(),
                BookmarkUpdateLogId(0),
                1000,
                Freshness::MostRecent,
            )
            .try_collect()
            .await?;

        // Sync entries starting from counter 0. sync_entries() function should skip
        // 2 first entries, and sync all entries after that
        sync_entries(
            ctx.clone(),
            &commit_sync_data,
            small_repo_dbs.clone(),
            next_log_entries.clone(),
            BookmarkUpdateLogId(0),
            Arc::new(AtomicBool::new(false)),
            CommitSyncContext::Backsyncer,
            false,
            fut,
        )
        .await?
        .await;

        let latest_log_id = next_log_entries.len() as i64;

        // Make sure all of the entries were synced
        let fetched_value = small_repo_dbs
            .counters
            .get_counter(&ctx, &format_counter(&large_repo.repo_identity().id()))
            .await?;

        assert_eq!(fetched_value, Some(latest_log_id));

        Ok(())
    })
}

#[mononoke::fbinit_test]
async fn backsync_linear_with_mover_that_removes_some_files(fb: FacebookInit) -> Result<(), Error> {
    let (commit_sync_data, small_repo_dbs) = init_repos(
        fb,
        MoverType::Only("files".to_string()),
        BookmarkRenamerType::Noop,
    )
    .await?;

    backsync_and_verify_master_wc(fb, commit_sync_data.clone(), small_repo_dbs).await?;
    let ctx = CoreContext::test_mock(fb);
    let target_cs_id = resolve_cs_id(&ctx, commit_sync_data.get_target_repo(), "master").await?;

    let map =
        list_working_copy_utf8(&ctx, commit_sync_data.get_target_repo(), target_cs_id).await?;
    assert_eq!(
        map,
        hashmap! {NonRootMPath::new("files")? => "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n".to_string()}
    );

    let target_cs_id =
        resolve_cs_id(&ctx, commit_sync_data.get_target_repo(), "anotherbookmark").await?;
    let map =
        list_working_copy_utf8(&ctx, commit_sync_data.get_target_repo(), target_cs_id).await?;
    assert_eq!(
        map,
        hashmap! {NonRootMPath::new("files")? => "branchmerge files content".to_string()}
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn backsync_linear_with_mover_that_removes_single_file(
    fb: FacebookInit,
) -> Result<(), Error> {
    let (commit_sync_data, small_repo_dbs) = init_repos(
        fb,
        MoverType::Except(vec!["10".to_string()]),
        BookmarkRenamerType::Noop,
    )
    .await?;

    backsync_and_verify_master_wc(fb, commit_sync_data.clone(), small_repo_dbs).await?;

    let ctx = CoreContext::test_mock(fb);
    let target_cs_id = resolve_cs_id(&ctx, commit_sync_data.get_target_repo(), "master").await?;

    let map =
        list_working_copy_utf8(&ctx, commit_sync_data.get_target_repo(), target_cs_id).await?;
    assert_eq!(
        map.into_iter().collect::<BTreeMap<_, _>>(),
        btreemap! {
                NonRootMPath::new("1")? => "1\n".to_string(),
                NonRootMPath::new("2")? => "2\n".to_string(),
                NonRootMPath::new("3")? => "3\n".to_string(),
                NonRootMPath::new("4")? => "4\n".to_string(),
                NonRootMPath::new("5")? => "5\n".to_string(),
                NonRootMPath::new("6")? => "6\n".to_string(),
                NonRootMPath::new("7")? => "7\n".to_string(),
                NonRootMPath::new("8")? => "8\n".to_string(),
                NonRootMPath::new("9")? => "9\n".to_string(),
                NonRootMPath::new("randomfile")? => "some other content".to_string(),
                NonRootMPath::new("files")? => "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n".to_string(),
        }
    );

    let target_cs_id =
        resolve_cs_id(&ctx, commit_sync_data.get_target_repo(), "anotherbookmark").await?;
    let map =
        list_working_copy_utf8(&ctx, commit_sync_data.get_target_repo(), target_cs_id).await?;
    assert_eq!(
        map.into_iter().collect::<BTreeMap<_, _>>(),
        btreemap! {
                NonRootMPath::new("1")? => "1\n".to_string(),
                NonRootMPath::new("2")? => "2\n".to_string(),
                NonRootMPath::new("3")? => "merged 3".to_string(),
                NonRootMPath::new("4")? => "4\n".to_string(),
                NonRootMPath::new("5")? => "5\n".to_string(),
                NonRootMPath::new("6")? => "6\n".to_string(),
                NonRootMPath::new("7")? => "7\n".to_string(),
                NonRootMPath::new("8")? => "8\n".to_string(),
                NonRootMPath::new("9")? => "9\n".to_string(),
                NonRootMPath::new("files")? => "branchmerge files content".to_string(),
                NonRootMPath::new("branchmerge")? => "new branch merge content".to_string(),
                NonRootMPath::new("repomergefile")? => "some content".to_string(),
                NonRootMPath::new("randomfile")? => "some other content".to_string(),
                NonRootMPath::new("repomerge/first")? => "new repo content".to_string(),
                NonRootMPath::new("repomerge/movedest")? => "moved content".to_string(),
                NonRootMPath::new("repomerge/second")? => "new repo second content".to_string(),
                NonRootMPath::new("repomerge/toremove")? => "new repo content".to_string(),

        }
    );

    Ok(())
}

#[mononoke::fbinit_test]
async fn backsync_linear_bookmark_renamer_only_master(fb: FacebookInit) -> Result<(), Error> {
    let master = BookmarkKey::new("master")?;
    let (commit_sync_data, small_repo_dbs) =
        init_repos(fb, MoverType::Noop, BookmarkRenamerType::Only(master)).await?;

    backsync_and_verify_master_wc(fb, commit_sync_data.clone(), small_repo_dbs).await?;

    let ctx = CoreContext::test_mock(fb);
    let target_cs_id = resolve_cs_id(&ctx, commit_sync_data.get_target_repo(), "master").await?;

    let map =
        list_working_copy_utf8(&ctx, commit_sync_data.get_target_repo(), target_cs_id).await?;
    assert_eq!(
        map.into_iter().collect::<BTreeMap<_, _>>(),
        btreemap! {
                NonRootMPath::new("1")? => "1\n".to_string(),
                NonRootMPath::new("2")? => "2\n".to_string(),
                NonRootMPath::new("3")? => "3\n".to_string(),
                NonRootMPath::new("4")? => "4\n".to_string(),
                NonRootMPath::new("5")? => "5\n".to_string(),
                NonRootMPath::new("6")? => "6\n".to_string(),
                NonRootMPath::new("7")? => "7\n".to_string(),
                NonRootMPath::new("8")? => "8\n".to_string(),
                NonRootMPath::new("9")? => "9\n".to_string(),
                NonRootMPath::new("10")? => "modified10\n".to_string(),
                NonRootMPath::new("randomfile")? => "some other content".to_string(),
                NonRootMPath::new("files")? => "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n".to_string(),
        }
    );

    // Bookmark should be deleted
    assert_eq!(
        commit_sync_data
            .get_target_repo()
            .get_bookmark_hg(ctx, &BookmarkKey::new("anotherbookmark")?)
            .await?,
        None
    );

    Ok(())
}

#[mononoke::fbinit_test]
async fn backsync_linear_bookmark_renamer_remove_all(fb: FacebookInit) -> Result<(), Error> {
    let (commit_sync_data, small_repo_dbs) =
        init_repos(fb, MoverType::Noop, BookmarkRenamerType::RemoveAll).await?;

    backsync_and_verify_master_wc(fb, commit_sync_data.clone(), small_repo_dbs).await?;

    let ctx = CoreContext::test_mock(fb);
    // Bookmarks should be deleted
    assert_eq!(
        commit_sync_data
            .get_target_repo()
            .get_bookmark_hg(ctx.clone(), &BookmarkKey::new("master")?)
            .await?,
        None
    );

    assert_eq!(
        commit_sync_data
            .get_target_repo()
            .get_bookmark_hg(ctx, &BookmarkKey::new("anotherbookmark")?)
            .await?,
        None
    );

    Ok(())
}

#[mononoke::fbinit_test]
async fn backsync_two_small_repos(fb: FacebookInit) -> Result<(), Error> {
    let (small_repos, _large_repo, _latest_log_id, dont_verify_commits) =
        init_merged_repos(fb, 2).await?;

    let ctx = CoreContext::test_mock(fb);

    for (commit_sync_data, small_repo_dbs) in small_repos {
        let small_repo_id = commit_sync_data.get_target_repo().repo_identity().id();
        println!("backsyncing small repo#{}", small_repo_id.id());
        let small_repo_dbs = Arc::new(small_repo_dbs);
        let small_repo_id = commit_sync_data.get_target_repo().repo_identity().id();
        backsync_latest(
            ctx.clone(),
            commit_sync_data.clone(),
            small_repo_dbs.clone(),
            BacksyncLimit::NoLimit,
            Arc::new(AtomicBool::new(false)),
            CommitSyncContext::Backsyncer,
            false,
            Box::new(future::ready(())),
        )
        .map_err(Error::from)
        .await?
        .await;

        println!("verifying small repo#{}", small_repo_id.id());
        verify_mapping_and_all_wc(
            ctx.clone(),
            commit_sync_data.clone(),
            dont_verify_commits.clone(),
        )
        .await?;
    }

    Ok(())
}

#[mononoke::fbinit_test]
async fn backsync_merge_new_repo_all_files_removed(fb: FacebookInit) -> Result<(), Error> {
    // Remove all files from new repo except for the file in the merge commit itself
    let (commit_sync_data, small_repo_dbs) = init_repos(
        fb,
        MoverType::Except(vec![
            REPOMERGE_FOLDER.to_string(),
            REPOMERGE_FILE.to_string(),
        ]),
        BookmarkRenamerType::Noop,
    )
    .await?;

    backsync_and_verify_master_wc(fb, commit_sync_data.clone(), small_repo_dbs).await?;

    let ctx = CoreContext::test_mock(fb);
    let target_cs_id = resolve_cs_id(&ctx, commit_sync_data.get_target_repo(), "master").await?;

    let map =
        list_working_copy_utf8(&ctx, commit_sync_data.get_target_repo(), target_cs_id).await?;
    assert_eq!(
        map.into_iter().collect::<BTreeMap<_, _>>(),
        btreemap! {
                NonRootMPath::new("1")? => "1\n".to_string(),
                NonRootMPath::new("2")? => "2\n".to_string(),
                NonRootMPath::new("3")? => "3\n".to_string(),
                NonRootMPath::new("4")? => "4\n".to_string(),
                NonRootMPath::new("5")? => "5\n".to_string(),
                NonRootMPath::new("6")? => "6\n".to_string(),
                NonRootMPath::new("7")? => "7\n".to_string(),
                NonRootMPath::new("8")? => "8\n".to_string(),
                NonRootMPath::new("9")? => "9\n".to_string(),
                NonRootMPath::new("10")? => "modified10\n".to_string(),
                NonRootMPath::new("randomfile")? => "some other content".to_string(),
                NonRootMPath::new("files")? => "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n".to_string(),
        }
    );

    let target_cs_id =
        resolve_cs_id(&ctx, commit_sync_data.get_target_repo(), "anotherbookmark").await?;
    let map =
        list_working_copy_utf8(&ctx, commit_sync_data.get_target_repo(), target_cs_id).await?;
    assert_eq!(
        map.into_iter().collect::<BTreeMap<_, _>>(),
        btreemap! {
                NonRootMPath::new("1")? => "1\n".to_string(),
                NonRootMPath::new("2")? => "2\n".to_string(),
                NonRootMPath::new("3")? => "merged 3".to_string(),
                NonRootMPath::new("4")? => "4\n".to_string(),
                NonRootMPath::new("5")? => "5\n".to_string(),
                NonRootMPath::new("6")? => "6\n".to_string(),
                NonRootMPath::new("7")? => "7\n".to_string(),
                NonRootMPath::new("8")? => "8\n".to_string(),
                NonRootMPath::new("9")? => "9\n".to_string(),
                NonRootMPath::new("10")? => "modified10\n".to_string(),
                NonRootMPath::new("files")? => "branchmerge files content".to_string(),
                NonRootMPath::new("branchmerge")? => "new branch merge content".to_string(),
                NonRootMPath::new("randomfile")? => "some other content".to_string(),
        }
    );

    Ok(())
}

#[mononoke::fbinit_test]
async fn backsync_merge_new_repo_branch_removed(fb: FacebookInit) -> Result<(), Error> {
    // Remove all files from new repo except for the file in the merge commit itself
    let (commit_sync_data, small_repo_dbs) = init_repos(
        fb,
        MoverType::Except(vec![REPOMERGE_FOLDER.to_string()]),
        BookmarkRenamerType::Noop,
    )
    .await?;

    backsync_and_verify_master_wc(fb, commit_sync_data.clone(), small_repo_dbs).await?;

    let ctx = CoreContext::test_mock(fb);
    let target_cs_id = resolve_cs_id(&ctx, commit_sync_data.get_target_repo(), "master").await?;

    let map =
        list_working_copy_utf8(&ctx, commit_sync_data.get_target_repo(), target_cs_id).await?;
    assert_eq!(
        map.into_iter().collect::<BTreeMap<_, _>>(),
        btreemap! {
                NonRootMPath::new("1")? => "1\n".to_string(),
                NonRootMPath::new("2")? => "2\n".to_string(),
                NonRootMPath::new("3")? => "3\n".to_string(),
                NonRootMPath::new("4")? => "4\n".to_string(),
                NonRootMPath::new("5")? => "5\n".to_string(),
                NonRootMPath::new("6")? => "6\n".to_string(),
                NonRootMPath::new("7")? => "7\n".to_string(),
                NonRootMPath::new("8")? => "8\n".to_string(),
                NonRootMPath::new("9")? => "9\n".to_string(),
                NonRootMPath::new("10")? => "modified10\n".to_string(),
                NonRootMPath::new("randomfile")? => "some other content".to_string(),
                NonRootMPath::new("files")? => "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n".to_string(),
        }
    );

    let target_cs_id =
        resolve_cs_id(&ctx, commit_sync_data.get_target_repo(), "anotherbookmark").await?;
    let map =
        list_working_copy_utf8(&ctx, commit_sync_data.get_target_repo(), target_cs_id).await?;
    assert_eq!(
        map.into_iter().collect::<BTreeMap<_, _>>(),
        btreemap! {
                NonRootMPath::new("1")? => "1\n".to_string(),
                NonRootMPath::new("2")? => "2\n".to_string(),
                NonRootMPath::new("3")? => "merged 3".to_string(),
                NonRootMPath::new("4")? => "4\n".to_string(),
                NonRootMPath::new("5")? => "5\n".to_string(),
                NonRootMPath::new("6")? => "6\n".to_string(),
                NonRootMPath::new("7")? => "7\n".to_string(),
                NonRootMPath::new("8")? => "8\n".to_string(),
                NonRootMPath::new("9")? => "9\n".to_string(),
                NonRootMPath::new("10")? => "modified10\n".to_string(),
                NonRootMPath::new("files")? => "branchmerge files content".to_string(),
                NonRootMPath::new("branchmerge")? => "new branch merge content".to_string(),
                NonRootMPath::new("repomergefile")? => "some content".to_string(),
                NonRootMPath::new("randomfile")? => "some other content".to_string(),
        }
    );

    Ok(())
}

#[mononoke::fbinit_test]
async fn backsync_branch_merge_remove_branch_merge_file(fb: FacebookInit) -> Result<(), Error> {
    let (commit_sync_data, small_repo_dbs) = init_repos(
        fb,
        MoverType::Except(vec![BRANCHMERGE_FILE.to_string()]),
        BookmarkRenamerType::Noop,
    )
    .await?;

    backsync_and_verify_master_wc(fb, commit_sync_data.clone(), small_repo_dbs).await?;

    let ctx = CoreContext::test_mock(fb);
    let target_cs_id = resolve_cs_id(&ctx, commit_sync_data.get_target_repo(), "master").await?;

    let map =
        list_working_copy_utf8(&ctx, commit_sync_data.get_target_repo(), target_cs_id).await?;
    assert_eq!(
        map.into_iter().collect::<BTreeMap<_, _>>(),
        btreemap! {
                NonRootMPath::new("1")? => "1\n".to_string(),
                NonRootMPath::new("2")? => "2\n".to_string(),
                NonRootMPath::new("3")? => "3\n".to_string(),
                NonRootMPath::new("4")? => "4\n".to_string(),
                NonRootMPath::new("5")? => "5\n".to_string(),
                NonRootMPath::new("6")? => "6\n".to_string(),
                NonRootMPath::new("7")? => "7\n".to_string(),
                NonRootMPath::new("8")? => "8\n".to_string(),
                NonRootMPath::new("9")? => "9\n".to_string(),
                NonRootMPath::new("10")? => "modified10\n".to_string(),
                NonRootMPath::new("randomfile")? => "some other content".to_string(),
                NonRootMPath::new("files")? => "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n".to_string(),
        }
    );

    let target_cs_id =
        resolve_cs_id(&ctx, commit_sync_data.get_target_repo(), "anotherbookmark").await?;
    let map =
        list_working_copy_utf8(&ctx, commit_sync_data.get_target_repo(), target_cs_id).await?;
    assert_eq!(
        map.into_iter().collect::<BTreeMap<_, _>>(),
        btreemap! {
                NonRootMPath::new("1")? => "1\n".to_string(),
                NonRootMPath::new("2")? => "2\n".to_string(),
                NonRootMPath::new("3")? => "merged 3".to_string(),
                NonRootMPath::new("4")? => "4\n".to_string(),
                NonRootMPath::new("5")? => "5\n".to_string(),
                NonRootMPath::new("6")? => "6\n".to_string(),
                NonRootMPath::new("7")? => "7\n".to_string(),
                NonRootMPath::new("8")? => "8\n".to_string(),
                NonRootMPath::new("9")? => "9\n".to_string(),
                NonRootMPath::new("10")? => "modified10\n".to_string(),
                NonRootMPath::new("files")? => "branchmerge files content".to_string(),
                NonRootMPath::new("repomergefile")? => "some content".to_string(),
                NonRootMPath::new("randomfile")? => "some other content".to_string(),
                NonRootMPath::new("repomerge/first")? => "new repo content".to_string(),
                NonRootMPath::new("repomerge/movedest")? => "moved content".to_string(),
                NonRootMPath::new("repomerge/second")? => "new repo second content".to_string(),
                NonRootMPath::new("repomerge/toremove")? => "new repo content".to_string(),

        }
    );

    Ok(())
}

#[mononoke::fbinit_test]
async fn backsync_unrelated_branch(fb: FacebookInit) -> Result<(), Error> {
    let master = BookmarkKey::new("master")?;
    let (commit_sync_data, small_repo_dbs) = init_repos(
        fb,
        MoverType::Except(vec!["unrelated_branch".to_string()]),
        BookmarkRenamerType::Only(master),
    )
    .await?;
    let small_repo_dbs = Arc::new(small_repo_dbs);
    let large_repo = commit_sync_data.get_source_repo();

    let ctx = CoreContext::test_mock(fb);
    let merge = build_unrelated_branch(ctx.clone(), large_repo).await;

    move_bookmark(
        ctx.clone(),
        large_repo.clone(),
        &BookmarkKey::new("otherrepo/somebook")?,
        merge,
    )
    .await?;

    let fut = backsync_latest(
        ctx.clone(),
        commit_sync_data.clone(),
        small_repo_dbs.clone(),
        BacksyncLimit::NoLimit,
        Arc::new(AtomicBool::new(false)),
        CommitSyncContext::Backsyncer,
        false,
        Box::new(future::ready(())),
    )
    .await?;

    // Unrelated branch should be ignored until it's merged into already backsynced
    // branch
    let maybe_outcome = commit_sync_data
        .get_commit_sync_outcome(&ctx, merge)
        .await?;
    assert!(maybe_outcome.is_none());

    println!("merging into master");
    let new_master =
        CreateCommitContext::new(&ctx, &large_repo, vec!["master", "otherrepo/somebook"])
            .commit()
            .await?;

    move_bookmark(
        ctx.clone(),
        large_repo.clone(),
        &BookmarkKey::new("master")?,
        new_master,
    )
    .await?;

    backsync_latest(
        ctx.clone(),
        commit_sync_data.clone(),
        small_repo_dbs.clone(),
        BacksyncLimit::NoLimit,
        Arc::new(AtomicBool::new(false)),
        CommitSyncContext::Backsyncer,
        false,
        fut,
    )
    .await?
    .await;
    let maybe_outcome = commit_sync_data
        .get_commit_sync_outcome(&ctx, new_master)
        .await?;
    assert!(maybe_outcome.is_some());
    let maybe_outcome = commit_sync_data
        .get_commit_sync_outcome(&ctx, merge)
        .await?;
    assert!(maybe_outcome.is_some());

    Ok(())
}

#[mononoke::fbinit_test]
async fn backsync_change_mapping(fb: FacebookInit) -> Result<(), Error> {
    // Initialize large and small repos
    let ctx = CoreContext::test_mock(fb);
    let mut factory = TestRepoFactory::new(fb)?;
    let large_repo_id = RepositoryId::new(1);
    let large_repo: TestRepo = factory.with_id(large_repo_id).build().await?;
    let small_repo_id = RepositoryId::new(2);
    let small_repo: TestRepo = factory.with_id(small_repo_id).build().await?;

    // Create commit syncer with two version - current and new
    let small_repo_dbs = TargetRepoDbs {
        bookmarks: small_repo.bookmarks_arc(),
        bookmark_update_log: small_repo.bookmark_update_log_arc(),
        counters: small_repo.mutable_counters_arc(),
    };
    init_target_repo(&ctx, &small_repo_dbs, large_repo_id).await?;
    let small_repo_dbs = Arc::new(small_repo_dbs);

    let repos = CommitSyncRepos::new(
        small_repo.clone(),
        large_repo.clone(),
        CommitSyncDirection::Backwards,
        SubmoduleDeps::ForSync(HashMap::new()),
    );

    let current_version = CommitSyncConfigVersion("current_version".to_string());
    let new_version = CommitSyncConfigVersion("new_version".to_string());

    let bookmark_renamer_type = BookmarkRenamerType::Noop;

    let (lv_cfg, lv_cfg_src) = TestLiveCommitSyncConfig::new_with_source();

    let current_version_config = CommitSyncConfig {
        large_repo_id: large_repo.repo_identity().id(),
        common_pushrebase_bookmarks: vec![BookmarkKey::new("master")?],
        small_repos: hashmap! {
            small_repo.repo_identity().id() => SmallRepoCommitSyncConfig {
                default_action: DefaultSmallToLargeCommitSyncPathAction::PrependPrefix(
                    NonRootMPath::new("current_prefix").unwrap(),
                ),
                map: hashmap! { },
                submodule_config: Default::default(),
            },
        },
        version_name: current_version.clone(),
    };

    lv_cfg_src.add_config(current_version_config);

    let new_version_config = CommitSyncConfig {
        large_repo_id: large_repo.repo_identity().id(),
        common_pushrebase_bookmarks: vec![BookmarkKey::new("master")?],
        small_repos: hashmap! {
            small_repo.repo_identity().id() => SmallRepoCommitSyncConfig {
                default_action: DefaultSmallToLargeCommitSyncPathAction::PrependPrefix(
                    NonRootMPath::new("new_prefix").unwrap(),
                ),
                map: hashmap! { },
                submodule_config: Default::default(),
            },
        },
        version_name: new_version.clone(),
    };
    lv_cfg_src.add_config(new_version_config);

    let common = bookmark_renamer_type.get_common_repo_config(
        small_repo.repo_identity().id(),
        large_repo.repo_identity().id(),
    );
    lv_cfg_src.add_common_config(common);

    let live_commit_sync_config = Arc::new(lv_cfg);

    let commit_sync_data = CommitSyncData::new(&ctx, repos, live_commit_sync_config);

    // Rewrite root commit with current version
    let root_cs_id = CreateCommitContext::new_root(&ctx, &large_repo)
        .commit()
        .await?;

    unsafe_always_rewrite_sync_commit(
        &ctx,
        root_cs_id,
        &commit_sync_data,
        None,
        &current_version,
        CommitSyncContext::Tests,
    )
    .await?;

    // Add one more empty commit with old mapping
    let before_mapping_change = CreateCommitContext::new(&ctx, &large_repo, vec![root_cs_id])
        .commit()
        .await?;

    // Now create a commit with a special extra that changes the mapping
    // to new version while backsyncing
    let change_mapping_commit =
        CreateCommitContext::new(&ctx, &large_repo, vec![before_mapping_change])
            .add_extra(
                CHANGE_XREPO_MAPPING_EXTRA.to_string(),
                new_version.clone().0.into_bytes(),
            )
            .commit()
            .await?;

    let after_mapping_change_commit =
        CreateCommitContext::new(&ctx, &large_repo, vec![change_mapping_commit])
            .add_file("new_prefix/file", "content")
            .commit()
            .await?;

    bookmark(&ctx, &large_repo, "head")
        .set_to(after_mapping_change_commit)
        .await?;

    // Do the backsync, and check the version
    let jk = JustKnobsInMemory::new(hashmap! {
        "scm/mononoke:ignore_change_xrepo_mapping_extra".to_string() => KnobVal::Bool(false),
    });

    let f = backsync_latest(
        ctx.clone(),
        commit_sync_data.clone(),
        small_repo_dbs.clone(),
        BacksyncLimit::NoLimit,
        Arc::new(AtomicBool::new(false)),
        CommitSyncContext::Backsyncer,
        false,
        Box::new(future::ready(())),
    );
    with_just_knobs_async(jk, f.boxed()).await?.await;

    let commit_sync_outcome = commit_sync_data
        .get_commit_sync_outcome(&ctx, before_mapping_change)
        .await?
        .ok_or_else(|| anyhow!("unexpected missing commit sync outcome"))?;

    assert_matches!(commit_sync_outcome, CommitSyncOutcome::RewrittenAs(_, version) => {
        assert_eq!(current_version, version);
    });

    let commit_sync_outcome = commit_sync_data
        .get_commit_sync_outcome(&ctx, change_mapping_commit)
        .await?
        .ok_or_else(|| anyhow!("unexpected missing commit sync outcome"))?;

    assert_matches!(commit_sync_outcome, CommitSyncOutcome::RewrittenAs(_, version) => {
        assert_eq!(new_version, version);
    });

    let commit_sync_outcome = commit_sync_data
        .get_commit_sync_outcome(&ctx, after_mapping_change_commit)
        .await?
        .ok_or_else(|| anyhow!("unexpected missing commit sync outcome"))?;

    let target_cs_id = assert_matches!(commit_sync_outcome, CommitSyncOutcome::RewrittenAs(target_cs_id, version) => {
        assert_eq!(new_version, version);
        target_cs_id
    });

    let map =
        list_working_copy_utf8(&ctx, commit_sync_data.get_target_repo(), target_cs_id).await?;
    assert_eq!(
        map,
        hashmap! {NonRootMPath::new("file")? => "content".to_string()}
    );

    Ok(())
}

async fn build_unrelated_branch(ctx: CoreContext, large_repo: &TestRepo) -> ChangesetId {
    let p1 = new_commit(
        ctx.clone(),
        large_repo,
        vec![],
        btreemap! {"unrelated_branch" => Some("first content")},
    )
    .await;
    println!("p1: {:?}", p1);

    let p2 = new_commit(
        ctx.clone(),
        large_repo,
        vec![],
        btreemap! {"unrelated_branch" => Some("second content")},
    )
    .await;
    println!("p2: {:?}", p2);

    let merge = new_commit(
        ctx.clone(),
        large_repo,
        vec![p1, p2],
        btreemap! {"unrelated_branch" => Some("merge content")},
    )
    .await;
    println!("merge: {:?}", merge);

    merge
}

async fn new_commit<T: AsRef<str>>(
    ctx: CoreContext,
    repo: &TestRepo,
    parents: Vec<ChangesetId>,
    contents: BTreeMap<&str, Option<T>>,
) -> ChangesetId {
    create_commit(
        ctx.clone(),
        repo.clone(),
        parents,
        store_files(&ctx, contents, &repo).await,
    )
    .await
}

async fn backsync_and_verify_master_wc(
    fb: FacebookInit,
    commit_sync_data: CommitSyncData<TestRepo>,
    small_repo_dbs: TargetRepoDbs,
) -> Result<(), Error> {
    let large_repo = commit_sync_data.get_source_repo();

    let ctx = CoreContext::test_mock(fb);
    let next_log_entries: Vec<_> = commit_sync_data
        .get_source_repo()
        .bookmark_update_log()
        .read_next_bookmark_log_entries(
            ctx.clone(),
            BookmarkUpdateLogId(0),
            1000,
            Freshness::MaybeStale,
        )
        .try_collect()
        .await?;
    let small_repo_dbs = Arc::new(small_repo_dbs);
    let latest_log_id = next_log_entries.len() as i64;

    let mut futs = vec![];
    // Run syncs in parallel
    for _ in 1..5 {
        let f = mononoke::spawn_task(backsync_latest(
            ctx.clone(),
            commit_sync_data.clone(),
            small_repo_dbs.clone(),
            BacksyncLimit::NoLimit,
            Arc::new(AtomicBool::new(false)),
            CommitSyncContext::Backsyncer,
            false,
            Box::new(future::ready(())),
        ))
        .flatten_err();
        futs.push(f);
    }

    futures::future::join_all(futures::future::try_join_all(futs).await?).await;

    // Check that counter was moved
    let fetched_value = small_repo_dbs
        .counters
        .get_counter(&ctx, &format_counter(&large_repo.repo_identity().id()))
        .await?;
    assert_eq!(fetched_value, Some(latest_log_id));

    verify_mapping_and_all_wc(ctx.clone(), commit_sync_data, vec![]).await?;
    Ok(())
}

async fn verify_mapping_and_all_wc(
    ctx: CoreContext,
    commit_sync_data: CommitSyncData<TestRepo>,
    dont_verify_commits: Vec<ChangesetId>,
) -> Result<(), Error> {
    let large_repo = commit_sync_data.get_source_repo();
    let small_repo = commit_sync_data.get_target_repo();

    verify_bookmarks(ctx.clone(), commit_sync_data.clone()).await?;

    let heads: Vec<_> = large_repo
        .bookmarks()
        .get_heads_maybe_stale(ctx.clone())
        .try_collect()
        .await?;

    println!("checking all source commits");
    let all_source_commits = large_repo
        .commit_graph()
        .ancestors_difference(&ctx, heads, vec![])
        .await?;

    // Check that all commits were synced correctly
    for source_cs_id in all_source_commits {
        if dont_verify_commits.contains(&source_cs_id) {
            continue;
        }
        let csc = commit_sync_data.clone();
        let outcome = csc.get_commit_sync_outcome(&ctx, source_cs_id).await?;
        let source_bcs = source_cs_id.load(&ctx, large_repo.repo_blobstore()).await?;
        let outcome = outcome.unwrap_or_else(|| {
            panic!(
                "commit has not been synced {} {:?}",
                source_cs_id, source_bcs
            )
        });
        use CommitSyncOutcome::*;

        let (target_cs_id, movers_to_use) = match outcome {
            NotSyncCandidate(_) => {
                continue;
            }
            EquivalentWorkingCopyAncestor(target_cs_id, ref version)
            | RewrittenAs(target_cs_id, ref version) => {
                println!("using mover for {:?}", version);
                (
                    target_cs_id,
                    commit_sync_data.get_movers_by_version(version).await?,
                )
            }
        };

        // Empty commits should always be synced, except for merges
        let bcs = source_cs_id
            .load(&ctx, csc.get_source_repo().repo_blobstore())
            .await?;
        if bcs.file_changes().collect::<Vec<_>>().is_empty() && !bcs.is_merge() {
            match outcome {
                RewrittenAs(..) => {}
                _ => {
                    panic!("empty commit should always be remapped {:?}", outcome);
                }
            };
        }

        let source_hg_cs_id = large_repo.derive_hg_changeset(&ctx, source_cs_id).await?;
        let target_hg_cs_id = small_repo.derive_hg_changeset(&ctx, target_cs_id).await?;

        compare_contents(
            &ctx,
            source_hg_cs_id,
            target_hg_cs_id,
            commit_sync_data.clone(),
            movers_to_use.clone(),
        )
        .await?;
    }
    Ok(())
}

async fn verify_bookmarks(
    ctx: CoreContext,
    commit_sync_data: CommitSyncData<TestRepo>,
) -> Result<(), Error> {
    let large_repo = commit_sync_data.get_source_repo();
    let small_repo = commit_sync_data.get_target_repo();

    let bookmarks: Vec<_> = large_repo
        .get_publishing_bookmarks_maybe_stale_hg(ctx.clone())
        .try_collect()
        .await?;

    // Check that bookmark point to corresponding working copies
    for (bookmark, source_hg_cs_id) in bookmarks {
        println!("checking bookmark: {}", bookmark.key());
        match commit_sync_data.rename_bookmark(bookmark.key()).await? {
            Some(renamed_book) => {
                if &renamed_book != bookmark.key() {
                    assert!(
                        small_repo
                            .get_bookmark_hg(ctx.clone(), bookmark.key())
                            .await?
                            .is_none()
                    );
                }
                let target_hg_cs_id = small_repo
                    .get_bookmark_hg(ctx.clone(), &renamed_book)
                    .await?
                    .unwrap_or_else(|| {
                        panic!("{} bookmark doesn't exist in target repo!", bookmark.key())
                    });

                let source_bcs_id = large_repo
                    .bonsai_hg_mapping()
                    .get_bonsai_from_hg(&ctx, source_hg_cs_id)
                    .await?
                    .unwrap();

                let commit_sync_outcome = commit_sync_data
                    .get_commit_sync_outcome(&ctx, source_bcs_id)
                    .await?;
                let commit_sync_outcome = commit_sync_outcome.expect("unsynced commit");

                println!(
                    "verify_bookmarks. calling compare_contents: source_bcs_id: {}, outcome: {:?}",
                    source_bcs_id, commit_sync_outcome
                );

                use CommitSyncOutcome::*;
                let movers = match commit_sync_outcome {
                    NotSyncCandidate(_) => {
                        panic!("commit should not point to NotSyncCandidate");
                    }
                    EquivalentWorkingCopyAncestor(_, version) | RewrittenAs(_, version) => {
                        println!("using mover for {:?}", version);
                        commit_sync_data.get_movers_by_version(&version).await?
                    }
                };

                compare_contents(
                    &ctx,
                    source_hg_cs_id,
                    target_hg_cs_id,
                    commit_sync_data.clone(),
                    movers.clone(),
                )
                .await?;
            }
            None => {
                // Make sure we don't have this bookmark in target repo
                assert!(
                    small_repo
                        .get_bookmark_hg(ctx.clone(), bookmark.key())
                        .await?
                        .is_none()
                );
            }
        }
    }

    Ok(())
}

async fn compare_contents(
    ctx: &CoreContext,
    source_hg_cs_id: HgChangesetId,
    target_hg_cs_id: HgChangesetId,
    commit_sync_data: CommitSyncData<TestRepo>,
    // TODO(T182311609): stop taking Movers and call a commit syncer method to
    // move paths.
    movers: Movers,
) -> Result<(), Error> {
    let source_content =
        list_content(ctx, source_hg_cs_id, commit_sync_data.get_source_repo()).await?;
    let target_content =
        list_content(ctx, target_hg_cs_id, commit_sync_data.get_target_repo()).await?;

    println!(
        "source content: {:?}, target content {:?}",
        source_content, target_content
    );

    let mover = movers.mover;

    let filtered_source_content: HashMap<_, _> = source_content
        .into_iter()
        .filter_map(|(key, value)| {
            mover
                .move_path(&NonRootMPath::new(key).unwrap())
                .unwrap()
                .map(|key| (key, value))
        })
        .map(|(path, value)| (format!("{}", path), value))
        .collect();

    assert_eq!(target_content, filtered_source_content);

    Ok(())
}

async fn list_content(
    ctx: &CoreContext,
    hg_cs_id: HgChangesetId,
    repo: &TestRepo,
) -> Result<HashMap<String, String>, Error> {
    let cs = hg_cs_id.load(ctx, repo.repo_blobstore()).await?;

    let entries = cs
        .manifestid()
        .list_all_entries(ctx.clone(), repo.repo_blobstore().clone())
        .try_collect::<Vec<_>>()
        .await?;

    let mut actual = HashMap::new();
    for (path, entry) in entries {
        match entry {
            Entry::Leaf((_, filenode_id)) => {
                let blobstore = repo.repo_blobstore();
                let envelope = filenode_id.load(ctx, blobstore).await?;
                let content =
                    filestore::fetch_concat(blobstore, ctx, envelope.content_id()).await?;
                let s = String::from_utf8_lossy(content.as_ref()).into_owned();
                actual.insert(
                    format!("{}", Option::<NonRootMPath>::from(path).unwrap()),
                    s,
                );
            }
            Entry::Tree(_) => {}
        }
    }

    Ok(actual)
}

enum BookmarkRenamerType {
    CommonAndPrefix(BookmarkKey, String),
    Only(BookmarkKey),
    RemoveAll,
    Noop,
}

impl BookmarkRenamerType {
    fn get_common_repo_config(
        &self,
        small_repo_id: RepositoryId,
        large_repo_id: RepositoryId,
    ) -> CommonCommitSyncConfig {
        use BookmarkRenamerType::*;

        match self {
            CommonAndPrefix(common, bookmark_prefix) => CommonCommitSyncConfig {
                common_pushrebase_bookmarks: vec![common.clone()],
                small_repos: hashmap! {
                    small_repo_id => SmallRepoPermanentConfig {
                        bookmark_prefix: AsciiString::from_str(bookmark_prefix).unwrap(),
                        common_pushrebase_bookmarks_map: HashMap::new(),
                    }
                },
                large_repo_id,
            },
            Only(name) => CommonCommitSyncConfig {
                common_pushrebase_bookmarks: vec![name.clone()],
                small_repos: hashmap! {
                    small_repo_id => SmallRepoPermanentConfig {
                        bookmark_prefix: AsciiString::from_str("nonexistentprefix").unwrap(),
                        common_pushrebase_bookmarks_map: HashMap::new(),
                    }
                },
                large_repo_id,
            },
            RemoveAll => CommonCommitSyncConfig {
                common_pushrebase_bookmarks: vec![],
                small_repos: hashmap! {
                    small_repo_id => SmallRepoPermanentConfig {
                        bookmark_prefix: AsciiString::from_str("nonexistentprefix").unwrap(),
                        common_pushrebase_bookmarks_map: HashMap::new(),
                    }
                },
                large_repo_id,
            },
            Noop => CommonCommitSyncConfig {
                common_pushrebase_bookmarks: vec![],
                small_repos: hashmap! {
                    small_repo_id => SmallRepoPermanentConfig {
                        bookmark_prefix: AsciiString::new(),
                        common_pushrebase_bookmarks_map: HashMap::new(),
                    }
                },
                large_repo_id,
            },
        }
    }
}

enum MoverType {
    Noop,
    Except(Vec<String>),
    Only(String),
}

impl MoverType {
    fn get_small_repo_config(&self) -> SmallRepoCommitSyncConfig {
        use MoverType::*;

        match self {
            Noop => SmallRepoCommitSyncConfig {
                default_action: DefaultSmallToLargeCommitSyncPathAction::Preserve,
                map: hashmap! {},
                submodule_config: Default::default(),
            },
            Except(files) => {
                let mut map = hashmap! {};
                for file in files {
                    map.insert(
                        NonRootMPath::new(file).unwrap(),
                        NonRootMPath::new(format!("nonexistentpath{}", file)).unwrap(),
                    );
                }
                SmallRepoCommitSyncConfig {
                    default_action: DefaultSmallToLargeCommitSyncPathAction::Preserve,
                    map,
                    submodule_config: Default::default(),
                }
            }
            Only(path) => SmallRepoCommitSyncConfig {
                default_action: DefaultSmallToLargeCommitSyncPathAction::PrependPrefix(
                    NonRootMPath::new("nonexistentpath").unwrap(),
                ),
                map: hashmap! {
                    NonRootMPath::new(path).unwrap() => NonRootMPath::new(path).unwrap(),
                },
                submodule_config: Default::default(),
            },
        }
    }
}

async fn init_repos(
    fb: FacebookInit,
    mover_type: MoverType,
    bookmark_renamer_type: BookmarkRenamerType,
) -> Result<(CommitSyncData<TestRepo>, TargetRepoDbs), Error> {
    override_just_knobs(JustKnobsInMemory::new(hashmap! {
        "scm/mononoke:cross_repo_skip_backsyncing_ordinary_empty_commits".to_string() => KnobVal::Bool(false),
        "scm/mononoke:ignore_change_xrepo_mapping_extra".to_string() => KnobVal::Bool(false),
    }));
    let ctx = CoreContext::test_mock(fb);
    let mut factory = TestRepoFactory::new(fb)?;

    let (lv_cfg, lv_cfg_src) = TestLiveCommitSyncConfig::new_with_source();
    let live_commit_sync_config = Arc::new(lv_cfg);

    let large_repo_id = RepositoryId::new(1);
    let large_repo: TestRepo = factory
        .with_id(large_repo_id)
        .with_live_commit_sync_config(live_commit_sync_config.clone())
        .build()
        .await?;
    Linear::init_repo(fb, &large_repo).await?;

    let small_repo_id = RepositoryId::new(2);
    let small_repo: TestRepo = factory
        .with_id(small_repo_id)
        .with_live_commit_sync_config(live_commit_sync_config.clone())
        .build()
        .await?;

    let small_repo_dbs = TargetRepoDbs {
        bookmarks: small_repo.bookmarks_arc(),
        bookmark_update_log: small_repo.bookmark_update_log_arc(),
        counters: small_repo.mutable_counters_arc(),
    };
    init_target_repo(&ctx, &small_repo_dbs, large_repo_id).await?;

    let repos = CommitSyncRepos::new(
        small_repo.clone(),
        large_repo.clone(),
        CommitSyncDirection::Backwards,
        SubmoduleDeps::ForSync(HashMap::new()),
    );

    let empty: BTreeMap<_, Option<&str>> = BTreeMap::new();
    // Create fake empty commit in the target repo
    let initial_commit_in_target = create_commit(
        ctx.clone(),
        small_repo.clone(),
        vec![],
        store_files(&ctx, empty.clone(), &large_repo).await,
    )
    .await;

    let version = CommitSyncConfigVersion("TEST_VERSION_NAME".to_string());
    let version_config = CommitSyncConfig {
        large_repo_id: large_repo.repo_identity().id(),
        common_pushrebase_bookmarks: vec![BookmarkKey::new("master")?],
        small_repos: hashmap! {
            small_repo.repo_identity().id() => mover_type.get_small_repo_config(),
        },
        version_name: version.clone(),
    };

    lv_cfg_src.add_config(version_config);
    let common = bookmark_renamer_type.get_common_repo_config(
        small_repo.repo_identity().id(),
        large_repo.repo_identity().id(),
    );
    lv_cfg_src.add_common_config(common);

    let git_submodules_action = get_git_submodule_action_by_version(
        &ctx,
        live_commit_sync_config.clone(),
        &version,
        large_repo_id,
        small_repo_id,
    )
    .await?;

    let commit_sync_data = CommitSyncData::new(&ctx, repos, live_commit_sync_config.clone());

    // Sync first commit manually
    let initial_bcs_id = large_repo
        .bonsai_hg_mapping()
        .get_bonsai_from_hg(
            &ctx,
            HgChangesetId::from_str("2d7d4ba9ce0a6ffd222de7785b249ead9c51c536").unwrap(),
        )
        .await?
        .unwrap();
    let first_bcs = initial_bcs_id
        .load(&ctx, large_repo.repo_blobstore())
        .await?;

    // No submodules are expanded in backsyncing
    let submodule_content_ids = Vec::<(Arc<TestRepo>, HashSet<_>)>::new();
    upload_commits(
        &ctx,
        vec![first_bcs.clone()],
        &large_repo,
        &small_repo,
        submodule_content_ids,
    )
    .await?;
    let first_bcs_mut = first_bcs.into_mut();

    let rewrite_res = {
        let empty_map = HashMap::new();
        cloned!(ctx, large_repo);
        rewrite_commit(
            &ctx,
            first_bcs_mut,
            &empty_map,
            commit_sync_data.get_movers_by_version(&version).await?,
            &large_repo,
            Default::default(),
            git_submodules_action,
            None, // Submodule expansion data not needed here
        )
        .await
    }?;

    let rewritten_first_bcs_id = match rewrite_res.rewritten {
        Some(mut rewritten) => {
            rewritten.parents.push(initial_commit_in_target);

            let rewritten = rewritten.freeze()?;
            save_changesets(&ctx, &small_repo, vec![rewritten.clone()]).await?;
            rewritten.get_changeset_id()
        }
        None => initial_commit_in_target,
    };

    let first_entry = SyncedCommitMappingEntry::new(
        large_repo.repo_identity().id(),
        initial_bcs_id,
        small_repo.repo_identity().id(),
        rewritten_first_bcs_id,
        CommitSyncConfigVersion("TEST_VERSION_NAME".to_string()),
        commit_sync_data.get_source_repo_type(),
    );
    commit_sync_data
        .get_mapping()
        .add(&ctx, first_entry)
        .await?;

    // Create a few new commits on top of master

    let master = BookmarkKey::new("master")?;
    let master_val = large_repo
        .bookmarks()
        .get(ctx.clone(), &master, bookmarks::Freshness::MostRecent)
        .await?
        .unwrap();

    let empty_bcs_id = create_commit(
        ctx.clone(),
        large_repo.clone(),
        vec![master_val],
        store_files(&ctx, empty, &large_repo).await,
    )
    .await;

    let first_bcs_id = create_commit(
        ctx.clone(),
        large_repo.clone(),
        vec![empty_bcs_id],
        store_files(
            &ctx,
            btreemap! {"randomfile" => Some("some content")},
            &large_repo,
        )
        .await,
    )
    .await;

    let second_bcs_id = create_commit(
        ctx.clone(),
        large_repo.clone(),
        vec![first_bcs_id],
        store_files(
            &ctx,
            btreemap! {"randomfile" => Some("some other content")},
            &large_repo,
        )
        .await,
    )
    .await;

    move_bookmark(ctx.clone(), large_repo.clone(), &master, second_bcs_id).await?;

    // Create new bookmark
    let master = BookmarkKey::new("anotherbookmark")?;
    move_bookmark(ctx.clone(), large_repo.clone(), &master, first_bcs_id).await?;

    // Merge new repo into master
    let first_new_repo_file = format!("{}/first", REPOMERGE_FOLDER);
    let to_remove_new_repo_file = format!("{}/toremove", REPOMERGE_FOLDER);
    let move_dest_new_repo_file = format!("{}/movedest", REPOMERGE_FOLDER);
    let second_new_repo_file = format!("{}/second", REPOMERGE_FOLDER);

    let first_new_repo_commit = new_commit(
        ctx.clone(),
        &large_repo,
        vec![],
        btreemap! {
            first_new_repo_file.as_ref() => Some("new repo content"),
            to_remove_new_repo_file.as_ref() => Some("new repo content"),
        },
    )
    .await;

    let p2 = {
        let (path_rename, rename_file_change) = store_rename(
            &ctx,
            (
                NonRootMPath::new(to_remove_new_repo_file.clone())?,
                first_new_repo_commit,
            ),
            &move_dest_new_repo_file,
            "moved content",
            &large_repo,
        )
        .await;

        let mut stored_files = store_files(
            &ctx,
            btreemap! {
                second_new_repo_file.as_ref() => Some("new repo second content"),
            },
            &large_repo,
        )
        .await;
        stored_files.insert(path_rename, rename_file_change);

        create_commit(
            ctx.clone(),
            large_repo.clone(),
            vec![first_new_repo_commit],
            stored_files,
        )
        .await
    };

    let merge = new_commit(
        ctx.clone(),
        &large_repo,
        vec![second_bcs_id, p2],
        btreemap! {
             REPOMERGE_FILE => Some("some content"),
        },
    )
    .await;
    move_bookmark(ctx.clone(), large_repo.clone(), &master, merge).await?;

    // Create a branch merge - merge initial commit in the repo with the last
    let branch_merge_p1 = new_commit(
        ctx.clone(),
        &large_repo,
        vec![initial_bcs_id],
        btreemap! {
            "3" => Some("branchmerge 3 content"),
        },
    )
    .await;

    let branch_merge = new_commit(
        ctx.clone(),
        &large_repo,
        vec![branch_merge_p1, merge],
        btreemap! {
            BRANCHMERGE_FILE => Some("branch merge content"),
            // Both parents have different content in "files" and "3" - need to resolve it
            "files" => Some("branchmerge files content"),
            "3" => Some("merged 3"),
        },
    )
    .await;
    move_bookmark(ctx.clone(), large_repo.clone(), &master, branch_merge).await?;

    // Do a branch merge again, but this time only change content in BRANCHMERGE_FILE
    let branch_merge_second = new_commit(
        ctx.clone(),
        &large_repo,
        vec![branch_merge_p1, branch_merge],
        btreemap! {
            BRANCHMERGE_FILE => Some("new branch merge content"),
            // Both parents have different content in "files" and "3" - need to resolve it
            "files" => Some("branchmerge files content"),
            "3" => Some("merged 3"),
        },
    )
    .await;
    move_bookmark(
        ctx.clone(),
        large_repo.clone(),
        &master,
        branch_merge_second,
    )
    .await?;

    Ok((commit_sync_data, small_repo_dbs))
}

async fn init_target_repo(
    ctx: &CoreContext,
    small_repo_dbs: &TargetRepoDbs,
    large_repo_id: RepositoryId,
) -> Result<(), Error> {
    // Init counters
    small_repo_dbs
        .counters
        .set_counter(ctx, &format_counter(&large_repo_id), 0, None)
        .await?;

    Ok(())
}

async fn init_merged_repos(
    fb: FacebookInit,
    num_repos: usize,
) -> Result<
    (
        Vec<(CommitSyncData<TestRepo>, TargetRepoDbs)>,
        TestRepo,
        i64,
        Vec<ChangesetId>,
    ),
    Error,
> {
    let ctx = CoreContext::test_mock(fb);

    let mut factory = TestRepoFactory::new(fb)?;
    let large_repo_id = RepositoryId::new(num_repos as i32);
    let large_repo: TestRepo = factory.with_id(large_repo_id).build().await?;

    let mapping = SqlSyncedCommitMappingBuilder::with_sqlite_in_memory()?
        .build(RendezVousOptions::for_test());

    let mut output = vec![];
    let mut small_repos = vec![];
    let mut moved_cs_ids = vec![];
    // Create small repos and one large repo
    for idx in 0..num_repos {
        let repoid = RepositoryId::new(idx as i32);
        let small_repo: TestRepo = factory.with_id(repoid).build().await?;
        let small_repo_dbs = TargetRepoDbs {
            bookmarks: small_repo.bookmarks_arc(),
            bookmark_update_log: small_repo.bookmark_update_log_arc(),
            counters: small_repo.mutable_counters_arc(),
        };

        // Init counters
        small_repo_dbs
            .counters
            .set_counter(&ctx, &format_counter(&large_repo_id), 0, None)
            .await?;

        let after_merge_version = CommitSyncConfigVersion("TEST_VERSION_NAME".to_string());
        let noop_version = CommitSyncConfigVersion("noop".to_string());

        let (lv_cfg, lv_cfg_src) = TestLiveCommitSyncConfig::new_with_source();

        let new_version_config = CommitSyncConfig {
            large_repo_id: large_repo.repo_identity().id(),
            common_pushrebase_bookmarks: vec![BookmarkKey::new("master")?],
            small_repos: hashmap! {
                small_repo.repo_identity().id() => SmallRepoCommitSyncConfig {
                    default_action: DefaultSmallToLargeCommitSyncPathAction::PrependPrefix(
                        NonRootMPath::new(format!("smallrepo{}", small_repo.repo_identity().id().id())).unwrap(),
                    ),
                    map: hashmap! { },
                    submodule_config: Default::default(),
                },
            },
            version_name: after_merge_version.clone(),
        };

        lv_cfg_src.add_config(new_version_config);

        let mover_type = MoverType::Noop;
        let noop_version_config = CommitSyncConfig {
            large_repo_id: large_repo.repo_identity().id(),
            common_pushrebase_bookmarks: vec![BookmarkKey::new("master")?],
            small_repos: hashmap! {
                small_repo.repo_identity().id() => mover_type.get_small_repo_config(),
            },
            version_name: noop_version.clone(),
        };
        lv_cfg_src.add_config(noop_version_config);

        let bookmark_renamer_type = BookmarkRenamerType::CommonAndPrefix(
            BookmarkKey::new("master")?,
            format!("smallrepo{}", repoid.id()),
        );

        let common = bookmark_renamer_type.get_common_repo_config(
            small_repo.repo_identity().id(),
            large_repo.repo_identity().id(),
        );
        lv_cfg_src.add_common_config(common);

        let live_commit_sync_config = Arc::new(lv_cfg);

        let repos = CommitSyncRepos::new(
            small_repo.clone(),
            large_repo.clone(),
            CommitSyncDirection::Backwards,
            SubmoduleDeps::ForSync(HashMap::new()),
        );

        let commit_sync_data = CommitSyncData::new(&ctx, repos, live_commit_sync_config);
        output.push((commit_sync_data, small_repo_dbs));

        let filename = format!("file_in_smallrepo{}", small_repo.repo_identity().id().id());
        let small_repo_cs_id = create_commit(
            ctx.clone(),
            small_repo.clone(),
            vec![],
            store_files(
                &ctx,
                btreemap! { filename.as_str() => Some("some content")},
                &small_repo,
            )
            .await,
        )
        .await;
        println!("small repo cs id w/o parents: {}", small_repo_cs_id);

        small_repos.push((small_repo.clone(), small_repo_cs_id.clone()));

        let mut other_repo_ids = vec![];
        for i in 0..num_repos {
            if i != idx {
                other_repo_ids.push(RepositoryId::new(i as i32));
            }
        }

        preserve_premerge_commit(
            ctx.clone(),
            large_repo.clone(),
            small_repo.clone(),
            other_repo_ids.clone(),
            small_repo_cs_id,
            &mapping,
        )
        .await?;

        let renamed_filename = format!(
            "smallrepo{}/{}",
            small_repo.repo_identity().id().id(),
            filename
        );
        let (renamed_path, rename) = store_rename(
            &ctx,
            (NonRootMPath::new(&filename).unwrap(), small_repo_cs_id),
            renamed_filename.as_str(),
            "some content",
            &large_repo,
        )
        .await;

        let moved_cs_id = create_commit(
            ctx.clone(),
            large_repo.clone(),
            vec![small_repo_cs_id],
            btreemap! {
                renamed_path => rename,
            },
        )
        .await;
        println!("large repo moved cs id: {}", moved_cs_id);
        moved_cs_ids.push(moved_cs_id);
    }

    // Create merge commit
    let merge_cs_id = create_commit(
        ctx.clone(),
        large_repo.clone(),
        moved_cs_ids.clone(),
        btreemap! {},
    )
    .await;

    println!("large repo merge cs id: {}", merge_cs_id);
    // Create an empty commit on top of a merge commit and sync it to all small repos
    let empty: BTreeMap<_, Option<&str>> = BTreeMap::new();
    // Create empty commit in the large repo, and sync it to all small repos
    let first_after_merge_commit = create_commit(
        ctx.clone(),
        large_repo.clone(),
        vec![merge_cs_id],
        store_files(&ctx, empty.clone(), &large_repo).await,
    )
    .await;
    println!("large repo empty commit: {}", first_after_merge_commit);

    for (small_repo, latest_small_repo_cs_id) in &small_repos {
        let small_repo_first_after_merge = create_commit(
            ctx.clone(),
            small_repo.clone(),
            vec![*latest_small_repo_cs_id],
            store_files(&ctx, empty.clone(), &small_repo).await,
        )
        .await;

        println!(
            "empty commit in {}: {}",
            small_repo.repo_identity().id(),
            small_repo_first_after_merge
        );
        let entry = SyncedCommitMappingEntry::new(
            large_repo.repo_identity().id(),
            first_after_merge_commit,
            small_repo.repo_identity().id(),
            small_repo_first_after_merge,
            CommitSyncConfigVersion("TEST_VERSION_NAME".to_string()),
            SyncedCommitSourceRepo::Large,
        );
        mapping.add(&ctx, entry).await?;
    }

    // Create new commit in large repo
    let mut latest_log_id = 0;
    {
        let master = BookmarkKey::new("master")?;
        let mut prev_master = None;
        for repo_id in 0..num_repos {
            let filename = format!("smallrepo{}/newfile", repo_id);
            let new_commit = create_commit(
                ctx.clone(),
                large_repo.clone(),
                vec![first_after_merge_commit],
                store_files(
                    &ctx,
                    btreemap! { filename.as_str() => Some("new content")},
                    &large_repo,
                )
                .await,
            )
            .await;

            println!("new commit in large repo: {}", new_commit);
            latest_log_id += 1;
            move_bookmark(ctx.clone(), large_repo.clone(), &master, new_commit).await?;
            prev_master = Some(new_commit);
        }

        // Create bookmark on premerge commit from first repo
        let premerge_book = BookmarkKey::new("smallrepo0/premerge_book")?;
        latest_log_id += 1;
        move_bookmark(
            ctx.clone(),
            large_repo.clone(),
            &premerge_book,
            small_repos[0].1,
        )
        .await?;

        // Now on second repo and move it to rewritten changeset
        let premerge_book = BookmarkKey::new("smallrepo1/premerge_book")?;
        latest_log_id += 1;
        move_bookmark(
            ctx.clone(),
            large_repo.clone(),
            &premerge_book,
            small_repos[1].1,
        )
        .await?;

        latest_log_id += 1;
        move_bookmark(
            ctx.clone(),
            large_repo.clone(),
            &premerge_book,
            prev_master.unwrap(),
        )
        .await?;

        // New commit that touches files from two different small repos
        let filename1 = "smallrepo0/newfile";
        let filename2 = "smallrepo1/newfile";
        let new_commit = create_commit(
            ctx.clone(),
            large_repo.clone(),
            vec![first_after_merge_commit],
            store_files(
                &ctx,
                btreemap! {
                    filename1 => Some("new content1"),
                    filename2 => Some("new content2"),
                },
                &large_repo,
            )
            .await,
        )
        .await;
        println!("large_repo newcommit: {}", new_commit);

        latest_log_id += 1;
        move_bookmark(ctx.clone(), large_repo.clone(), &master, new_commit).await?;

        // Create a Preserved commit
        let premerge_book = BookmarkKey::new("smallrepo0/preserved_commit")?;
        let filename = "smallrepo1/newfile";
        let new_commit = create_commit(
            ctx.clone(),
            large_repo.clone(),
            vec![small_repos[0].1],
            store_files(
                &ctx,
                btreemap! {
                    filename => Some("preserved content"),
                },
                &large_repo,
            )
            .await,
        )
        .await;
        println!("smallrepo1 newcommit: {}", new_commit);

        latest_log_id += 1;
        move_bookmark(ctx.clone(), large_repo.clone(), &premerge_book, new_commit).await?;
    }

    let mut commits_to_skip_verification = vec![];
    commits_to_skip_verification.extend(moved_cs_ids);
    commits_to_skip_verification.push(merge_cs_id);

    Ok((
        output,
        large_repo,
        latest_log_id,
        commits_to_skip_verification,
    ))
}

async fn preserve_premerge_commit(
    ctx: CoreContext,
    large_repo: TestRepo,
    small_repo: TestRepo,
    another_small_repo_ids: Vec<RepositoryId>,
    bcs_id: ChangesetId,
    mapping: &SqlSyncedCommitMapping,
) -> Result<(), Error> {
    println!(
        "preserve_premerge_commit called. large_repo: {}; small_repo: {}, another_small_repo_ids: {:?}, bcs_id: {}",
        large_repo.repo_identity().id(),
        small_repo.repo_identity().id(),
        another_small_repo_ids,
        bcs_id
    );

    let version = CommitSyncConfigVersion("noop".to_string());
    // Doesn't matter what mover to use - we are going to preserve the commit anyway
    let small_to_large_sync_config = {
        let repos = CommitSyncRepos::new(
            small_repo.clone(),
            large_repo.clone(),
            CommitSyncDirection::Forward,
            SubmoduleDeps::ForSync(HashMap::new()),
        );

        let (lv_cfg, lv_cfg_src) = TestLiveCommitSyncConfig::new_with_source();

        let bookmark_renamer_type = BookmarkRenamerType::Noop;
        let mover_type = MoverType::Noop;

        let version_config = CommitSyncConfig {
            large_repo_id: large_repo.repo_identity().id(),
            common_pushrebase_bookmarks: vec![BookmarkKey::new("master")?],
            small_repos: hashmap! {
                small_repo.repo_identity().id() => mover_type.get_small_repo_config(),
            },
            version_name: version.clone(),
        };

        lv_cfg_src.add_config(version_config);
        let common = bookmark_renamer_type.get_common_repo_config(
            small_repo.repo_identity().id(),
            large_repo.repo_identity().id(),
        );
        lv_cfg_src.add_common_config(common);

        let live_commit_sync_config = Arc::new(lv_cfg);
        CommitSyncData::new(&ctx, repos, live_commit_sync_config)
    };

    unsafe_sync_commit(
        &ctx,
        bcs_id,
        &small_to_large_sync_config,
        CandidateSelectionHint::Only,
        CommitSyncContext::Tests,
        Some(CommitSyncConfigVersion("noop".to_string())),
        false,
    )
    .await?;

    for another_repo_id in another_small_repo_ids {
        mapping
            .insert_equivalent_working_copy(
                &ctx,
                EquivalentWorkingCopyEntry {
                    large_repo_id: large_repo.repo_identity().id(),
                    large_bcs_id: bcs_id,
                    small_repo_id: another_repo_id,
                    small_bcs_id: None,
                    version_name: Some(version.clone()),
                },
            )
            .await?;
    }
    Ok(())
}

async fn move_bookmark(
    ctx: CoreContext,
    repo: TestRepo,
    bookmark: &BookmarkKey,
    bcs_id: ChangesetId,
) -> Result<(), Error> {
    let mut txn = repo.bookmarks().create_transaction(ctx.clone());

    let prev_bcs_id = repo
        .bookmarks()
        .get(ctx, bookmark, bookmarks::Freshness::MostRecent)
        .await?;

    match prev_bcs_id {
        Some(prev_bcs_id) => {
            txn.update(
                bookmark,
                bcs_id,
                prev_bcs_id,
                BookmarkUpdateReason::TestMove,
            )?;
        }
        None => {
            txn.create(bookmark, bcs_id, BookmarkUpdateReason::TestMove)?;
        }
    }

    assert!(txn.commit().await?.is_some());
    Ok(())
}
