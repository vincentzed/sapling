/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {MessageBusStatus} from './MessageBus';
import type {
  ApplicationInfo,
  ChangedFile,
  CommitInfo,
  MergeConflicts,
  RepoInfo,
  SmartlogCommits,
  SubmodulesByRoot,
  SubscriptionKind,
  SubscriptionResultsData,
  UncommittedChanges,
  ValidatedRepoInfo,
} from './types';

import {DEFAULT_DAYS_OF_COMMITS_TO_LOAD} from 'isl-server/src/constants';
import {atom} from 'jotai';
import {reuseEqualObjects} from 'shared/deepEqualExt';
import {randomId} from 'shared/utils';
import {type BookmarksData, bookmarksDataStorage} from './BookmarksData';
import serverAPI from './ClientToServerAPI';
import {latestSuccessorsMapAtom, successionTracker} from './SuccessionTracker';
import {Dag, DagCommitInfo} from './dag/dag';
import {readInterestingAtoms, serializeAtomsState} from './debug/getInterestingAtoms';
import {atomFamilyWeak, configBackedAtom, readAtom, writeAtom} from './jotaiUtils';
import platform from './platform';
import {atomResetOnCwdChange, repositoryData} from './repositoryData';
import {registerCleanup, registerDisposable} from './utils';
import {Internal} from './Internal';

export {repositoryData};

registerDisposable(
  repositoryData,
  serverAPI.onMessageOfType('repoInfo', event => {
    writeAtom(repositoryData, {info: event.info, cwd: event.cwd});
  }),
  import.meta.hot,
);
registerCleanup(
  repositoryData,
  serverAPI.onSetup(() =>
    serverAPI.postMessage({
      type: 'requestRepoInfo',
    }),
  ),
  import.meta.hot,
);

export const repositoryInfoOrError = atom(
  get => {
    const data = get(repositoryData);
    return data?.info;
  },
  (
    get,
    set,
    update: RepoInfo | undefined | ((_prev: RepoInfo | undefined) => RepoInfo | undefined),
  ) => {
    const value = typeof update === 'function' ? update(get(repositoryData)?.info) : update;
    set(repositoryData, last => ({
      ...last,
      info: value,
    }));
  },
);

/** ValidatedRepoInfo, or undefined on error. */
export const repositoryInfo = atom(
  get => {
    const info = get(repositoryInfoOrError);
    if (info?.type === 'success') {
      return info;
    }
    return undefined;
  },
  (
    get,
    set,
    update:
      | ValidatedRepoInfo
      | undefined
      | ((_prev: ValidatedRepoInfo | undefined) => ValidatedRepoInfo | undefined),
  ) => {
    const value = typeof update === 'function' ? update(get(repositoryInfo)) : update;
    set(repositoryData, last => ({
      ...last,
      info: value,
    }));
  },
);

/** Main command name, like 'sl'. */
export const mainCommandName = atom(get => {
  const info = get(repositoryInfo);
  return info?.command ?? 'sl';
});

/** List of repo roots. Useful when cwd is in nested submodules. */
export const repoRoots = atom(get => {
  const info = get(repositoryInfo);
  return info?.repoRoots;
});

export const applicationinfo = atom<ApplicationInfo | undefined>(undefined);
registerDisposable(
  applicationinfo,
  serverAPI.onMessageOfType('applicationInfo', event => {
    writeAtom(applicationinfo, event.info);
  }),
  import.meta.hot,
);
registerCleanup(
  applicationinfo,
  serverAPI.onSetup(() =>
    serverAPI.postMessage({
      type: 'requestApplicationInfo',
    }),
  ),
  import.meta.hot,
);

export const reconnectingStatus = atom<MessageBusStatus>({type: 'initializing'});
registerDisposable(
  reconnectingStatus,
  platform.messageBus.onChangeStatus(status => {
    writeAtom(reconnectingStatus, status);
  }),
  import.meta.hot,
);

export async function forceFetchCommit(revset: string): Promise<CommitInfo> {
  serverAPI.postMessage({
    type: 'fetchLatestCommit',
    revset,
  });
  const response = await serverAPI.nextMessageMatching(
    'fetchedLatestCommit',
    message => message.revset === revset,
  );
  if (response.info.error) {
    throw response.info.error;
  }
  return response.info.value;
}

export const mostRecentSubscriptionIds: Record<SubscriptionKind, string> = {
  smartlogCommits: '',
  uncommittedChanges: '',
  mergeConflicts: '',
  submodules: '',
};

/**
 * Send a subscribeFoo message to the server on initialization,
 * and send an unsubscribe message on dispose.
 * Extract subscription response messages via a unique subscriptionID per effect call.
 */
function subscriptionEffect<K extends SubscriptionKind>(
  kind: K,
  onData: (data: SubscriptionResultsData[K]) => unknown,
): () => void {
  const subscriptionID = randomId();
  mostRecentSubscriptionIds[kind] = subscriptionID;
  const disposable = serverAPI.onMessageOfType('subscriptionResult', event => {
    if (event.subscriptionID !== subscriptionID || event.kind !== kind) {
      return;
    }
    onData(event.data as SubscriptionResultsData[K]);
  });

  const disposeSubscription = serverAPI.onSetup(() => {
    serverAPI.postMessage({
      type: 'subscribe',
      kind,
      subscriptionID,
    });

    return () =>
      serverAPI.postMessage({
        type: 'unsubscribe',
        kind,
        subscriptionID,
      });
  });

  return () => {
    disposable.dispose();
    disposeSubscription();
  };
}

export const latestUncommittedChangesData = atom<{
  fetchStartTimestamp: number;
  fetchCompletedTimestamp: number;
  files: UncommittedChanges;
  error?: Error;
}>({fetchStartTimestamp: 0, fetchCompletedTimestamp: 0, files: []});
// This is used by a test. Tests do not go through babel to rewrite source
// to insert debugLabel.
latestUncommittedChangesData.debugLabel = 'latestUncommittedChangesData';

registerCleanup(
  latestUncommittedChangesData,
  subscriptionEffect('uncommittedChanges', data => {
    writeAtom(latestUncommittedChangesData, last => ({
      ...data,
      files:
        data.files.value ??
        // leave existing files in place if there was no error
        (last.error == null ? [] : last.files) ??
        [],
      error: data.files.error,
    }));
  }),
  import.meta.hot,
);

/**
 * Latest fetched uncommitted file changes from the server, without any previews.
 * Prefer using `uncommittedChangesWithPreviews`, since it includes optimistic state
 * and previews.
 */
export const latestUncommittedChanges = atom<Array<ChangedFile>>(
  get => get(latestUncommittedChangesData).files,
);

export const uncommittedChangesFetchError = atom(get => {
  return get(latestUncommittedChangesData).error;
});

export const mergeConflicts = atom<MergeConflicts | undefined>(undefined);
registerCleanup(
  mergeConflicts,
  subscriptionEffect('mergeConflicts', data => {
    writeAtom(mergeConflicts, data);
  }),
);

export const inMergeConflicts = atom(get => get(mergeConflicts) != undefined);

export const latestCommitsData = atom<{
  fetchStartTimestamp: number;
  fetchCompletedTimestamp: number;
  commits: SmartlogCommits;
  error?: Error;
}>({fetchStartTimestamp: 0, fetchCompletedTimestamp: 0, commits: []});

registerCleanup(
  latestCommitsData,
  subscriptionEffect('smartlogCommits', data => {
    const previousDag = readAtom(latestDag);
    writeAtom(latestCommitsData, last => {
      let commits = last.commits;
      const newCommits = data.commits.value;
      if (newCommits != null) {
        // leave existing commits in place if there was no error
        commits = reuseEqualObjects(commits, newCommits, c => c.hash);
      }
      return {
        ...data,
        commits,
        error: data.commits.error,
      };
    });
    if (data.commits.value) {
      successionTracker.findNewSuccessionsFromCommits(previousDag, data.commits.value);
    }
  }),
);

export const latestUncommittedChangesTimestamp = atom(get => {
  return get(latestUncommittedChangesData).fetchCompletedTimestamp;
});

/**
 * Lookup a commit by hash, *WITHOUT PREVIEWS*.
 * Generally, you'd want to look up WITH previews, which you can use dagWithPreviews for.
 */
export const commitByHash = atomFamilyWeak((hash: string) => atom(get => get(latestDag).get(hash)));

export const latestCommits = atom(get => {
  return get(latestCommitsData).commits;
});

/** The dag also includes a mutationDag to answer successor queries. */
export const latestDag = atom(get => {
  const commits = get(latestCommits);
  const successorMap = get(latestSuccessorsMapAtom);
  const bookmarksData = get(bookmarksDataStorage);
  const commitDag = undefined; // will be populated from `commits`
  const dag = Dag.fromDag(commitDag, successorMap)
    .add(
      commits.map(c => {
        return DagCommitInfo.fromCommitInfo(filterBookmarks(bookmarksData, c));
      }),
    )
    .maybeForceConnectPublic();
  return dag;
});

function filterBookmarks(bookmarksData: BookmarksData, commit: CommitInfo): CommitInfo {
  if (commit.phase !== 'public') {
    return commit;
  }

  const hiddenBookmarks = new Set(bookmarksData.hiddenRemoteBookmarks);
  const recommendedBookmarks = new Set(Internal.getRecommendedBookmarks?.());

  // Filter by recommended bookmarks or hidden remote bookmarks
  const bookmarkFilter = (b: string) =>
    bookmarksData.useRecommendedBookmark ? recommendedBookmarks.has(b) : !hiddenBookmarks.has(b);

  return {
    ...commit,
    remoteBookmarks: commit.remoteBookmarks.filter(bookmarkFilter),
    bookmarks: commit.bookmarks.filter(bookmarkFilter),
    stableCommitMetadata: commit.stableCommitMetadata?.filter(b => !hiddenBookmarks.has(b.value)),
  };
}

export const commitFetchError = atom(get => {
  return get(latestCommitsData).error;
});

export const authorString = configBackedAtom<string | null>(
  'ui.username',
  null,
  true /* read-only */,
  true /* use raw value */,
);

export const isFetchingCommits = atom(false);
registerDisposable(
  isFetchingCommits,
  serverAPI.onMessageOfType('subscriptionResult', () => {
    writeAtom(isFetchingCommits, false); // new commits OR error means the fetch is not running anymore
  }),
  import.meta.hot,
);
registerDisposable(
  isFetchingCommits,
  serverAPI.onMessageOfType('beganFetchingSmartlogCommitsEvent', () => {
    writeAtom(isFetchingCommits, true);
  }),
  import.meta.hot,
);

export const isFetchingAdditionalCommits = atom(false);
registerDisposable(
  isFetchingAdditionalCommits,
  serverAPI.onMessageOfType('subscriptionResult', e => {
    if (e.kind === 'smartlogCommits') {
      writeAtom(isFetchingAdditionalCommits, false);
    }
  }),
  import.meta.hot,
);
registerDisposable(
  isFetchingAdditionalCommits,
  serverAPI.onMessageOfType('subscriptionResult', e => {
    if (e.kind === 'smartlogCommits') {
      writeAtom(isFetchingAdditionalCommits, false);
    }
  }),
  import.meta.hot,
);
registerDisposable(
  isFetchingAdditionalCommits,
  serverAPI.onMessageOfType('beganLoadingMoreCommits', () => {
    writeAtom(isFetchingAdditionalCommits, true);
  }),
  import.meta.hot,
);

export const isFetchingUncommittedChanges = atom(false);
registerDisposable(
  isFetchingUncommittedChanges,
  serverAPI.onMessageOfType('subscriptionResult', e => {
    if (e.kind === 'uncommittedChanges') {
      writeAtom(isFetchingUncommittedChanges, false); // new files OR error means the fetch is not running anymore
    }
  }),
  import.meta.hot,
);
registerDisposable(
  isFetchingUncommittedChanges,
  serverAPI.onMessageOfType('beganFetchingUncommittedChangesEvent', () => {
    writeAtom(isFetchingUncommittedChanges, true);
  }),
  import.meta.hot,
);

export const commitsShownRange = atomResetOnCwdChange<number | undefined>(
  DEFAULT_DAYS_OF_COMMITS_TO_LOAD,
);
registerDisposable(
  applicationinfo,
  serverAPI.onMessageOfType('commitsShownRange', event => {
    writeAtom(commitsShownRange, event.rangeInDays);
  }),
  import.meta.hot,
);

/**
 * Latest head commit from original data from the server, without any previews.
 * Prefer using `dagWithPreviews.resolve('.')`, since it includes optimistic state
 * and previews.
 */
export const latestHeadCommit = atom(get => {
  const commits = get(latestCommits);
  return commits.find(commit => commit.isDot);
});

/**
 * No longer in the "loading" state:
 * - Either the list of commits has successfully loaded
 * - or there was an error during the fetch
 */
export const haveCommitsLoadedYet = atom(get => {
  const data = get(latestCommitsData);
  return data.commits.length > 0 || data.error != null;
});

export const haveRemotePath = atom(get => {
  const info = get(repositoryInfo);
  // codeReviewSystem.type is 'unknown' or other values if paths.default is present.
  return info?.type === 'success' && info.codeReviewSystem.type !== 'none';
});

registerDisposable(
  serverAPI,
  serverAPI.onMessageOfType('getUiState', () => {
    const state = readInterestingAtoms();
    window.clientToServerAPI?.postMessage({
      type: 'gotUiState',
      state: JSON.stringify(serializeAtomsState(state), undefined, 2),
    });
  }),
  import.meta.hot,
);

export const submodulesByRoot = atom<SubmodulesByRoot>(new Map());

registerCleanup(
  submodulesByRoot,
  subscriptionEffect('submodules', fetchedSubmoduleMap => {
    writeAtom(submodulesByRoot, _prev_data => {
      // TODO: In the future we may add more granular client-server API
      // to update submodules. For now we just replace the whole map when the active repo updates.
      return fetchedSubmoduleMap;
    });
  }),
  import.meta.hot,
);
